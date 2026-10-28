use crate::daemon_connection::DesktopConnectionConfig;
use cshell_application::*;
use cshell_domain::SessionId;
use cshell_ipc::{
    Envelope, Handshake, TerminalToolsOperation as Op, TerminalToolsRequest as Request,
    TerminalToolsStatus as Status, client_handshake, envelope, features, read_envelope, transport,
    write_envelope,
};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
#[derive(Clone, Default)]
pub(super) struct View {
    pub catalog: Option<Arc<TerminalToolsView>>,
    pub raw: Option<Arc<RawJournalSlice>>,
    pub raw_epoch: u64,
    pub shell: Option<cshell_domain::ShellHistory>,
    pub shell_epoch:u64,
    pub generation: u64,
    pub busy: bool,
    pub writable: bool,
    pub error: Option<String>,
    pub imported: Option<(u64, HighlightDocument)>,
    pub file: Option<(u64, Arc<[u8]>)>,
    pub status: String,
    pub saved: Option<Arc<TerminalToolsCatalog>>,
}
enum Command {
    Request(Request, u64, bool),
    Import(String, u64),
    Export(String, Arc<TerminalToolsCatalog>),
    ReplayFile(String, u64),
    ExportAnsi(Request, u64, String),
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
        let worker=std::thread::Builder::new().name("cshell-terminal-tools-ipc".into()).spawn(move||{
            let runtime=match tokio::runtime::Builder::new_current_thread().enable_all().build(){Ok(r)=>r,Err(_)=>{shared.lock().unwrap_or_else(std::sync::PoisonError::into_inner).error=Some("终端工具线程无法启动".into());return;}};
            runtime.block_on(async{loop{
                let command=tokio::select!{biased;_=stopped.changed()=>break,c=receiver.recv()=>match c{Some(c)=>c,None=>break}};
                match command{
                    Command::Request(request,epoch,explicit)=>{
                        let mutation=matches!(Op::try_from(request.operation),Ok(Op::Save|Op::Arm));
                        let result=tokio::select!{biased;_=stopped.changed()=>break,r=query(&config,&request)=>r};
                        let mut view=shared.lock().unwrap_or_else(std::sync::PoisonError::into_inner);view.busy=false;view.generation=view.generation.wrapping_add(1);
                        match result{
                            Ok(TerminalToolsReply::View(catalog))=>{
                                if view.catalog.as_ref().is_some_and(|old|old.catalog.revision>catalog.catalog.revision){view.error=Some("配置版本回退，请重新读取".into());view.writable=false;}
                                else{if request.operation==Op::Save as i32{view.saved=Some(Arc::new(catalog.catalog.clone()));}view.catalog=Some(Arc::new(catalog));view.error=None;view.status="已读取终端工具状态".into();if explicit&&request.operation==Op::Get as i32{view.writable=true;}if request.operation==Op::Save as i32{view.imported=None;}}
                            },
                            Ok(TerminalToolsReply::Shell(data))=>{view.shell=Some(data);view.shell_epoch=epoch;view.error=None;},
                            Ok(TerminalToolsReply::Raw(raw))=>{view.raw=Some(Arc::new(raw));view.raw_epoch=epoch;view.error=None;},
                            Err(error)=>{view.error=Some(error);if mutation{view.writable=false;}if request.operation==Op::RawJournal as i32{view.raw=None;}}
                        }
                    },
                    Command::Import(path,epoch)=>{
                        let result=tokio::task::spawn_blocking(move||crate::terminal_tools_panel::read_highlight_file(&path)).await.unwrap_or(Err("高亮导入线程失败".into()));
                        let mut view=shared.lock().unwrap_or_else(std::sync::PoisonError::into_inner);view.busy=false;view.generation=view.generation.wrapping_add(1);match result{Ok(doc)=>{view.imported=Some((epoch,doc));view.error=None;},Err(error)=>view.error=Some(error)}
                    },
                    Command::ReplayFile(path,epoch)=>{
                        let result=tokio::task::spawn_blocking(move||{
                            use std::io::Read;let file=std::fs::File::open(path).map_err(|_|"ANSI 文件打开失败".to_owned())?;
                            if !file.metadata().map_err(|_|"ANSI 文件检查失败".to_owned())?.is_file(){return Err("ANSI 输入必须是普通文件".into());}
                            let mut bytes=Vec::new();file.take(8*1024*1024+1).read_to_end(&mut bytes).map_err(|_|"ANSI 文件读取失败".to_owned())?;
                            if bytes.len()>8*1024*1024{return Err("ANSI 文件超过 8 MiB".into());}Ok(Arc::<[u8]>::from(bytes))
                        }).await.unwrap_or(Err("ANSI 文件线程失败".into()));
                        let mut view=shared.lock().unwrap_or_else(std::sync::PoisonError::into_inner);view.busy=false;view.generation=view.generation.wrapping_add(1);match result{Ok(bytes)=>{view.file=Some((epoch,bytes));view.error=None;},Err(error)=>{view.file=None;view.error=Some(error);}}
                    },
                    Command::ExportAnsi(mut request,count,path)=>{
                        let result=tokio::select!{biased;_=stopped.changed()=>break,r=async{
                            let end=request.cursor.checked_add(count).ok_or("日志范围溢出".to_owned())?;if count==0||count>4096{return Err("日志导出最多 4096 条记录".into());}
                            let deadline=tokio::time::Instant::now()+Duration::from_secs(120);let mut bytes=Vec::new();while request.cursor<end{
                                let TerminalToolsReply::Raw(raw)=tokio::time::timeout_at(deadline,query(&config,&request)).await.map_err(|_|"ANSI 导出超过 120 秒".to_owned())??else{return Err("日志回复类型错误".into());};
                                if raw.total_records<end{return Err("日志范围不完整".into());}if bytes.len()+raw.data.len()>8*1024*1024{return Err("ANSI 导出超过 8 MiB".into());}bytes.extend_from_slice(&raw.data);(request.cursor,request.offset)=raw.next_cursor();
                            }crate::profile_export::save_export(&path,&bytes)
                        }=>r};
                        let mut view=shared.lock().unwrap_or_else(std::sync::PoisonError::into_inner);view.busy=false;view.generation=view.generation.wrapping_add(1);match result{Ok(())=>{view.status="ANSI 原始输出已导出（不含输入/时间旁路）".into();view.error=None;},Err(error)=>view.error=Some(error)}
                    },
                    Command::Export(path,catalog)=>{
                        let result=tokio::task::spawn_blocking(move||{let doc=HighlightDocument{format:"cshell.highlight-sets".into(),version:1,sets:catalog.highlights.clone()};let bytes=serde_json::to_vec_pretty(&doc).map_err(|_|"高亮导出编码失败".to_owned())?;crate::profile_export::save_export(&path,&bytes)}).await.unwrap_or(Err("高亮导出线程失败".into()));
                        let mut view=shared.lock().unwrap_or_else(std::sync::PoisonError::into_inner);view.busy=false;view.generation=view.generation.wrapping_add(1);match result{Ok(())=>{view.status="HighlightSet 已导出".into();view.error=None;},Err(error)=>view.error=Some(error)}
                    },
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
    fn send(&self, command: Command) -> bool {
        let mut view = self
            .view
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if view.busy {
            return false;
        }
        if let Command::Request(req, _, _) = &command
            && matches!(Op::try_from(req.operation), Ok(Op::Save | Op::Arm))
            && (!view.writable
                || view
                    .catalog
                    .as_ref()
                    .is_none_or(|c| c.catalog.revision != req.expected_revision))
        {
            return false;
        }
        let accepted = self
            .sender
            .as_ref()
            .is_some_and(|s| s.try_send(command).is_ok());
        if accepted {
            view.busy = true;
        }
        accepted
    }
    pub fn request(&self, request: Request, epoch: u64, explicit: bool) -> bool {
        self.send(Command::Request(request, epoch, explicit))
    }
    pub fn import(&self, path: String, epoch: u64) -> bool {
        self.send(Command::Import(path, epoch))
    }
    pub fn export(&self, path: String, catalog: Arc<TerminalToolsCatalog>) -> bool {
        self.send(Command::Export(path, catalog))
    }
    pub fn replay_file(&self, path: String, epoch: u64) -> bool {
        self.send(Command::ReplayFile(path, epoch))
    }
    pub fn export_ansi(&self, request: Request, count: u64, path: String) -> bool {
        self.send(Command::ExportAnsi(request, count, path))
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
pub(super) async fn query(
    config: &DesktopConnectionConfig,
    request: &Request,
) -> Result<TerminalToolsReply, String> {
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
        hello.feature_bits = features::TERMINAL_TOOLS;
        let negotiated = client_handshake(&mut stream, 1, hello)
            .await
            .map_err(|_| "daemon 认证失败".to_owned())?;
        if request.operation==Op::ShellHistory as i32 && negotiated.protocol_minor<75{return Err("请一起更新 GUI 与后台，以支持命令导航".into());}
        if negotiated.feature_bits & features::TERMINAL_TOOLS == 0 {
            return Err("daemon 不支持终端工具".into());
        }
        write_envelope(
            &mut stream,
            &Envelope {
                request_id: 2,
                payload: Some(envelope::Payload::TerminalToolsRequest(request.clone())),
                ..Default::default()
            },
        )
        .await
        .map_err(|_| "终端工具发送结果不明，请读取状态，勿重放启用或保存".to_owned())?;
        let reply = read_envelope(&mut stream)
            .await
            .map_err(|_| "终端工具回复结果不明，请重新读取".to_owned())?;
        if reply.request_id != 2 {
            return Err("终端工具回复身份不匹配".into());
        }
        let Some(envelope::Payload::TerminalToolsResponse(reply)) = reply.payload else {
            return Err("终端工具回复类型不匹配".into());
        };
        if reply.detail.len() > 1024 {
            return Err("终端工具回复超限".into());
        }
        if reply.status != Status::Ok as i32 {
            return Err(reply.detail);
        }
        let saved = if request.operation == Op::Save as i32 {
            Some(decode_terminal_tools(&request.document_json).map_err(str::to_owned)?)
        } else {
            None
        };
        let id = if request.session_id.is_empty() {
            None
        } else {
            Some(SessionId::from_bytes(
                request
                    .session_id
                    .as_slice()
                    .try_into()
                    .map_err(|_| "无效会话范围".to_owned())?,
            ))
        };
        let expectation = match Op::try_from(request.operation)
            .map_err(|_| "无效终端工具操作".to_owned())?
        {
            Op::ShellHistory => TerminalToolsExpectation::Shell {session:id.ok_or("missing shell session")?,id:request.cursor},
            Op::Get => TerminalToolsExpectation::View { after: 0 },
            Op::Poll => TerminalToolsExpectation::View {
                after: request.cursor,
            },
            Op::Save => TerminalToolsExpectation::Saved(saved.as_ref().ok_or("缺少保存范围")?),
            Op::Arm => TerminalToolsExpectation::Armed {
                session: id.ok_or("缺少会话范围")?,
                revision: request.expected_revision,
                allow_text: request.allow_text,
            },
            Op::Disarm => TerminalToolsExpectation::Disarmed(id.ok_or("缺少会话范围")?),
            Op::RawJournal => TerminalToolsExpectation::Raw {
                session: id.ok_or("缺少会话范围")?,
                sequence: request.cursor,
                offset: request.offset,
            },
        };
        decode_terminal_tools_reply(&reply.document_json, expectation).map_err(str::to_owned)
    })
    .await
    .map_err(|_| "终端工具超时，请读取状态；勿重放保存或启用".to_owned())?
}
