use crate::{daemon_connection::DesktopConnectionConfig, tunnel_connection::Connection};
use cshell_application::{
    TunnelDedicatedConfiguration, TunnelDefinition, TunnelKind, TunnelReconnectPolicy, TunnelState,
};
use cshell_domain::{ProfileId, SessionId, TunnelId};
use cshell_ipc::{TunnelOperation, TunnelRequest};
#[derive(Default)]
pub(super) struct TunnelPanel {
    pub open: bool,
    connection: Option<Connection>,
    error: Option<String>,
    draft: TunnelDefinition,
    selected: Option<TunnelId>,
    stamp: Option<(u64, Option<SessionId>, Option<TunnelId>)>,
    save_confirm: bool,
    start_confirm: bool,
    stop_confirm: bool,
    remove_confirm: bool,
    last_poll: Option<std::time::Instant>,
    dedicated_profile_stamp: Option<(ProfileId, u64)>,
    freeze_confirm: bool,
    dedicated_start_confirm: bool,
    clear_dedicated_confirm: bool,
    retry_enabled: bool,
    retry_max: u32,
    retry_delay: u32,
}
impl TunnelPanel {
    pub fn generation(&self) -> Option<u64> {
        self.connection.as_ref().map(|c| c.view().generation)
    }
    pub fn draw(
        &mut self,
        context: &egui::Context,
        session: Option<SessionId>,
        config: Option<&DesktopConnectionConfig>,
        profile: Option<(ProfileId, u64)>,
    ) {
        if !self.open {
            return;
        }
        if self.dedicated_profile_stamp != profile {
            self.dedicated_profile_stamp = profile;
            self.freeze_confirm = false;
            self.dedicated_start_confirm = false;
            self.clear_dedicated_confirm = false;
        }
        if self.connection.is_none()
            && self.error.is_none()
            && let Some(config) = config
        {
            match Connection::start(config.clone()) {
                Ok(c) => {
                    c.request(TunnelRequest::default(), true);
                    self.connection = Some(c);
                }
                Err(_) => self.error = Some("隧道后台线程无法启动".into()),
            }
        }
        context.request_repaint_after(std::time::Duration::from_millis(250));
        if let Some(c) = &self.connection
            && self
                .last_poll
                .is_none_or(|t| t.elapsed() >= std::time::Duration::from_secs(1))
        {
            if c.request(TunnelRequest::default(), false) {
                self.last_poll = Some(std::time::Instant::now());
            }
        }
        let view = self.connection.as_ref().map(Connection::view);
        let mut open = self.open;
        let mut command = None;
        cshell_ui::form::dialog(&context, "SSH 隧道管理").open(&mut open).default_size([760.0,660.0]).show(context,|ui|{
            ui.weak("保存不会启动。可绑定交互 SSH 会话，或另建无 PTY 的专用连接；专用连接关闭参考标签后仍可运行。");
            if ui.button("刷新隧道").clicked(){self.error=None;command=Some((TunnelRequest::default(),true));}
            if let Some(error)=&self.error{ui.colored_label(cshell_ui::theme::DANGER,error);}
            let Some(view)=&view else{ui.label("daemon 连接不可用");return;};
            if let Some(error)=&view.error{ui.colored_label(cshell_ui::theme::DANGER,error);}
            let Some(snapshot)=&view.snapshot else{ui.label("等待载入隧道配置");return;};
            let stamp=(snapshot.revision,session,self.selected);
            if self.stamp!=Some(stamp){self.stamp=Some(stamp);self.save_confirm=false;self.start_confirm=false;self.stop_confirm=false;self.remove_confirm=false;self.freeze_confirm=false;self.dedicated_start_confirm=false;self.clear_dedicated_confirm=false;}
            let ready=view.writable&&!view.busy;
            ui.label(format!("{} 条隧道 · {}",snapshot.items.len(),if view.busy{"处理中"}else if ready{"已就绪"}else{"请刷新状态"}));
            egui::ScrollArea::vertical().id_salt("tunnel-catalog").max_height(200.0).show(ui,|ui|{
                for row in &snapshot.items{ui.push_id(row.id,|ui|{
                    if ui.selectable_label(self.selected==Some(row.id),format!("{} · {:?} · {:?}",row.definition.name,row.definition.kind,row.state)).clicked(){self.selected=Some(row.id);self.draft=row.definition.clone();let policy=row.dedicated.as_ref().and_then(|p|p.configuration.reconnect.as_ref()).filter(|p|p.max_retries>0);self.retry_enabled=policy.is_some();self.retry_max=policy.map_or(3,|p|p.max_retries);self.retry_delay=policy.map_or(5000,|p|p.delay_ms);self.save_confirm=false;self.start_confirm=false;self.stop_confirm=false;self.remove_confirm=false;self.freeze_confirm=false;self.dedicated_start_confirm=false;self.clear_dedicated_confirm=false;}
                    ui.label(format!("绑定 {}:{} → {}:{}",row.definition.bind_address,row.definition.bind_port,row.definition.target_host,row.definition.target_port));
                    if let(Some(address),Some(port))=(&row.bound_address,row.bound_port){ui.label(format!("已确认绑定／最近回执：{address}:{port}"));}
                    if let Some(peer)=&row.peer{ui.label(format!("{}@{}:{} · {}",peer.username,peer.host.host,peer.host.port,peer.host_key_sha256));}
                    if row.dedicated.is_some(){ui.label(format!("专用 SSH · 恢复次数 {} · 运行独立于参考标签",row.reconnect_attempt));}
                    ui.weak(&row.detail);
                    if row.state==TunnelState::Unknown{ui.weak(if row.dedicated_run{"专用运行不明：自动恢复已停止，先明确停止／清除旧绑定；不在新连接上取消原授权。"}else{"先停止原绑定。不明的远端自动分配端口需要关闭原 SSH 会话，再明确清除旧绑定。"});}
                });}
            });
            ui.separator();
            let mut changed=ui.text_edit_singleline(&mut self.draft.name).changed();
            let previous=self.draft.kind;
            ui.horizontal_wrapped(|ui|{for(kind,label)in[(TunnelKind::Local,"本地转发"),(TunnelKind::Remote,"远程转发"),(TunnelKind::Dynamic,"动态 SOCKS5")]{changed|=ui.selectable_value(&mut self.draft.kind,kind,label).changed();}});
            if previous!=self.draft.kind{if self.draft.kind==TunnelKind::Dynamic{self.draft.target_host.clear();self.draft.target_port=0;}else if previous==TunnelKind::Dynamic{self.draft.target_host="127.0.0.1".into();self.draft.target_port=22;}}
            ui.horizontal_wrapped(|ui|{ui.label("监听 IP");changed|=ui.text_edit_singleline(&mut self.draft.bind_address).changed();ui.label("端口（0 自动分配）");changed|=ui.add(egui::DragValue::new(&mut self.draft.bind_port)).changed();});
            if self.draft.kind!=TunnelKind::Dynamic{ui.horizontal_wrapped(|ui|{ui.label(if self.draft.kind==TunnelKind::Remote{"daemon 本机目标"}else{"SSH 服务端可访问目标"});changed|=ui.text_edit_singleline(&mut self.draft.target_host).changed();changed|=ui.add(egui::DragValue::new(&mut self.draft.target_port).range(1..=65535)).changed();});}
            changed|=ui.checkbox(&mut self.draft.allow_non_loopback,"允许非 loopback 监听；明确接受其他机器连接此端口").changed();
            ui.horizontal_wrapped(|ui|{ui.label("最多连接");changed|=ui.add(egui::DragValue::new(&mut self.draft.max_connections).range(1..=128)).changed();ui.label("SOCKS 握手毫秒");changed|=ui.add(egui::DragValue::new(&mut self.draft.socks_timeout_ms).range(1000..=30000)).changed();});
            if changed{self.save_confirm=false;self.start_confirm=false;self.stop_confirm=false;self.remove_confirm=false;self.freeze_confirm=false;self.dedicated_start_confirm=false;self.clear_dedicated_confirm=false;}
            if let Err(error)=self.draft.validate(){ui.colored_label(cshell_ui::theme::DANGER,error);}
            ui.weak("保存配置后，可在列表中选择并启动隧道。");
            let selected=self.selected.and_then(|id|snapshot.items.iter().find(|r|r.id==id));
            ui.horizontal_wrapped(|ui|{
                if ui.add_enabled(ready&&self.draft.validate().is_ok(),egui::Button::new("新增隧道")).clicked(){command=Some((make(TunnelOperation::Create,TunnelId::new(),snapshot.revision,None,serde_json::to_vec(&self.draft).unwrap_or_default()),false));self.save_confirm=false;}
                if let Some(row)=selected && ui.add_enabled(ready&&self.draft.validate().is_ok()&&row.state==TunnelState::Stopped,egui::Button::new("保存所选隧道")).clicked(){command=Some((make(TunnelOperation::Update,row.id,snapshot.revision,None,serde_json::to_vec(&self.draft).unwrap_or_default()),false));self.save_confirm=false;}
            });
            if let Some(row)=selected{
                ui.label(format!("启动使用当前会话：{}",session.map_or("未选择".into(),|s|s.to_string())));
                if row.definition.allow_non_loopback {ui.checkbox(&mut self.start_confirm,"允许此隧道接收其他机器的连接");}
                if let Some(session)=session && ui.add_enabled(ready&&(!row.definition.allow_non_loopback||self.start_confirm)&&row.state==TunnelState::Stopped&&row.dedicated.is_none()&&self.draft==row.definition,egui::Button::new("启动所选隧道")).clicked(){command=Some((make(TunnelOperation::Start,row.id,snapshot.revision,Some(session),Vec::new()),false));self.start_confirm=false;}
                ui.collapsing("专用连接与有界恢复",|ui|{
                    if let Some(plan)=&row.dedicated {
                        ui.label(format!("冻结 Profile {} · 元数据版本 {}",plan.configuration.profile_id,plan.configuration.profile_revision));
                        for hop in &plan.jumps{ui.label(format!("跳板 {}@{}:{} · {:?}",hop.username,hop.host,hop.port,hop.auth_method));}
                        ui.label(format!("目标 {}@{}:{} · {:?}",plan.target.username,plan.target.host,plan.target.port,plan.target.auth_method));
                        ui.label(plan.configuration.reconnect.as_ref().map_or("自动恢复关闭".into(),|p|format!("本次运行最多 {} 次恢复，等待 {} ms；首次故障后恢复窗口 10 分钟",p.max_retries,p.delay_ms)));
                        ui.weak("冻结的是配置与引用；密钥文件和凭据在认证时读取，不能改变已捕获的完整 peer。");
                    }
                    let mut changed=ui.checkbox(&mut self.retry_enabled,"明确允许本次专用运行自动恢复（默认关闭）").changed();
                    if self.retry_enabled{if self.retry_max==0{self.retry_max=3;}if self.retry_delay==0{self.retry_delay=5000;}ui.horizontal_wrapped(|ui|{ui.label("最多恢复次数");changed|=ui.add(egui::DragValue::new(&mut self.retry_max).range(1..=5)).changed();ui.label("等待 ms");changed|=ui.add(egui::DragValue::new(&mut self.retry_delay).range(1000..=60000)).changed();});}
                    if changed{self.freeze_confirm=false;self.dedicated_start_confirm=false;}
                    let draft_policy=self.retry_enabled.then_some(TunnelReconnectPolicy{max_retries:self.retry_max,delay_ms:self.retry_delay});
                    let policy_matches=row.dedicated.as_ref().is_some_and(|p|p.configuration.reconnect.as_ref().filter(|p|p.max_retries>0)==draft_policy.as_ref());
                    if self.draft!=row.definition || row.dedicated.is_some()&&!policy_matches{ui.weak("存在未保存的定义或恢复策略；先保存，或重新选择记录载入已保存内容，再确认启动。");}
                    if let Some((id,revision))=profile{ui.label(format!("参考当前已验证会话的 Profile {id} · 版本 {revision}"));}else{ui.weak("冻结新计划需选择保存的 SSH 会话，并从 SSH 连接页载入当前 Profile 元数据。");}
                    ui.weak("保存连接方案不会启动监听；启动后使用上方目标和重连设置。");
                    if let (Some(reference),Some((id,revision)))=(session,profile) && ui.add_enabled(ready&&row.state==TunnelState::Stopped,egui::Button::new("保存专用连接方案")).clicked(){
                        let cfg=TunnelDedicatedConfiguration{profile_id:id,profile_revision:revision,reconnect:self.retry_enabled.then_some(TunnelReconnectPolicy{max_retries:self.retry_max,delay_ms:self.retry_delay})};
                        command=Some((make(TunnelOperation::ConfigureDedicated,row.id,snapshot.revision,Some(reference),serde_json::to_vec(&cfg).unwrap_or_default()),false));self.freeze_confirm=false;
                    }
                    if row.definition.allow_non_loopback {ui.checkbox(&mut self.dedicated_start_confirm,"允许专用隧道接收其他机器的连接");}
                    if ui.add_enabled(ready&&(!row.definition.allow_non_loopback||self.dedicated_start_confirm)&&row.state==TunnelState::Stopped&&row.dedicated.is_some()&&policy_matches&&self.draft==row.definition,egui::Button::new("启动专用连接")).clicked(){command=Some((make(TunnelOperation::StartDedicated,row.id,snapshot.revision,None,Vec::new()),false));self.dedicated_start_confirm=false;}
                    ui.checkbox(&mut self.clear_dedicated_confirm,"确认清除停止的专用计划；保留 peer 绑定");
                    if ui.add_enabled(ready&&self.clear_dedicated_confirm&&row.state==TunnelState::Stopped&&row.dedicated.is_some(),egui::Button::new("清除专用计划")).clicked(){command=Some((make(TunnelOperation::ClearDedicated,row.id,snapshot.revision,None,Vec::new()),false));self.clear_dedicated_confirm=false;}
                });
                if row.state==TunnelState::Unknown {ui.checkbox(&mut self.stop_confirm,"确认停止状态未确认的原连接绑定");}
                if ui.add_enabled(ready&&(row.state!=TunnelState::Unknown||self.stop_confirm)&&row.state!=TunnelState::Stopped,egui::Button::new("停止隧道")).clicked(){command=Some((make(TunnelOperation::Stop,row.id,snapshot.revision,None,Vec::new()),false));self.stop_confirm=false;}
                ui.checkbox(&mut self.remove_confirm,"确认移除已停止的隧道配置");
                if ui.add_enabled(ready&&self.remove_confirm&&row.state==TunnelState::Stopped,egui::Button::new("移除配置")).clicked(){command=Some((make(TunnelOperation::Remove,row.id,snapshot.revision,None,Vec::new()),false));self.remove_confirm=false;}
            }
        });
        self.open = open;
        if let Some((request, explicit)) = command
            && let Some(c) = &self.connection
            && !c.request(request, explicit)
        {
            self.error = Some("隧道版本或请求队列变化；重新载入并确认".into());
        }
    }
}
fn make(
    op: TunnelOperation,
    id: TunnelId,
    revision: u64,
    session: Option<SessionId>,
    document_json: Vec<u8>,
) -> TunnelRequest {
    TunnelRequest {
        operation: op as i32,
        tunnel_id: id.as_uuid().as_bytes().to_vec(),
        expected_revision: revision,
        confirmed: true,
        session_id: session
            .map(|s| s.as_uuid().as_bytes().to_vec())
            .unwrap_or_default(),
        document_json,
    }
}
