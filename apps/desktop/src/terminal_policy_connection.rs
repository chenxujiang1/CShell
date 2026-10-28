use crate::daemon_connection::DesktopConnectionConfig;
use cshell_application::*;
use cshell_ipc::{
    Envelope, Handshake, TerminalPolicyOperation as Op, TerminalPolicyRequest as Request,
    TerminalPolicyStatus as Status, client_handshake, envelope, features, read_envelope, transport,
    write_envelope,
};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
#[derive(Clone, Default)]
pub(super) struct View {
    pub catalog: Option<Arc<TerminalPolicyView>>,
    pub preview: Option<Arc<TerminalPolicyPreview>>,
    pub report: Option<Arc<TerminalResolutionReport>>,
    pub preview_epoch: u64,
    pub report_epoch: u64,
    pub busy: bool,
    pub writable: bool,
    pub generation: u64,
    pub error: Option<String>,
    pub saved: Option<Arc<TerminalPolicyCatalog>>,
}
struct Command {
    request: Request,
    epoch: u64,
    explicit: bool,
    review: Option<Arc<TerminalPolicyPreview>>,
    policies: Option<Arc<TerminalPolicyView>>,
    snapshot: Option<Arc<CatalogSnapshot>>,
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
        let worker=std::thread::Builder::new().name("cshell-terminal-policy-ipc".into()).spawn(move||{
            let runtime=match tokio::runtime::Builder::new_current_thread().enable_all().build(){Ok(r)=>r,Err(_)=>{shared.lock().unwrap_or_else(std::sync::PoisonError::into_inner).error=Some("策略线程无法启动".into());return;}};
            runtime.block_on(async{loop{
                let command=tokio::select!{biased;_=stopped.changed()=>break,c=receiver.recv()=>match c{Some(c)=>c,None=>break}};
                let apply=command.request.operation==Op::Apply as i32;
                let result=tokio::select!{biased;_=stopped.changed()=>break,r=query(&config,&command)=>r};
                let mut view=shared.lock().unwrap_or_else(std::sync::PoisonError::into_inner);view.busy=false;view.generation=view.generation.wrapping_add(1);
                match result{
                    Ok(TerminalPolicyReply::Catalog(catalog))=>{
                        if view.catalog.as_ref().is_some_and(|old|old.catalog.revision>catalog.catalog.revision){view.error=Some("策略版本回退，请恢复原目录".into());view.writable=false;}
                        else{if apply{view.saved=Some(Arc::new(catalog.catalog.clone()));}if apply||view.catalog.as_ref().is_none_or(|old|old.catalog.revision!=catalog.catalog.revision||old.profile_revision!=catalog.profile_revision||old.host_revision!=catalog.host_revision){view.preview=None;view.report=None;}view.catalog=Some(Arc::new(catalog));view.error=None;if command.explicit&&command.request.operation==Op::Get as i32{view.writable=true;}}
                    },
                    Ok(TerminalPolicyReply::Preview(p))=>{view.preview=Some(Arc::new(p));view.preview_epoch=command.epoch;view.error=None;},
                    Ok(TerminalPolicyReply::Resolved(r))=>{view.report=Some(Arc::new(r));view.report_epoch=command.epoch;view.error=None;},
                    Err(error)=>{view.error=Some(error);if apply{view.preview=None;view.writable=false;}else if command.request.operation==Op::Preview as i32{view.preview=None;}else if command.request.operation==Op::Resolve as i32{view.report=None;}}
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
        explicit: bool,
        snapshot: Option<Arc<CatalogSnapshot>>,
    ) -> bool {
        let mut view = self
            .view
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if view.busy {
            return false;
        }
        if matches!(Op::try_from(request.operation), Ok(Op::Preview | Op::Apply))
            && (!view.writable
                || view.catalog.as_ref().is_none_or(|c| {
                    c.catalog.revision != request.expected_revision
                        || c.profile_revision != request.profile_revision
                        || c.host_revision != request.host_revision
                }))
        {
            return false;
        }
        if request.operation == Op::Apply as i32
            && view.preview.as_ref().is_none_or(|p| {
                p.token.as_slice() != request.preview_token.as_slice()
                    || p.fingerprint()
                        .ok()
                        .is_none_or(|sha| sha.as_slice() != request.review_sha256.as_slice())
            })
        {
            return false;
        }
        let command = Command {
            request,
            epoch,
            explicit,
            review: view.preview.clone(),
            policies: view.catalog.clone(),
            snapshot,
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
) -> Result<TerminalPolicyReply, String> {
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
        hello.feature_bits = features::TERMINAL_POLICIES;
        let negotiated = client_handshake(&mut stream, 1, hello)
            .await
            .map_err(|_| "daemon 认证失败".to_owned())?;
        if negotiated.feature_bits & features::TERMINAL_POLICIES == 0 {
            return Err("daemon 不支持上下文继承策略".into());
        }
        write_envelope(
            &mut stream,
            &Envelope {
                request_id: 2,
                payload: Some(envelope::Payload::TerminalPolicyRequest(
                    command.request.clone(),
                )),
                ..Default::default()
            },
        )
        .await
        .map_err(|_| "策略发送结果不明，请读取并重新预览，勿重放提交".to_owned())?;
        let response = read_envelope(&mut stream)
            .await
            .map_err(|_| "策略回复结果不明，请明确读取".to_owned())?;
        if response.request_id != 2 {
            return Err("策略回复身份不匹配".into());
        }
        let Some(envelope::Payload::TerminalPolicyResponse(response)) = response.payload else {
            return Err("策略回复类型不匹配".into());
        };
        if response.detail.len() > 4096 {
            return Err("策略回复错误信息超限".into());
        }
        if response.status != Status::Ok as i32 {
            return Err(response.detail);
        }
        let req = &command.request;
        let proposal = if req.operation == Op::Preview as i32 {
            Some(
                serde_json::from_slice::<TerminalPolicyProposal>(&req.document_json)
                    .map_err(|_| "无效策略提案".to_owned())?,
            )
        } else {
            None
        };
        let context = if req.operation == Op::Resolve as i32 {
            Some(
                serde_json::from_slice::<TerminalResolutionContext>(&req.document_json)
                    .map_err(|_| "无效继承上下文".to_owned())?,
            )
        } else {
            None
        };
        let expect = match Op::try_from(req.operation).map_err(|_| "无效策略操作".to_owned())?
        {
            Op::Get => TerminalPolicyExpectation::Catalog,
            Op::Preview => {
                TerminalPolicyExpectation::Preview(proposal.as_ref().ok_or("缺少策略范围")?)
            }
            Op::Apply => {
                TerminalPolicyExpectation::Saved(command.review.as_deref().ok_or("缺少策略预览")?)
            }
            Op::Resolve => TerminalPolicyExpectation::Resolved {
                context: context.as_ref().ok_or("缺少上下文")?,
                profile_revision: req.profile_revision,
                host_revision: req.host_revision,
                policy_revision: req.expected_revision,
            },
        };
        let reply = decode_terminal_policy_reply(&response.document_json, expect)?;
        if let (TerminalPolicyReply::Resolved(report), Some(snapshot), Some(policies)) =
            (&reply, &command.snapshot, &command.policies)
        {
            if snapshot.revision != report.profile_revision
                || policies.catalog.revision != report.policy_revision
                || snapshot.profiles.iter().find(|p| p.id == report.profile.id)
                    != Some(&report.profile)
            {
                return Err("继承上下文与捕获目录不匹配".into());
            }
            let expected = resolve_terminal_context(
                snapshot,
                &policies.catalog,
                &report.context,
                report.host.as_ref(),
            )?;
            if expected != (report.settings.clone(), report.matched_tags.clone()) {
                return Err("继承值或来源不匹配".into());
            }
        }
        Ok(reply)
    })
    .await
    .map_err(|_| "策略操作超时，请重新读取；勿重放提交".to_owned())?
}
