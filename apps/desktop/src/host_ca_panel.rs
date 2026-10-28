use crate::{daemon_connection::DesktopConnectionConfig, host_ca_connection::Connection};
use cshell_application::*;
use cshell_domain::{ClipboardHost, HostAuthorityId};
use cshell_ipc::{CaOperation, CaRequest};
#[derive(Default)]
pub(super) struct HostCaPanel {
    pub open: bool,
    connection: Option<Connection>,
    error: Option<String>,
    draft: HostCaDraft,
    selected: Option<HostAuthorityId>,
    sent_draft: Option<HostCaDraft>,
    stamp: Option<(u64, Option<HostAuthorityId>, Option<[u8; 32]>)>,
    seen_preview: Option<[u8; 32]>,
    fingerprint: String,
    confirm: bool,
    certificate: String,
    target_host: String,
    target_port: u16,
    last_poll: Option<std::time::Instant>,
}
impl HostCaPanel {
    pub fn generation(&self) -> Option<u64> {
        self.connection.as_ref().map(|c| c.view().generation)
    }
    pub fn draw(&mut self, context: &egui::Context, config: Option<&DesktopConnectionConfig>) {
        if !self.open {
            return;
        }
        if self.connection.is_none()
            && self.error.is_none()
            && let Some(config) = config
        {
            match Connection::start(config.clone()) {
                Ok(c) => {
                    c.request(CaRequest::default(), true);
                    self.connection = Some(c);
                }
                Err(_) => self.error = Some("CA 连接线程无法启动".into()),
            }
        }
        context.request_repaint_after(std::time::Duration::from_millis(250));
        if let Some(c) = &self.connection
            && self
                .last_poll
                .is_none_or(|t| t.elapsed() >= std::time::Duration::from_secs(2))
        {
            if c.request(CaRequest::default(), false) {
                self.last_poll = Some(std::time::Instant::now());
            }
        }
        let view = self.connection.as_ref().map(Connection::view);
        let mut open = self.open;
        let mut command = None;
        cshell_ui::form::dialog(&context, "OpenSSH CA 与证书").open(&mut open).default_size([820.0,700.0]).show(context,|ui|{
            ui.weak("托管 CA 公钥与现有 known_hosts 合并校验。停用作用于本目录；吊销全局拒绝该公钥及其签发的主机证书。公钥轮换创建新记录。");
            if ui.button("明确重新载入信任目录").clicked(){self.error=None;command=Some((CaRequest::default(),true));}
            if let Some(error)=&self.error{ui.colored_label(cshell_ui::theme::DANGER,error);}
            let Some(view)=&view else{ui.label("daemon 连接不可用");return;};if let Some(error)=&view.error{ui.colored_label(cshell_ui::theme::DANGER,error);}
            if let Some(catalog)=&view.catalog{
                let stamp=(catalog.revision,self.selected,view.preview.as_ref().map(|p|p.token));if self.stamp!=Some(stamp){self.stamp=Some(stamp);self.confirm=false;self.fingerprint.clear();}
                if let Some(p)=&view.preview&&self.seen_preview!=Some(p.token){self.seen_preview=Some(p.token);if let Some(record)=&p.record&&self.selected==Some(p.id)&&self.sent_draft.as_ref()==Some(&self.draft){self.draft=record.definition.clone();}self.sent_draft=None;}
                let ready=view.writable&&!view.busy;ui.label(format!("版本 {} · {} 个 CA · {}",catalog.revision,catalog.items.len(),if ready{"可操作"}else{"等待／需明确重新载入"}));
                egui::ScrollArea::vertical().id_salt("host-ca-catalog").max_height(170.0).show(ui,|ui|{for row in &catalog.items{ui.push_id(row.id,|ui|{if ui.selectable_label(self.selected==Some(row.id),format!("{} · {:?} · {}",row.definition.name,row.definition.state,row.id)).clicked(){self.selected=Some(row.id);self.draft=row.definition.clone();self.confirm=false;self.fingerprint.clear();}ui.label(&row.definition.patterns);ui.monospace(&row.fingerprint);});}});
                if ui.button("新建 CA 草稿").clicked(){self.selected=Some(HostAuthorityId::new());self.draft=HostCaDraft::default();self.confirm=false;self.fingerprint.clear();}
                ui.label("名称");let mut changed=ui.text_edit_singleline(&mut self.draft.name).changed();ui.label("OpenSSH 主机范围，逗号分隔；非 22 端口用 [host]:port");changed|=ui.text_edit_singleline(&mut self.draft.patterns).changed();ui.label("OpenSSH CA 公钥");changed|=ui.add(egui::TextEdit::multiline(&mut self.draft.public_key).desired_rows(3).char_limit(16384)).changed();
                ui.horizontal_wrapped(|ui|{for(state,label)in[(HostCaState::Disabled,"停用"),(HostCaState::Trusted,"按范围信任"),(HostCaState::Revoked,"全局吊销公钥")]{changed|=ui.selectable_value(&mut self.draft.state,state,label).changed();}});
                if changed{self.confirm=false;self.fingerprint.clear();}
                ui.horizontal_wrapped(|ui|{
                    if ui.add_enabled(ready,egui::Button::new("规范化并预览保存／状态变更")).clicked(){let id=*self.selected.get_or_insert_with(HostAuthorityId::new);let mut draft=self.draft.clone();draft.public_key=draft.public_key.trim().into();self.draft=draft.clone();self.sent_draft=Some(draft.clone());command=Some((CaRequest{operation:CaOperation::PreviewSave as i32,authority_id:id.as_uuid().as_bytes().to_vec(),expected_revision:catalog.revision,document_json:serde_json::to_vec(&draft).unwrap_or_default(),..Default::default()},false));}
                    if let Some(id)=self.selected&&let Some(row)=catalog.items.iter().find(|r|r.id==id)&&ui.add_enabled(ready&&row.definition.state!=HostCaState::Revoked,egui::Button::new("预览移除该 CA")).clicked(){command=Some((CaRequest{operation:CaOperation::PreviewRemove as i32,authority_id:id.as_uuid().as_bytes().to_vec(),expected_revision:catalog.revision,..Default::default()},false));}
                });
                if let Some(p)=&view.preview&&self.selected==Some(p.id){
                    ui.separator();ui.label(format!("预览 {} · 版本 {} · 到期 Unix {}",p.id,p.catalog_revision,p.expires_unix));
                    if let Some(row)=&p.record{ui.label(format!("保存 {} · {:?} · {}",row.definition.name,row.definition.state,row.definition.patterns));ui.add(egui::Label::new(&row.definition.public_key).wrap().selectable(true));}else{ui.label("移除目录记录；现有 known_hosts 条目继续独立生效");}
                    ui.monospace(&p.fingerprint);ui.label("输入完整 SHA256 指纹确认");if ui.add(egui::TextEdit::singleline(&mut self.fingerprint).char_limit(64)).changed(){self.confirm=false;}
                    ui.checkbox(&mut self.confirm,"确认此预览的公钥、范围、状态及目录版本；提交只发送一次");
                    let draft_matches=p.record.as_ref().is_none_or(|r|r.definition==self.draft);let now=std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(u64::MAX,|d|d.as_secs());
                    if ui.add_enabled(ready&&self.confirm&&self.fingerprint==p.fingerprint&&p.catalog_revision==catalog.revision&&draft_matches&&now<p.expires_unix,egui::Button::new("确认提交信任变更")).clicked(){command=Some((CaRequest{operation:if p.record.is_some(){CaOperation::ApplySave}else{CaOperation::ApplyRemove} as i32,authority_id:p.id.as_uuid().as_bytes().to_vec(),expected_revision:p.catalog_revision,preview_token:p.token.to_vec(),confirmed_fingerprint:self.fingerprint.clone(),confirmed:true,..Default::default()},false));self.confirm=false;self.fingerprint.clear();}
                    if !draft_matches{ui.weak("草稿已变化，重新预览后确认。");}
                }
            }
            ui.collapsing("只读检查 OpenSSH 公钥证书",|ui|{
                ui.add(egui::TextEdit::multiline(&mut self.certificate).desired_rows(4).char_limit(65536));ui.horizontal_wrapped(|ui|{ui.label("可选主机上下文");ui.text_edit_singleline(&mut self.target_host);if self.target_port==0{self.target_port=22;}ui.add(egui::DragValue::new(&mut self.target_port).range(1..=65535));});
                if ui.add_enabled(!view.busy&&!self.certificate.trim().is_empty()&&self.certificate.len()<=65536,egui::Button::new("检查签名、有效期、主体和当前主机信任")).clicked(){let target=if self.target_host.trim().is_empty(){Some(None)}else{ClipboardHost::new(self.target_host.trim(),self.target_port).map(Some)};if let Some(target)=target{command=Some((CaRequest{operation:CaOperation::InspectCertificate as i32,document_json:serde_json::to_vec(&HostCertificateRequest{certificate:self.certificate.clone(),target}).unwrap_or_default(),..Default::default()},false));}else{self.error=Some("主机上下文无效".into());}}
                if let Some(report)=&view.certificate{let target=if self.target_host.trim().is_empty(){Some(None)}else{ClipboardHost::new(self.target_host.trim(),self.target_port).map(Some)};let matches=sftp_digest(self.certificate.trim().as_bytes()).sha256==report.certificate_sha256&&target.as_ref()==Some(&report.target)&&report.catalog_revision.is_none_or(|revision|view.catalog.as_ref().is_some_and(|c|c.revision==revision));if !matches{ui.weak("证书输入、上下文或目录已变化，请重新检查。");}else{ui.label(format!("{} · serial {} · ID {}",if report.host_certificate{"主机证书"}else{"用户证书"},report.serial,report.key_id));ui.label(format!("签名数学校验 {} · 检查时刻有效 {} · Unix {}",report.signature_valid,report.time_valid,report.checked_unix));ui.label(format!("有效期 {} ≤ time < {}",report.valid_after,report.valid_before));ui.label(format!("主体 {:?}",report.principals));ui.monospace(format!("主体 {}",report.subject_fingerprint));ui.monospace(format!("签发者 {}",report.signer_fingerprint));ui.label(format!("critical {:?} · extensions {:?}",report.critical_options,report.extensions));if let Some(target)=&report.target{ui.label(format!("上下文 {}:{} · 检查时信任 {:?}",target.host,target.port,report.host_decision));}ui.weak("检查不连接服务器。用户证书的服务端授权由 sshd 配置决定。");}}
            });
        });
        self.open = open;
        if let Some((request, explicit)) = command
            && let Some(c) = &self.connection
            && !c.request(request, explicit)
        {
            self.error = Some("CA 状态或队列变化；重新载入并准备新预览".into());
        }
    }
}
