//! Bounded host inventory operations; query/freeze never opens a connection.
use crate::daemon_connection::DesktopConnectionConfig;
use cshell_application::*;
use cshell_domain::HostSelection;
use cshell_ipc::{
    Envelope, Handshake, HostGroupOperation, HostGroupRequest, HostGroupResponse, HostGroupStatus,
    client_handshake, envelope, features, read_envelope, transport, write_envelope,
};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

#[derive(Clone, Debug, Default)]
pub(crate) struct HostGroupView {
    pub(crate) snapshot: Option<Arc<HostManagementSnapshot>>,
    pub(crate) preview: Option<Arc<HostSelectionPreview>>,
    pub(crate) frozen: Option<Arc<FrozenHostTargets>>,
    pub(crate) writable: bool,
    pub(crate) busy: bool,
    pub(crate) generation: u64,
    pub(crate) status: String,
}
pub(crate) type HostRevision = (u64, u64);
enum Operation {
    Reload,
    Change(HostRevision, HostCatalogChange),
    Query(HostSelection, bool),
}
pub(crate) struct HostGroupConnection {
    shared: Arc<Mutex<HostGroupView>>,
    sender: Option<tokio::sync::mpsc::Sender<Operation>>,
    worker: Option<std::thread::JoinHandle<()>>,
}
#[derive(Debug)]
struct RequestError {
    message: String,
    conflict: bool,
}
impl RequestError {
    fn local(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            conflict: false,
        }
    }
}
impl HostGroupConnection {
    #[cfg(test)]
    pub(crate) fn for_panel_fixture(view: HostGroupView) -> Self {
        Self {
            shared: Arc::new(Mutex::new(view)),
            sender: None,
            worker: None,
        }
    }
    pub(crate) fn start(config: DesktopConnectionConfig) -> std::io::Result<Self> {
        let shared = Arc::new(Mutex::new(HostGroupView {
            busy: true,
            ..Default::default()
        }));
        let worker_shared = Arc::clone(&shared);
        let (sender, mut receiver) = tokio::sync::mpsc::channel(16);
        let worker = std::thread::Builder::new()
            .name("cshell-host-groups".into())
            .spawn(move || {
                let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                else {
                    let mut view = worker_shared
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    view.busy = false;
                    view.status = "无法启动主机存储连接".into();
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
                            view.preview = None;
                            view.frozen = None;
                            view.snapshot.clone()
                        };
                        let mutation =
                            matches!(operation, Operation::Change(..) | Operation::Reload);
                        let result = perform(&config, operation, current.as_deref()).await;
                        {
                            let mut view = worker_shared
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner);
                            view.busy = false;
                            view.generation = view.generation.wrapping_add(1);
                            match result {
                                Ok(Outcome::Catalog(snapshot)) => {
                                    view.snapshot = Some(Arc::new(snapshot));
                                    view.writable = true;
                                    view.status = "主机与分组存储已同步".into();
                                }
                                Ok(Outcome::Preview(preview)) => {
                                    view.status = format!(
                                        "已预览 {} 个目标；等待固定确认",
                                        preview.hosts.len()
                                    );
                                    view.preview = Some(Arc::new(preview));
                                }
                                Ok(Outcome::Frozen(frozen)) => {
                                    view.status = format!(
                                        "已固定 {} 个目标，快照 {}",
                                        frozen.targets().len(),
                                        frozen.id()
                                    );
                                    view.frozen = Some(Arc::new(frozen));
                                }
                                Err(error) => {
                                    if mutation || error.conflict {
                                        view.writable = false;
                                    }
                                    view.status = if mutation || error.conflict {
                                        format!("{}；需重新加载，未自动重放写入", error.message)
                                    } else {
                                        error.message
                                    };
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
    pub(crate) fn view(&self) -> HostGroupView {
        self.shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
    pub(crate) fn reload(&self) -> bool {
        self.enqueue(Operation::Reload)
    }
    pub(crate) fn change(&self, revision: HostRevision, change: HostCatalogChange) -> bool {
        self.enqueue(Operation::Change(revision, change))
    }
    pub(crate) fn query(&self, selection: HostSelection, freeze: bool) -> bool {
        self.enqueue(Operation::Query(selection, freeze))
    }
    fn enqueue(&self, operation: Operation) -> bool {
        let mut view = self
            .shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if view.busy {
            return false;
        }
        if !matches!(operation, Operation::Reload) && !view.writable {
            return false;
        }
        if let Operation::Change(revision, _) = &operation
            && view
                .snapshot
                .as_ref()
                .is_none_or(|snapshot| *revision != (snapshot.revision, snapshot.profile_revision))
        {
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
impl Drop for HostGroupConnection {
    fn drop(&mut self) {
        self.sender.take();
        if let Some(worker) = self.worker.take() {
            let _joined = worker.join();
        }
    }
}
enum Outcome {
    Catalog(HostManagementSnapshot),
    Preview(HostSelectionPreview),
    Frozen(FrozenHostTargets),
}
fn validate_response(response: HostGroupResponse) -> Result<HostGroupResponse, RequestError> {
    match HostGroupStatus::try_from(response.status) {
        Ok(HostGroupStatus::Ok) => Ok(response),
        Ok(HostGroupStatus::Conflict) => Err(RequestError {
            message: "主机或 SSH Profile 已被修改".into(),
            conflict: true,
        }),
        Ok(HostGroupStatus::Unsupported) => Err(RequestError::local(
            "当前 daemon 不支持主机组，请使用新版 daemon",
        )),
        Ok(HostGroupStatus::Invalid | HostGroupStatus::Limit | HostGroupStatus::MissingProfile) => {
            Err(RequestError::local(
                if response.detail.len() <= 256 && !response.detail.chars().any(char::is_control) {
                    response.detail.clone()
                } else {
                    "主机请求无效".into()
                },
            ))
        }
        Ok(HostGroupStatus::Corrupt) => Err(RequestError::local("主机存储损坏")),
        _ => Err(RequestError::local("主机存储或请求不可用")),
    }
}
fn decode_catalog(response: HostGroupResponse) -> Result<HostManagementSnapshot, RequestError> {
    let response = validate_response(response)?;
    let catalog = decode_host_management(&response.document_json)
        .map_err(|error| RequestError::local(error.to_string()))?;
    if response.revision != catalog.revision
        || response.profile_revision != catalog.profile_revision
    {
        return Err(RequestError::local("主机回复版本不匹配"));
    }
    Ok(catalog)
}
async fn perform(
    config: &DesktopConnectionConfig,
    operation: Operation,
    current: Option<&HostManagementSnapshot>,
) -> Result<Outcome, RequestError> {
    if matches!(operation, Operation::Reload) {
        return send(config, HostGroupRequest::default())
            .await
            .and_then(decode_catalog)
            .map(Outcome::Catalog);
    }
    let current = current.ok_or_else(|| RequestError::local("请先加载主机库"))?;
    match operation {
        Operation::Change(revision, change) => {
            if revision != (current.revision, current.profile_revision) {
                return Err(RequestError {
                    message: "编辑所依据的版本已变更".into(),
                    conflict: true,
                });
            }
            let catalog = apply_host_change(&current.catalog, change)
                .map_err(|error| RequestError::local(error.to_string()))?;
            let request = HostGroupRequest {
                operation: HostGroupOperation::Save as i32,
                expected_revision: current.revision,
                expected_profile_revision: current.profile_revision,
                catalog_json: encode_hosts(&catalog)
                    .map_err(|error| RequestError::local(error.to_string()))?,
                selection_json: Vec::new(),
                target_limit: 0,
            };
            let saved = decode_catalog(send(config, request).await?)?;
            if Some(saved.revision) != current.revision.checked_add(1)
                || saved.profile_revision != current.profile_revision
                || saved.catalog != catalog
            {
                return Err(RequestError::local(
                    "保存回复与提交的主机库或版本不一致，结果不明",
                ));
            }
            Ok(Outcome::Catalog(saved))
        }
        Operation::Query(selection, freeze) => {
            let expected = select_hosts(&current.catalog, &selection, MAX_HOST_TARGETS)
                .map_err(|error| RequestError::local(error.to_string()))?;
            let request = HostGroupRequest {
                operation: if freeze {
                    HostGroupOperation::Freeze
                } else {
                    HostGroupOperation::Preview
                } as i32,
                expected_revision: current.revision,
                expected_profile_revision: current.profile_revision,
                catalog_json: Vec::new(),
                selection_json: encode_host_selection(&selection)
                    .map_err(|error| RequestError::local(error.to_string()))?,
                target_limit: MAX_HOST_TARGETS as u32,
            };
            let response = validate_response(send(config, request).await?)?;
            if response.revision != current.revision
                || response.profile_revision != current.profile_revision
            {
                return Err(RequestError {
                    message: "查询回复版本不匹配".into(),
                    conflict: true,
                });
            }
            if freeze {
                let targets = decode_frozen_hosts(&response.document_json)
                    .map_err(|error| RequestError::local(error.to_string()))?;
                if targets.host_revision() != current.revision
                    || targets.profile_revision() != current.profile_revision
                    || targets.selection() != &selection
                    || targets.targets() != expected
                {
                    return Err(RequestError::local("固定目标回复与预览不一致"));
                }
                Ok(Outcome::Frozen(targets))
            } else {
                let preview = decode_host_preview(&response.document_json)
                    .map_err(|error| RequestError::local(error.to_string()))?;
                if preview.host_revision != current.revision
                    || preview.profile_revision != current.profile_revision
                    || preview.selection != selection
                    || preview.hosts != expected
                {
                    return Err(RequestError::local("目标预览回复不一致"));
                }
                Ok(Outcome::Preview(preview))
            }
        }
        Operation::Reload => Err(RequestError::local("invalid host operation")),
    }
}
async fn send(
    config: &DesktopConnectionConfig,
    request: HostGroupRequest,
) -> Result<HostGroupResponse, RequestError> {
    tokio::time::timeout(Duration::from_secs(5), async {
        let resolved = config
            .resolve()
            .map_err(|_| RequestError::local("无法发现 daemon"))?;
        let mut stream = transport::connect(&resolved.endpoint)
            .await
            .map_err(|_| RequestError::local("无法连接 daemon"))?;
        let mut handshake = Handshake::new(
            resolved.daemon_instance_id.to_vec(),
            resolved.instance_token.to_vec(),
        );
        handshake.feature_bits = features::HOST_GROUP_CONTROL;
        let negotiated = client_handshake(&mut stream, 1, handshake)
            .await
            .map_err(|_| RequestError::local("daemon 身份或协议验证失败"))?;
        if negotiated.feature_bits & features::HOST_GROUP_CONTROL == 0 {
            return Err(RequestError::local("当前 daemon 不支持主机组"));
        }
        write_envelope(
            &mut stream,
            &Envelope {
                request_id: 2,
                payload: Some(envelope::Payload::HostGroupRequest(request)),
                ..Default::default()
            },
        )
        .await
        .map_err(|_| RequestError::local("主机请求发送失败，保存结果可能不明"))?;
        let response = read_envelope(&mut stream)
            .await
            .map_err(|_| RequestError::local("主机回复中断，保存结果可能不明"))?;
        if response.request_id != 2 {
            return Err(RequestError::local("主机回复编号不匹配"));
        }
        match response.payload {
            Some(envelope::Payload::HostGroupResponse(response)) => Ok(response),
            _ => Err(RequestError::local("主机回复类型不匹配")),
        }
    })
    .await
    .map_err(|_| RequestError::local("主机请求超时，保存结果可能不明"))?
}

#[cfg(test)]
mod tests;
