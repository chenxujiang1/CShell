use crate::daemon_connection::DesktopConnectionConfig;
use cshell_application::*;
use cshell_domain::HostAuthorityId;
use cshell_ipc::{
    CaOperation, CaRequest, CaStatus, Envelope, Handshake, client_handshake, envelope, features,
    read_envelope, transport, write_envelope,
};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
#[derive(Clone, Default)]
pub(super) struct View {
    pub catalog: Option<Arc<HostCaCatalog>>,
    pub preview: Option<Arc<HostCaPreview>>,
    pub certificate: Option<Arc<HostCertificateReport>>,
    pub writable: bool,
    pub busy: bool,
    pub generation: u64,
    pub epoch: u64,
    pub error: Option<String>,
}
pub(super) struct Connection {
    view: Arc<Mutex<View>>,
    sender: Option<tokio::sync::mpsc::Sender<Command>>,
    shutdown: tokio::sync::watch::Sender<bool>,
    worker: Option<std::thread::JoinHandle<()>>,
}
struct Command {
    request: CaRequest,
    explicit: bool,
    epoch: u64,
    review: Option<Arc<HostCaPreview>>,
}
impl std::fmt::Debug for Connection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostCaConnection").finish_non_exhaustive()
    }
}
impl Connection {
    pub fn start(config: DesktopConnectionConfig) -> Result<Self, std::io::Error> {
        let view = Arc::new(Mutex::new(View::default()));
        let shared = view.clone();
        let (sender, mut receiver) = tokio::sync::mpsc::channel::<Command>(4);
        let (shutdown, mut stop) = tokio::sync::watch::channel(false);
        let worker=std::thread::Builder::new().name("cshell-host-ca-ipc".into()).spawn(move||{
            let runtime=match tokio::runtime::Builder::new_current_thread().enable_all().build(){Ok(r)=>r,Err(_)=>{shared.lock().unwrap_or_else(std::sync::PoisonError::into_inner).error=Some("CA 后台线程无法启动".into());return;}};
            runtime.block_on(async{loop{
                let command=tokio::select!{_ = stop.changed()=>break,c=receiver.recv()=>match c{Some(c)=>c,None=>break}};
                let apply=matches!(CaOperation::try_from(command.request.operation),Ok(CaOperation::ApplySave|CaOperation::ApplyRemove));
                let result=tokio::select!{_ = stop.changed()=>break,r=send(&config,&command)=>r};
                let mut view=shared.lock().unwrap_or_else(std::sync::PoisonError::into_inner);view.busy=false;view.generation=view.generation.wrapping_add(1);
                match result{
                    Ok(HostCaReply::Catalog(c)) if view.catalog.as_ref().is_none_or(|old|c.revision>=old.revision)=>{
                        if apply||view.catalog.as_ref().is_none_or(|old|old.revision!=c.revision){view.preview=None;}
                        view.catalog=Some(Arc::new(c));view.error=None;
                        if command.explicit && command.request.operation==0&&command.epoch==view.epoch{view.writable=true;}
                    }
                    Ok(HostCaReply::Preview(p))=>{view.preview=Some(Arc::new(p));view.error=None;}
                    Ok(HostCaReply::Certificate(c))=>{view.certificate=Some(Arc::new(c));view.error=None;}
                    other=>{view.error=Some(other.err().unwrap_or_else(||"CA 版本回退，请重新载入".into()));if apply{view.writable=false;view.epoch=view.epoch.wrapping_add(1);view.preview=None;}else if command.request.operation!=CaOperation::InspectCertificate as i32{view.preview=None;}}
                }
            }});
        })?;
        Ok(Self {
            view,
            sender: Some(sender),
            shutdown,
            worker: Some(worker),
        })
    }
    pub fn view(&self) -> View {
        self.view
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
    pub fn request(&self, request: CaRequest, explicit: bool) -> bool {
        let mut view = self
            .view
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if view.busy {
            return false;
        }
        if !matches!(
            CaOperation::try_from(request.operation),
            Ok(CaOperation::Get | CaOperation::InspectCertificate)
        ) && (!view.writable
            || view
                .catalog
                .as_ref()
                .is_none_or(|c| c.revision != request.expected_revision))
        {
            return false;
        }
        let apply = matches!(
            CaOperation::try_from(request.operation),
            Ok(CaOperation::ApplySave | CaOperation::ApplyRemove)
        );
        if apply
            && view.preview.as_ref().is_none_or(|p| {
                p.token.as_slice() != request.preview_token.as_slice()
                    || p.id.as_uuid().as_bytes().as_slice() != request.authority_id.as_slice()
                    || p.catalog_revision != request.expected_revision
                    || p.fingerprint != request.confirmed_fingerprint
            })
        {
            return false;
        }
        let command = Command {
            request,
            explicit,
            epoch: view.epoch,
            review: view.preview.clone(),
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
        let _ = self.shutdown.send(true);
        self.sender.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
async fn send(config: &DesktopConnectionConfig, command: &Command) -> Result<HostCaReply, String> {
    let request = &command.request;
    let resolved = config.resolve().map_err(|e| e.to_string())?;
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
        hello.feature_bits = features::HOST_CA_CONTROL;
        let negotiated = client_handshake(&mut stream, 1, hello)
            .await
            .map_err(|_| "daemon 认证失败".to_owned())?;
        if negotiated.feature_bits & features::HOST_CA_CONTROL == 0 {
            return Err("daemon 不支持 CA 管理".into());
        }
        write_envelope(
            &mut stream,
            &Envelope {
                request_id: 2,
                payload: Some(envelope::Payload::CaRequest(request.clone())),
                ..Default::default()
            },
        )
        .await
        .map_err(|_| "CA 发送结果不明；重新载入并新建预览，不重放提交".to_owned())?;
        let response = read_envelope(&mut stream)
            .await
            .map_err(|_| "CA 回复结果不明；重新载入，不重放提交".to_owned())?;
        if response.request_id != 2 {
            return Err("CA 回复身份不匹配".into());
        }
        let Some(envelope::Payload::CaResponse(response)) = response.payload else {
            return Err("CA 回复类型不匹配".into());
        };
        if response.detail.len() > 1024 {
            return Err("CA 回复超限".into());
        }
        if response.status != CaStatus::Ok as i32 {
            return Err(response.detail);
        }
        let reply = decode_host_ca_reply(&response.document_json).map_err(str::to_owned)?;
        let id = if request.authority_id.is_empty() {
            None
        } else {
            Some(HostAuthorityId::from_bytes(
                request
                    .authority_id
                    .as_slice()
                    .try_into()
                    .map_err(|_| "无效 CA 范围".to_owned())?,
            ))
        };
        match CaOperation::try_from(request.operation).map_err(|_| "无效 CA 操作".to_owned())? {
            CaOperation::Get => reply
                .confirm(HostCaReplyExpectation::Catalog)
                .map_err(str::to_owned)?,
            CaOperation::PreviewSave => {
                let draft: HostCaDraft = serde_json::from_slice(&request.document_json)
                    .map_err(|_| "无效 CA 草稿".to_owned())?;
                reply
                    .confirm(HostCaReplyExpectation::PreviewSave(
                        id.ok_or("缺少 CA 范围")?,
                        request.expected_revision,
                        &draft,
                    ))
                    .map_err(str::to_owned)?;
            }
            CaOperation::PreviewRemove => reply
                .confirm(HostCaReplyExpectation::PreviewRemove(
                    id.ok_or("缺少 CA 范围")?,
                    request.expected_revision,
                ))
                .map_err(str::to_owned)?,
            CaOperation::ApplySave | CaOperation::ApplyRemove => reply
                .confirm(HostCaReplyExpectation::Applied(
                    command.review.as_deref().ok_or("缺少 CA 预览")?,
                ))
                .map_err(str::to_owned)?,
            CaOperation::InspectCertificate => {
                let input: HostCertificateRequest = serde_json::from_slice(&request.document_json)
                    .map_err(|_| "无效证书范围".to_owned())?;
                reply
                    .confirm(HostCaReplyExpectation::Certificate(&input))
                    .map_err(str::to_owned)?;
            }
        }
        Ok(reply)
    })
    .await
    .map_err(|_| "CA 请求超时；重新载入并准备新预览，不重放提交".to_owned())?
}
