//! Explicit command-template saves with bounded, serialized CAS operations.
use crate::daemon_connection::DesktopConnectionConfig;
use cshell_application::{
    QuickCommandChange, QuickCommandSnapshot, apply_quick_command_change, decode_quick_commands,
    encode_quick_commands,
};
use cshell_domain::QuickCommandCatalog;
use cshell_ipc::{
    Envelope, Handshake, QuickCommandOperation, QuickCommandRequest, QuickCommandResponse,
    QuickCommandStatus, client_handshake, envelope, features, read_envelope, transport,
    write_envelope,
};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

#[derive(Clone, Debug, Default)]
pub(crate) struct QuickCommandView {
    pub(crate) snapshot: Option<Arc<QuickCommandSnapshot>>,
    pub(crate) writable: bool,
    pub(crate) busy: bool,
    pub(crate) status: String,
}
enum Operation {
    Reload,
    Change(QuickCommandChange),
}
pub(crate) struct QuickCommandConnection {
    shared: Arc<Mutex<QuickCommandView>>,
    sender: Option<tokio::sync::mpsc::Sender<Operation>>,
    worker: Option<std::thread::JoinHandle<()>>,
}
impl QuickCommandConnection {
    pub(crate) fn start(config: DesktopConnectionConfig) -> std::io::Result<Self> {
        let shared = Arc::new(Mutex::new(QuickCommandView {
            busy: true,
            ..Default::default()
        }));
        let worker_shared = Arc::clone(&shared);
        let (sender, mut receiver) = tokio::sync::mpsc::channel(16);
        let worker = std::thread::Builder::new()
            .name("cshell-quick-commands".into())
            .spawn(move || {
                let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                else {
                    worker_shared
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .status = "无法启动快捷命令存储连接".into();
                    return;
                };
                runtime.block_on(async move {
                    let mut operation = Operation::Reload;
                    loop {
                        let current = {
                            let mut view = worker_shared
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner);
                            view.busy = true;
                            view.snapshot.clone()
                        };
                        let result = match operation {
                            Operation::Reload => send(&config, QuickCommandRequest::default())
                                .await
                                .and_then(decode_response),
                            Operation::Change(change) => {
                                let writable = worker_shared
                                    .lock()
                                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                                    .writable;
                                if !writable {
                                    Err("存储写入已暂停，请重新加载".into())
                                } else {
                                    let revision =
                                        current.as_ref().map_or(0, |snapshot| snapshot.revision);
                                    let empty = QuickCommandCatalog::default();
                                    let catalog = current
                                        .as_ref()
                                        .map_or(&empty, |snapshot| &snapshot.catalog);
                                    match apply_quick_command_change(catalog, change) {
                                        Ok(next) => save(&config, revision, &next).await,
                                        Err(_) => {
                                            Err("快捷命令格式或大小无效，请修正后重新加载".into())
                                        }
                                    }
                                }
                            }
                        };
                        {
                            let mut view = worker_shared
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner);
                            view.busy = false;
                            match result {
                                Ok(snapshot) => {
                                    view.snapshot = snapshot.map(Arc::new);
                                    view.writable = true;
                                    view.status = "快捷命令存储已同步".into();
                                }
                                Err(error) => {
                                    view.writable = false;
                                    view.status =
                                        format!("{error}；重新加载后再保存，未自动重试写入");
                                }
                            }
                        }
                        let Some(next) = receiver.recv().await else {
                            break;
                        };
                        operation = next;
                    }
                });
            })?;
        Ok(Self {
            shared,
            sender: Some(sender),
            worker: Some(worker),
        })
    }
    pub(crate) fn view(&self) -> QuickCommandView {
        self.shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
    pub(crate) fn reload(&self) -> bool {
        self.enqueue(Operation::Reload)
    }
    pub(crate) fn change(&self, change: QuickCommandChange) -> bool {
        self.enqueue(Operation::Change(change))
    }
    fn enqueue(&self, operation: Operation) -> bool {
        let mut view = self
            .shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if view.busy || matches!(operation, Operation::Change(_)) && !view.writable {
            return false;
        }
        if self
            .sender
            .as_ref()
            .is_some_and(|sender| sender.try_send(operation).is_ok())
        {
            view.busy = true;
            true
        } else {
            false
        }
    }
}
impl Drop for QuickCommandConnection {
    fn drop(&mut self) {
        self.sender.take();
        if let Some(worker) = self.worker.take() {
            let _joined = worker.join();
        }
    }
}
fn decode_response(response: QuickCommandResponse) -> Result<Option<QuickCommandSnapshot>, String> {
    match QuickCommandStatus::try_from(response.status) {
        Ok(QuickCommandStatus::Ok) => {}
        Ok(QuickCommandStatus::Conflict) => return Err("快捷命令已被其他窗口修改".into()),
        Ok(QuickCommandStatus::Unsupported) => {
            return Err("当前 daemon 不支持快捷命令，请使用新版 daemon".into());
        }
        Ok(QuickCommandStatus::Corrupt) => return Err("快捷命令存储内容损坏".into()),
        _ => return Err("快捷命令请求被拒绝或存储不可用".into()),
    }
    if response.document_json.is_empty() && response.revision == 0 {
        return Ok(None);
    }
    if response.document_json.is_empty() || response.revision == 0 {
        return Err("快捷命令回复缺少正文或版本".into());
    }
    Ok(Some(QuickCommandSnapshot {
        revision: response.revision,
        catalog: decode_quick_commands(&response.document_json)
            .map_err(|_| "快捷命令回复格式无效")?,
    }))
}
async fn save(
    config: &DesktopConnectionConfig,
    revision: u64,
    catalog: &QuickCommandCatalog,
) -> Result<Option<QuickCommandSnapshot>, String> {
    let document_json = encode_quick_commands(catalog).map_err(|_| "快捷命令格式无效")?;
    let response = send(
        config,
        QuickCommandRequest {
            operation: QuickCommandOperation::Save as i32,
            expected_revision: revision,
            document_json,
        },
    )
    .await?;
    let snapshot = decode_response(response)?;
    if snapshot.as_ref().is_none_or(|saved| {
        Some(saved.revision) != revision.checked_add(1) || &saved.catalog != catalog
    }) {
        return Err("保存回复与提交的快捷命令或版本不一致，结果不明".into());
    }
    Ok(snapshot)
}
async fn send(
    config: &DesktopConnectionConfig,
    request: QuickCommandRequest,
) -> Result<QuickCommandResponse, String> {
    tokio::time::timeout(Duration::from_secs(5), async {
        let resolved = config.resolve().map_err(|_| "无法发现 daemon")?;
        let mut stream = transport::connect(&resolved.endpoint)
            .await
            .map_err(|_| "无法连接 daemon")?;
        let mut handshake = Handshake::new(
            resolved.daemon_instance_id.to_vec(),
            resolved.instance_token.to_vec(),
        );
        let advanced = request.operation == QuickCommandOperation::Save as i32
            && decode_quick_commands(&request.document_json)
                .is_ok_and(|catalog| catalog.version == 2);
        handshake.feature_bits = features::QUICK_COMMAND_CONTROL | features::QUICK_TASK_TEMPLATES;
        let negotiated = client_handshake(&mut stream, 1, handshake)
            .await
            .map_err(|_| "daemon 身份或协议检查失败")?;
        if negotiated.feature_bits & features::QUICK_COMMAND_CONTROL == 0 {
            return Err("当前 daemon 不支持快捷命令".into());
        }
        if advanced && negotiated.feature_bits & features::QUICK_TASK_TEMPLATES == 0 {
            return Err("当前 daemon 不支持任务快捷模板，请更新 daemon".into());
        }
        write_envelope(
            &mut stream,
            &Envelope {
                request_id: 2,
                payload: Some(envelope::Payload::QuickCommandRequest(request)),
                ..Default::default()
            },
        )
        .await
        .map_err(|_| "快捷命令发送失败，保存结果可能不明")?;
        let response = read_envelope(&mut stream)
            .await
            .map_err(|_| "快捷命令回复中断，保存结果可能不明")?;
        if response.request_id != 2 {
            return Err("快捷命令回复编号不匹配".into());
        }
        match response.payload {
            Some(envelope::Payload::QuickCommandResponse(response)) => Ok(response),
            _ => Err("快捷命令回复类型不匹配".into()),
        }
    })
    .await
    .map_err(|_| "快捷命令请求超时，保存结果可能不明".to_owned())?
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon_connection::ResolvedDesktopEndpoint;
    use cshell_domain::{
        ConnectionId, QuickCommandAction, QuickCommandId, QuickCommandRecord, QuickCommandTarget,
    };
    use cshell_ipc::{DiscoveryRecord, HandshakePolicy, RuntimePaths, server_handshake};
    use std::{
        collections::BTreeSet,
        sync::atomic::{AtomicUsize, Ordering},
        time::Instant,
    };
    async fn settled(connection: &QuickCommandConnection) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while connection.view().busy {
            assert!(
                Instant::now() < deadline,
                "quick command worker did not settle"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn lost_save_reply_and_conflict_block_writes_until_explicit_authenticated_reload()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let paths = RuntimePaths::prepare(directory.path().join("runtime"))?;
        let discovery = DiscoveryRecord::generate(&paths);
        #[cfg(windows)]
        let endpoint = format!(r"\\.\pipe\cshell-command-fixture-{}", ConnectionId::new());
        #[cfg(unix)]
        let endpoint = directory.path().join("commands.sock");
        let listener = transport::LocalListener::bind(&endpoint)?;
        let config = DesktopConnectionConfig::for_fixture(ResolvedDesktopEndpoint {
            endpoint,
            instance_token: discovery.instance_token,
            daemon_instance_id: discovery.daemon_instance_id,
        });
        let policy = HandshakePolicy::with_instance_id(
            discovery.instance_token,
            discovery.daemon_instance_id,
            features::QUICK_COMMAND_CONTROL,
        );
        let writes = Arc::new(AtomicUsize::new(0));
        let server_writes = Arc::clone(&writes);
        let server = tokio::spawn(async move {
            let mut catalog = QuickCommandCatalog::default();
            let mut revision = 0;
            for step in 0..6 {
                let mut stream = listener.accept().await?;
                server_handshake(&mut stream, &policy).await?;
                let request = read_envelope(&mut stream).await?;
                let Some(envelope::Payload::QuickCommandRequest(command)) = request.payload else {
                    return Err("unexpected request".into());
                };
                let mut response = QuickCommandResponse::default();
                if command.operation == QuickCommandOperation::Save as i32 {
                    server_writes.fetch_add(1, Ordering::AcqRel);
                    if step == 4 {
                        response.status = QuickCommandStatus::Conflict as i32;
                    } else {
                        assert_eq!(command.expected_revision, revision);
                        catalog = decode_quick_commands(&command.document_json)?;
                        revision += 1;
                    }
                    if step == 1 {
                        continue;
                    } // Commit happened; the reply is deliberately lost.
                }
                response.revision = revision;
                if revision > 0 {
                    response.document_json = encode_quick_commands(&catalog)?;
                }
                write_envelope(
                    &mut stream,
                    &Envelope {
                        request_id: request.request_id,
                        payload: Some(envelope::Payload::QuickCommandResponse(response)),
                        ..Default::default()
                    },
                )
                .await?;
            }
            Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
        });
        let connection = QuickCommandConnection::start(config)?;
        settled(&connection).await;
        assert!(connection.view().writable);
        let command = QuickCommandRecord {
            id: QuickCommandId::new(),
            name: "health".into(),
            folder_id: None,
            tags: BTreeSet::new(),
            systems: BTreeSet::new(),
            action: QuickCommandAction::SendText {
                text: "PRIVATE_SAVED_BODY".into(),
                append_enter: false,
                target: QuickCommandTarget::Current,
            },
        };
        assert!(connection.change(QuickCommandChange::Upsert(command.clone())));
        settled(&connection).await;
        assert!(!connection.view().writable);
        assert!(!connection.view().status.contains("PRIVATE_SAVED_BODY"));
        assert_eq!(writes.load(Ordering::Acquire), 1);
        assert!(!connection.change(QuickCommandChange::Remove(command.id)));
        assert!(connection.reload());
        settled(&connection).await;
        assert_eq!(
            connection
                .view()
                .snapshot
                .as_ref()
                .map(|snapshot| snapshot.revision),
            Some(1)
        );
        assert!(connection.change(QuickCommandChange::Remove(command.id)));
        settled(&connection).await;
        assert_eq!(
            connection
                .view()
                .snapshot
                .as_ref()
                .map(|snapshot| snapshot.revision),
            Some(2)
        );
        assert!(connection.change(QuickCommandChange::Upsert(command.clone())));
        settled(&connection).await;
        assert!(!connection.view().writable);
        assert!(!connection.change(QuickCommandChange::Remove(command.id)));
        assert_eq!(writes.load(Ordering::Acquire), 3);
        assert!(connection.reload());
        settled(&connection).await;
        assert!(connection.view().writable);
        assert_eq!(
            connection
                .view()
                .snapshot
                .as_ref()
                .map(|snapshot| snapshot.revision),
            Some(2)
        );
        drop(connection);
        server.await?.map_err(|error| error.to_string())?;
        Ok(())
    }
}
