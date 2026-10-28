use crate::daemon_connection::DesktopConnectionConfig;
use cshell_application::*;
use cshell_domain::ProfileId;
use cshell_ipc::{
    Envelope, Handshake, ProfileBatchOperation as Op, ProfileBatchRequest as Request,
    ProfileBatchStatus as Status, client_handshake, envelope, features, read_envelope, transport,
    write_envelope,
};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
#[derive(Clone, Default)]
pub(super) struct View {
    pub authority: Option<u64>,
    pub preview: Option<Arc<ProfileBatchPreview>>,
    pub preview_epoch: u64,
    pub settings: Option<Arc<ProfileSettingsReport>>,
    pub settings_epoch: u64,
    pub applied: Option<u64>,
    pub busy: bool,
    pub writable: bool,
    pub generation: u64,
    pub error: Option<String>,
}
struct Command {
    request: Request,
    epoch: u64,
    snapshot: Option<Arc<CatalogSnapshot>>,
    preview: Option<Arc<ProfileBatchPreview>>,
    explicit: bool,
}
pub(super) struct Connection {
    view: Arc<Mutex<View>>,
    sender: Option<tokio::sync::mpsc::Sender<Command>>,
    stop: tokio::sync::watch::Sender<bool>,
    worker: Option<std::thread::JoinHandle<()>>,
}
impl Connection {
    pub fn start(config: DesktopConnectionConfig) -> Result<Self, std::io::Error> {
        let view = Arc::new(Mutex::new(View::default()));
        let shared = view.clone();
        let (sender, mut receiver) = tokio::sync::mpsc::channel::<Command>(2);
        let (stop, mut stopped) = tokio::sync::watch::channel(false);
        let worker=std::thread::Builder::new().name("cshell-profile-batch-ipc".into()).spawn(move||{
            let runtime=match tokio::runtime::Builder::new_current_thread().enable_all().build(){Ok(r)=>r,Err(_)=>{shared.lock().unwrap_or_else(std::sync::PoisonError::into_inner).error=Some("批量编辑线程无法启动".into());return;}};
            runtime.block_on(async{loop{
                let command=tokio::select!{biased;_=stopped.changed()=>break,c=receiver.recv()=>match c{Some(c)=>c,None=>break}};
                let applying=command.request.operation==Op::Apply as i32;
                let result=tokio::select!{biased;_=stopped.changed()=>break,r=query(&config,&command)=>r};
                let mut view=shared.lock().unwrap_or_else(std::sync::PoisonError::into_inner);view.busy=false;view.generation=view.generation.wrapping_add(1);
                match result{
                    Ok(ProfileBatchReply::Authority(a))=>{view.authority=Some(a.revision);view.preview=None;view.settings=None;view.error=None;if command.explicit{view.writable=true;}},
                    Ok(ProfileBatchReply::Preview(p))=>{view.preview=Some(Arc::new(p));view.preview_epoch=command.epoch;view.error=None;},
                    Ok(ProfileBatchReply::Settings(s))=>{view.settings=Some(Arc::new(s));view.settings_epoch=command.epoch;view.error=None;},
                    Ok(ProfileBatchReply::Applied(a))=>{view.applied=Some(a.revision);view.authority=Some(a.revision);view.preview=None;view.error=None;},
                    Err(error)=>{view.error=Some(error);if applying{view.writable=false;view.preview=None;}else if command.request.operation==Op::Preview as i32{view.preview=None;}else if command.request.operation==Op::Inspect as i32{view.settings=None;}}
                }
            }});
        })?;
        Ok(Self {
            view,
            sender: Some(sender),
            stop,
            worker: Some(worker),
        })
    }
    pub fn view(&self) -> View {
        self.view
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
    pub fn request(
        &self,
        request: Request,
        epoch: u64,
        snapshot: Option<Arc<CatalogSnapshot>>,
        explicit: bool,
    ) -> bool {
        let mut view = self
            .view
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if view.busy {
            return false;
        }
        if matches!(Op::try_from(request.operation), Ok(Op::Preview | Op::Apply))
            && (!view.writable || view.authority != Some(request.expected_revision))
        {
            return false;
        }
        if request.operation == Op::Apply as i32
            && view.preview.as_ref().is_none_or(|p| {
                p.token.as_slice() != request.preview_token.as_slice()
                    || p.plan.revision != request.expected_revision
                    || p.plan
                        .fingerprint()
                        .ok()
                        .is_none_or(|sha| sha.as_slice() != request.review_sha256.as_slice())
            })
        {
            return false;
        }
        let command = Command {
            request,
            epoch,
            snapshot,
            preview: view.preview.clone(),
            explicit,
        };
        let accepted = self
            .sender
            .as_ref()
            .is_some_and(|s| s.try_send(command).is_ok());
        if accepted {
            view.busy = true;
        }
        accepted
    }
}
impl Drop for Connection {
    fn drop(&mut self) {
        let _ = self.stop.send(true);
        self.sender.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
async fn query(
    config: &DesktopConnectionConfig,
    command: &Command,
) -> Result<ProfileBatchReply, String> {
    let resolved = config.resolve().map_err(|_| "daemon 发现失败".to_owned())?;
    tokio::time::timeout(Duration::from_secs(40), async {
        #[cfg(windows)]
        let mut stream = transport::connect(&resolved.endpoint)
            .await
            .map_err(|_| "daemon 连接失败".to_owned())?;
        #[cfg(unix)]
        let mut stream = transport::connect(std::path::Path::new(&resolved.endpoint))
            .await
            .map_err(|_| "daemon 连接失败".to_owned())?;
        let mut hello = Handshake::new(
            resolved.daemon_instance_id.to_vec(),
            resolved.instance_token.to_vec(),
        );
        hello.feature_bits = features::PROFILE_BATCH;
        let negotiated = client_handshake(&mut stream, 1, hello)
            .await
            .map_err(|_| "daemon 认证失败".to_owned())?;
        if negotiated.feature_bits & features::PROFILE_BATCH == 0 {
            return Err("daemon 不支持批量编辑 / 继承检查".into());
        }
        write_envelope(
            &mut stream,
            &Envelope {
                request_id: 2,
                payload: Some(envelope::Payload::ProfileBatchRequest(
                    command.request.clone(),
                )),
                ..Default::default()
            },
        )
        .await
        .map_err(|_| "批量发送结果不明，请重新读取；勿重放提交".to_owned())?;
        let reply = read_envelope(&mut stream)
            .await
            .map_err(|_| "批量回复结果不明，请重新读取；勿重放提交".to_owned())?;
        if reply.request_id != 2 {
            return Err("批量回复身份不匹配".into());
        }
        let Some(envelope::Payload::ProfileBatchResponse(reply)) = reply.payload else {
            return Err("批量回复类型不匹配".into());
        };
        if reply.detail.len() > 4096 {
            return Err("批量错误信息超限".into());
        }
        if reply.status != Status::Ok as i32 {
            return Err(reply.detail);
        }
        let request = &command.request;
        let draft = if request.operation == Op::Preview as i32 {
            Some(decode_profile_batch(&request.document_json).map_err(str::to_owned)?)
        } else {
            None
        };
        let expect = match Op::try_from(request.operation).map_err(|_| "无效批量操作".to_owned())?
        {
            Op::Reload => ProfileBatchExpectation::Authority,
            Op::Preview => ProfileBatchExpectation::Preview {
                draft: draft.as_ref().ok_or("缺少批量范围")?,
                snapshot: command.snapshot.as_deref(),
            },
            Op::Apply => ProfileBatchExpectation::Applied(
                command.preview.as_deref().ok_or("缺少已审核预览")?,
            ),
            Op::Inspect => ProfileBatchExpectation::Settings {
                profile: ProfileId::from_bytes(
                    request
                        .profile_id
                        .as_slice()
                        .try_into()
                        .map_err(|_| "无效 Profile 范围")?,
                ),
                revision: (request.expected_revision != 0).then_some(request.expected_revision),
            },
        };
        let reply = decode_profile_batch_reply(&reply.document_json, expect)?;
        if let (ProfileBatchReply::Settings(report), Some(snapshot)) = (&reply, &command.snapshot) {
            let catalog = ProfileCatalog::from_snapshot(snapshot.as_ref().clone())
                .map_err(|e| e.to_string())?;
            if report.revision != snapshot.revision
                || snapshot.profiles.iter().find(|p| p.id == report.profile.id)
                    != Some(&report.profile)
                || catalog
                    .resolve(report.profile.id)
                    .map_err(|e| e.to_string())?
                    != report.settings
            {
                return Err("继承检查与当前目录不匹配".into());
            }
        }
        Ok(reply)
    })
    .await
    .map_err(|_| "批量操作超时，请读取目录并准备新的预览；勿重放提交".to_owned())?
}
