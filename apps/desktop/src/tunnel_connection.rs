use crate::daemon_connection::DesktopConnectionConfig;
use cshell_application::{
    MAX_TUNNEL_DOCUMENT, TunnelDedicatedConfiguration, TunnelDefinition, TunnelReplyExpectation,
    TunnelSnapshot, decode_tunnels,
};
use cshell_domain::{SessionId, TunnelId};
use cshell_ipc::{
    Envelope, Handshake, TunnelOperation, TunnelRequest, TunnelStatus, client_handshake, envelope,
    features, read_envelope, transport, write_envelope,
};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
#[derive(Clone, Default)]
pub(super) struct View {
    pub snapshot: Option<Arc<TunnelSnapshot>>,
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
    request: TunnelRequest,
    explicit: bool,
    epoch: u64,
}
impl std::fmt::Debug for Connection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TunnelConnection").finish_non_exhaustive()
    }
}
impl Connection {
    pub fn start(config: DesktopConnectionConfig) -> Result<Self, std::io::Error> {
        let view = Arc::new(Mutex::new(View::default()));
        let shared = view.clone();
        let (sender, mut receiver) = tokio::sync::mpsc::channel::<Command>(4);
        let (shutdown, mut stop) = tokio::sync::watch::channel(false);
        let worker=std::thread::Builder::new().name("cshell-tunnel-ipc".into()).spawn(move||{
            let runtime=match tokio::runtime::Builder::new_current_thread().enable_all().build(){Ok(r)=>r,Err(_)=>{shared.lock().unwrap_or_else(std::sync::PoisonError::into_inner).error=Some("隧道后台连接无法启动".into());return;}};
            runtime.block_on(async{loop{
                let cmd=tokio::select!{_ = stop.changed()=>break, cmd=receiver.recv()=>match cmd{Some(c)=>c,None=>break}};
                let mutation=cmd.request.operation!=TunnelOperation::Get as i32;
                let result=tokio::select!{_ = stop.changed()=>break,r=send(&config,&cmd.request)=>r};
                let mut view=shared.lock().unwrap_or_else(std::sync::PoisonError::into_inner);view.busy=false;view.generation=view.generation.wrapping_add(1);
                match result{
                    Ok(snapshot) if view.snapshot.as_ref().is_none_or(|s|snapshot.revision>=s.revision)=>{view.snapshot=Some(Arc::new(snapshot));view.error=None;if cmd.explicit&&!mutation&&cmd.epoch==view.epoch{view.writable=true;}}
                    other=>{view.error=Some(other.err().unwrap_or_else(||"隧道版本回退；请重新载入".into()));if mutation{view.writable=false;view.epoch=view.epoch.wrapping_add(1);}}
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
    pub fn request(&self, request: TunnelRequest, explicit: bool) -> bool {
        let mut view = self
            .view
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if view.busy
            || request.operation != 0
                && (!view.writable
                    || view
                        .snapshot
                        .as_ref()
                        .is_none_or(|s| s.revision != request.expected_revision))
        {
            return false;
        }
        let command = Command {
            request,
            explicit,
            epoch: view.epoch,
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
async fn send(
    config: &DesktopConnectionConfig,
    request: &TunnelRequest,
) -> Result<TunnelSnapshot, String> {
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
        let required = features::TUNNEL_CONTROL
            | if request.operation >= 6 {
                features::DEDICATED_TUNNELS
            } else {
                0
            };
        hello.feature_bits = features::TUNNEL_CONTROL | features::DEDICATED_TUNNELS;
        let negotiated = client_handshake(&mut stream, 1, hello)
            .await
            .map_err(|_| "daemon 认证失败".to_owned())?;
        if negotiated.feature_bits & required != required {
            return Err("daemon 不支持隧道管理".into());
        }
        write_envelope(
            &mut stream,
            &Envelope {
                request_id: 2,
                payload: Some(envelope::Payload::TunnelRequest(request.clone())),
                ..Default::default()
            },
        )
        .await
        .map_err(|_| "发送结果不明；重新载入，不重放操作".to_owned())?;
        let reply = read_envelope(&mut stream)
            .await
            .map_err(|_| "回复结果不明；重新载入，不重放操作".to_owned())?;
        if reply.request_id != 2 {
            return Err("隧道回复身份不匹配".into());
        }
        let Some(envelope::Payload::TunnelResponse(reply)) = reply.payload else {
            return Err("隧道回复类型不匹配".into());
        };
        if reply.document_json.len() > MAX_TUNNEL_DOCUMENT || reply.detail.len() > 1024 {
            return Err("隧道回复超限".into());
        }
        if reply.status != TunnelStatus::Ok as i32 {
            return Err(reply.detail);
        }
        let snapshot = decode_tunnels(&reply.document_json).map_err(str::to_owned)?;
        if request.operation != 0 {
            let id = TunnelId::from_bytes(
                request
                    .tunnel_id
                    .as_slice()
                    .try_into()
                    .map_err(|_| "无效隧道范围".to_owned())?,
            );
            let definition: Option<TunnelDefinition> = if matches!(
                TunnelOperation::try_from(request.operation),
                Ok(TunnelOperation::Create | TunnelOperation::Update)
            ) {
                Some(
                    serde_json::from_slice(&request.document_json)
                        .map_err(|_| "无效隧道定义".to_owned())?,
                )
            } else {
                None
            };
            let expected = match TunnelOperation::try_from(request.operation)
                .map_err(|_| "无效操作".to_owned())?
            {
                TunnelOperation::Create | TunnelOperation::Update => {
                    TunnelReplyExpectation::Saved(id, definition.as_ref().ok_or("缺少定义")?)
                }
                TunnelOperation::Start => TunnelReplyExpectation::Starting(
                    id,
                    SessionId::from_bytes(
                        request
                            .session_id
                            .as_slice()
                            .try_into()
                            .map_err(|_| "无效会话".to_owned())?,
                    ),
                ),
                TunnelOperation::Stop => TunnelReplyExpectation::Stopping(id),
                TunnelOperation::Remove => TunnelReplyExpectation::Removed(id),
                TunnelOperation::ConfigureDedicated => {
                    let configuration: TunnelDedicatedConfiguration =
                        serde_json::from_slice(&request.document_json)
                            .map_err(|_| "无效专用连接配置".to_owned())?;
                    snapshot
                        .confirm_reply(
                            request.expected_revision,
                            TunnelReplyExpectation::DedicatedConfigured(id, &configuration),
                        )
                        .map_err(str::to_owned)?;
                    if snapshot
                        .items
                        .iter()
                        .find(|r| r.id == id)
                        .and_then(|r| r.dedicated.as_ref())
                        .is_none_or(|p| {
                            p.reference_session.as_uuid().as_bytes().as_slice()
                                != request.session_id.as_slice()
                        })
                    {
                        return Err("专用连接参考范围不匹配".into());
                    }
                    return Ok(snapshot);
                }
                TunnelOperation::StartDedicated => TunnelReplyExpectation::DedicatedStarting(id),
                TunnelOperation::ClearDedicated => TunnelReplyExpectation::DedicatedCleared(id),
                _ => return Err("无效修改操作".into()),
            };
            snapshot
                .confirm_reply(request.expected_revision, expected)
                .map_err(str::to_owned)?;
        }
        Ok(snapshot)
    })
    .await
    .map_err(|_| "隧道请求超时；重新载入，不重放操作".to_owned())?
}
