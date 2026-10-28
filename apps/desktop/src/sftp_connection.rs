use crate::daemon_connection::DesktopConnectionConfig;
use cshell_application::{
    SftpBrowseOptions, SftpEditDocument, SftpEditState, SftpFileJob, SftpFileState, SftpLinkPolicy,
    SftpQueueDraft, SftpQueueSnapshot, SftpWorkspaceReply, SftpWorkspaceSummary, decode_sftp_queue,
    decode_sftp_workspace,
};
use cshell_domain::SessionId;
use cshell_ipc::{
    Envelope, Handshake, SftpDirectoryEntry, SftpOperation, SftpRequest, SftpResponse, SftpStatus,
    SftpTransfer, SftpTransferState, client_handshake, envelope, features, read_envelope,
    transport, write_envelope,
};
use std::sync::{Arc, Mutex};
pub(crate) mod lifecycle;
pub use lifecycle::ConnectionPhase;
mod transfer_controls;
mod queue_batch;
pub(crate) use queue_batch::BatchAction;
mod workspace;
mod tree;
mod sync;
pub(crate) use transfer_controls::can_pause as can_pause_transfer;

#[derive(Clone, Default)]
pub struct SftpView {
    pub(crate) connection_trace:Option<crate::connection_diagnostics::View>,
    pub edit_review:Option<Arc<cshell_application::SftpEditReview>>,
    pub sync_preview:Option<Arc<cshell_application::SftpSyncPreview>>,
    pub sync_pending:Option<(SessionId,SftpOperation)>,
    pub sync_error:Option<String>,
    pub trees:std::collections::BTreeMap<SessionId,cshell_application::SftpTreeSnapshot>,
    pub tree_pending:std::collections::BTreeSet<SessionId>,
    pub generation: u64,
    pub connection_profile: Option<cshell_domain::ProfileId>,
    pub connection_info: Option<cshell_application::SftpConnectionInfo>,
    pub connection_previous: Option<SessionId>,
    pub connection_phase: ConnectionPhase,
    pub connection_cancelled: bool,
    pub close_requested: bool,
    pub session_id: Option<SessionId>,
    pub entries: Vec<SftpDirectoryEntry>,
    pub truncated: bool,
    pub transfer: Option<SftpTransfer>,
    pub status: String,
    pub batch_status: Option<String>,
    pub error: Option<String>,
    pub shared_local_browser:bool,
    pub local_entries: Vec<SftpDirectoryEntry>,
    pub local_directory: Option<String>,
    pub local_truncated: bool,
    pub queue: Option<Arc<SftpQueueSnapshot>>,
    pub queue_writable: bool,
    pub queue_busy: bool,
    pub local_cursor: Option<BrowseHandle>,
    pub remote_cursor: Option<BrowseHandle>,
    pub remote_loaded_directory: Option<String>,
    pub remote_requested_directory: Option<String>,
    pub peer: Option<cshell_domain::ClipboardTrust>,
    pub migration_source: Option<Arc<SftpQueueSnapshot>>,
    pub file_job: Option<Arc<SftpFileJob>>,
    pub edit: Option<Arc<SftpEditDocument>>,
    pub edit_text: Option<Arc<String>>,
    pub history: Vec<SftpWorkspaceSummary>,
    pub history_offset: u32,
    pub history_more: bool,
    pub file_writable: bool,
    pub edit_writable: bool,
    pub workspace_busy: bool,
    pub workspace_epoch: u64,
    pub queue_transfers: std::collections::BTreeMap<SessionId, SftpTransfer>,
    pub paused_items: std::collections::BTreeSet<cshell_domain::SftpQueueItemId>,
    pub queue_hold: bool,
    pub stopping_items: std::collections::BTreeSet<cshell_domain::SftpQueueItemId>,
}

#[derive(Clone)]
pub struct BrowseHandle {
    pub id: Vec<u8>,
    pub revision: u64,
    pub directory: String,
}
impl std::fmt::Debug for BrowseHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BrowseHandle")
            .field("revision", &self.revision)
            .finish_non_exhaustive()
    }
}

pub enum SftpClientCommand {
    Sync(SessionId,SftpOperation,SessionId,Vec<u8>),
    Tree(SessionId,SftpOperation,SessionId,Option<cshell_application::SftpTreeStart>),
    ConnectProfile { session:SessionId, profile:cshell_domain::ProfileId, previous:Option<SessionId>, source_ssh_session:Option<SessionId> },
    Disconnect(SessionId),
    OpenBrowser(SessionId),
    BrowseManaged(SessionId, SftpBrowseOptions),
    BrowseLocal(SessionId,SftpBrowseOptions),
    QueueWrite(SessionId, SftpOperation, Vec<u8>, u64, Vec<u8>),
    Workspace(SessionId, WorkspaceCommand),
    BrowseNext(SessionId, bool, BrowseHandle),
    BrowseClose(SessionId, bool, BrowseHandle),
    LocalList(SessionId, String),
    QueueGet(SessionId),
    QueueAdd(SessionId, SftpQueueDraft, u64),
    TransferOne(SessionId, SftpQueueDraft, u64),
    TransferBatch(SessionId, Vec<SftpQueueDraft>, u64, bool),
    StopQueueItem(SessionId, cshell_domain::SftpQueueItemId, u64, bool),
    ResumeQueueItem(SessionId, cshell_domain::SftpQueueItemId, u64),
    LiveQueue(SessionId, bool),
    QueueBatch(SessionId, BatchAction, Vec<cshell_domain::SftpQueueItemId>, u64),
    QueueControl(SessionId, SftpOperation, u64, Vec<u8>),
    List(SessionId, String),
    Recover(SessionId),
    Upload(SessionId, String, String),
    Download(SessionId, String, String),
    Cancel(SessionId, Vec<u8>),
}
impl SftpClientCommand {
    fn session(&self)->SessionId {
        match self {
            Self::Sync(session,..)=>*session,
            Self::Tree(session,..)=>*session,
            Self::ConnectProfile{session,..}=>*session,
            Self::Disconnect(id)|Self::OpenBrowser(id)|Self::QueueGet(id)|Self::Recover(id)
            |Self::BrowseManaged(id,_)|Self::BrowseLocal(id,_)|Self::Workspace(id,_)|Self::LocalList(id,_)|Self::LiveQueue(id,_)|Self::List(id,_)|Self::Cancel(id,_)
            |Self::BrowseNext(id,_,_)|Self::BrowseClose(id,_,_)|Self::QueueAdd(id,_,_)|Self::TransferOne(id,_,_)|Self::ResumeQueueItem(id,_,_)|Self::Upload(id,_,_)|Self::Download(id,_,_)
            |Self::TransferBatch(id,_,_,_)|Self::StopQueueItem(id,_,_,_)|Self::QueueBatch(id,_,_,_)|Self::QueueControl(id,_,_,_)
            |Self::QueueWrite(id,_,_,_,_)=>*id,
        }
    }
}
pub struct WorkspaceCommand {
    pub operation: SftpOperation,
    pub data: Vec<u8>,
    pub revision: u64,
    pub object: Vec<u8>,
    pub source_session: Option<SessionId>,
    pub epoch: u64,
}
impl std::fmt::Debug for WorkspaceCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkspaceCommand")
            .field("operation", &self.operation)
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for SftpView {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SftpView")
            .field("generation", &self.generation)
            .field("session_id", &self.session_id)
            .field("entry_count", &self.entries.len())
            .field("transfer", &self.transfer)
            .field("has_error", &self.error.is_some())
            .finish_non_exhaustive()
    }
}
impl std::fmt::Debug for SftpClientCommand {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Sync(..)=>"Sync",
            Self::Tree(..)=>"Tree",
            Self::ConnectProfile{..} => "ConnectProfile",
            Self::Disconnect(..) => "Disconnect",
            Self::OpenBrowser(..) => "OpenBrowser",
            Self::BrowseManaged(..) => "BrowseManaged",
            Self::BrowseLocal(..) => "BrowseLocal",
            Self::QueueWrite(..) => "QueueWrite",
            Self::Workspace(..) => "Workspace",
            Self::BrowseNext(..) => "BrowseNext",
            Self::BrowseClose(..) => "BrowseClose",
            Self::LocalList(..) => "LocalList",
            Self::QueueGet(..) => "QueueGet",
            Self::QueueAdd(..) => "QueueAdd",
            Self::TransferOne(..) => "TransferOne",
            Self::TransferBatch(..) => "TransferBatch",
            Self::StopQueueItem(..) => "StopQueueItem",
            Self::ResumeQueueItem(..) => "ResumeQueueItem",
            Self::LiveQueue(..) => "LiveQueue",
            Self::QueueBatch(..) => "QueueBatch",
            Self::QueueControl(..) => "QueueControl",
            Self::List(..) => "List",
            Self::Recover(..) => "Recover",
            Self::Upload(..) => "Upload",
            Self::Download(..) => "Download",
            Self::Cancel(..) => "Cancel",
        })
    }
}

#[derive(Debug)]
pub struct DesktopSftpConnection {
    shared: Arc<Mutex<SftpView>>,
    sender: Option<tokio::sync::mpsc::Sender<SftpClientCommand>>,
    shutdown: tokio::sync::watch::Sender<bool>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl DesktopSftpConnection {
    #[cfg(test)]
    pub(crate) fn for_view_test(view:SftpView)->Self {
        Self{shared:Arc::new(Mutex::new(view)),sender:None,shutdown:tokio::sync::watch::channel(false).0,worker:None}
    }
    pub fn start(config: DesktopConnectionConfig) -> Result<Self, std::io::Error> {
        let shared = Arc::new(Mutex::new(SftpView::default()));
        let (sender, receiver) = tokio::sync::mpsc::channel(16);
        let (shutdown, shutdown_receiver) = tokio::sync::watch::channel(false);
        let worker_shared = Arc::clone(&shared);
        let worker = std::thread::Builder::new()
            .name("cshell-sftp-ipc".into())
            .spawn(move || {
                match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => {
                        runtime.block_on(run(config, worker_shared, receiver, shutdown_receiver))
                    }
                    Err(error) => {
                        worker_shared
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .error = Some(error.to_string());
                    }
                }
            })?;
        Ok(Self {
            shared,
            sender: Some(sender),
            shutdown,
            worker: Some(worker),
        })
    }

    pub fn view(&self) -> SftpView {
        self.shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub(crate) fn share_local_browser(&self) {
        self.shared.lock().unwrap_or_else(std::sync::PoisonError::into_inner).shared_local_browser=true;
    }
    pub(crate) fn tab_status(&self)->(u64,ConnectionPhase,Option<SessionId>,Option<String>,usize) {
        let view=self.shared.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let host=view.peer.as_ref().map(|p|format!("{}@{}:{}",p.username,p.host.host,p.host.port));
        (view.generation,view.connection_phase,view.session_id,host,view.queue.as_ref().map_or(0,|q|q.items.iter().filter(|i|matches!(i.state,cshell_application::SftpQueueState::Running|cshell_application::SftpQueueState::Pending)).count())+view.trees.values().filter(|r|!r.state.finished()).count())
    }
    pub(crate) fn retire(self) {
        // Joining an IPC worker can wait for bounded network cleanup. Keep it off the UI thread.
        let _=std::thread::Builder::new().name("cshell-sftp-close".into()).spawn(move||drop(self));
    }
    pub fn generation(&self) -> u64 {
        self.shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .generation
    }
    pub(crate) fn take_cancelled_connection(&self)->bool {
        let mut view=self.shared.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        std::mem::take(&mut view.connection_cancelled)
    }

    pub fn request(&self, mut command: SftpClientCommand) -> bool {
        let mutation = match &command {
            SftpClientCommand::QueueAdd(session, _, revision)
            | SftpClientCommand::TransferOne(session, _, revision)
            | SftpClientCommand::TransferBatch(session, _, revision, _)
            | SftpClientCommand::QueueBatch(session, _, _, revision)
            | SftpClientCommand::StopQueueItem(session, _, revision, _)
            | SftpClientCommand::ResumeQueueItem(session, _, revision)
            | SftpClientCommand::QueueControl(session, _, revision, _) => {
                Some((*session, *revision))
            }
            SftpClientCommand::QueueWrite(session, _, _, revision, _) => {
                Some((*session, *revision))
            }
            _ => None,
        };
        let mut view = self
            .shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let lifecycle_action = match &command {
            SftpClientCommand::ConnectProfile {session,profile,previous,..} => {
                if view.connection_phase.busy() || (view.connection_profile.is_some() && *previous != view.connection_previous.or(view.session_id)) {return false;}
                Some((*session,Some(*profile),*previous))
            }
            SftpClientCommand::Disconnect(session) => {
                if view.connection_profile.is_none() || view.session_id != Some(*session) {return false;}
                if view.connection_phase == ConnectionPhase::Connecting {
                    view.close_requested=true;
                    view.status="正在取消 SFTP 连接…".into();
                    view.generation=view.generation.wrapping_add(1);
                    return true;
                }
                if view.connection_phase==ConnectionPhase::Disconnecting {return false;}
                Some((*session,None,None))
            }
            _ => {
                if view.connection_profile.is_some() && (view.connection_phase!=ConnectionPhase::Connected || view.session_id!=Some(command.session())) {return false;}
                None
            }
        };
        let held = if let SftpClientCommand::LiveQueue(session, pause) = &command {
            if view.session_id != Some(*session)
                || !view.queue_writable
                || view.queue_busy
                || view.workspace_busy
            {
                return false;
            }
            Some(*pause)
        } else {
            None
        };
        if matches!(
            &command,
            SftpClientCommand::TransferBatch(..) | SftpClientCommand::StopQueueItem(..) | SftpClientCommand::QueueBatch(..)
        ) && view.workspace_busy
        {
            return false;
        }
        if let SftpClientCommand::StopQueueItem(_, item, _, _) = &command
            && view.stopping_items.contains(item)
        {
            return false;
        }
        let resumed = match &command {
            SftpClientCommand::QueueControl(_, SftpOperation::QueueResumeVerified, _, item) => {
                <[u8; 16]>::try_from(item.as_slice())
                    .ok()
                    .map(cshell_domain::SftpQueueItemId::from_bytes)
            }
            _ => None,
        };
        if matches!(&command, SftpClientCommand::QueueGet(..))
            && (view.queue_busy || view.workspace_busy)
        {
            return false;
        }
        if let Some((session, revision)) = mutation {
            if view.session_id != Some(session)
                || !view.queue_writable
                || view.queue_busy
                || view.queue.as_ref().is_none_or(|q| q.revision != revision)
            {
                return false;
            }
        }
        if let SftpClientCommand::Workspace(session, cmd) = &mut command {
            if view.session_id.is_none() {
                reset_scope(&mut view, *session);
            }
            if view.workspace_busy
                || view.session_id.is_some_and(|s| s != *session)
                || cmd.data.len() > cshell_application::MAX_SFTP_WORKSPACE_DOCUMENT
            {
                return false;
            }
            cmd.epoch = view.workspace_epoch;
            let wire = cmd.source_session.unwrap_or(*session);
            match cmd.operation {
                SftpOperation::FileApply | SftpOperation::FileJobCancel => {
                    if !view.file_writable
                        || view.file_job.as_ref().is_none_or(|j| {
                            j.session != wire
                                || j.id.as_uuid().as_bytes().as_slice() != cmd.object.as_slice()
                                || j.revision != cmd.revision
                        })
                    {
                        return false;
                    }
                }
                SftpOperation::EditReview => {
                    if !workspace::valid_edit_review(&view,wire,cmd){return false;}
                }
                SftpOperation::EditRebase|SftpOperation::EditLaunch
                | SftpOperation::EditSave
                | SftpOperation::EditClose
                | SftpOperation::EditRebind => {
                    if !view.edit_writable
                        || view.edit.as_ref().is_none_or(|j| {
                            j.id.as_uuid().as_bytes().as_slice() != cmd.object.as_slice()
                                || j.revision != cmd.revision
                                || cmd.operation != SftpOperation::EditRebind && j.session != wire
                        })
                    {
                        return false;
                    }
                }
                SftpOperation::QueueMigrate | SftpOperation::QueueAddMany => {
                    if !view.queue_writable || view.queue_busy {
                        return false;
                    }
                }
                _ => {}
            }
            if cmd.operation == SftpOperation::EditSave {
                let Ok(save) =
                    serde_json::from_slice::<cshell_application::SftpEditSave>(&cmd.data)
                else {
                    return false;
                };
                if !view.queue_writable
                    || view.queue_busy
                    || view
                        .queue
                        .as_ref()
                        .is_none_or(|q| q.revision != save.queue_revision)
                {
                    return false;
                }
            }
        }
        let sync=if let SftpClientCommand::Sync(session,op,id,_)=&command {
            if view.sync_pending.is_some()||view.session_id!=Some(*session){return false;}Some((*id,*op))
        }else{None};
        let workspace = matches!(&command, SftpClientCommand::Workspace(..));
        let tree=if let SftpClientCommand::Tree(session,op,id,options)=&command {
            if view.tree_pending.contains(id){return false;}
            Some((*session,*op,*id,options.clone()))
        }else{None};
        let queued = self
            .sender
            .as_ref()
            .is_some_and(|sender| sender.try_send(command).is_ok());
        if queued && let Some(scope)=sync {
            view.sync_pending=Some(scope);view.sync_error=None;
            if scope.1==SftpOperation::SyncApply && let Some(preview)=view.sync_preview.clone() {
                view.trees.insert(scope.0,cshell_application::SftpTreeSnapshot{id:scope.0,session:preview.session,direction:preview.options.direction,state:cshell_application::TerminalUploadState::Preparing,completed:0,total_files:0,bytes:0,total_bytes:0,bytes_per_second:0,detail:"正在核对已审阅的差异，等待同步队列确认".into(),retryable:false,label:"目录同步".into(),destination:if preview.options.direction==cshell_application::SftpQueueDirection::Upload{preview.options.remote.clone()}else{preview.options.local.clone()}});
            }
        }
        if queued && let Some((session,profile,previous))=lifecycle_action {
            if let Some(profile)=profile {
                reset_scope(&mut view,session);
                view.connection_profile=Some(profile);
                view.connection_previous=previous;
                view.connection_info=None;
                view.connection_phase=ConnectionPhase::Connecting;
                view.close_requested=false;
                view.connection_cancelled=false;
                view.error=None;
                view.status="正在连接 SFTP…".into();
            } else {view.connection_phase=ConnectionPhase::Disconnecting;view.status="正在断开 SFTP…".into();}
            view.generation=view.generation.wrapping_add(1);
        }
        if queued && let Some(held) = held {
            view.queue_hold = held;
        }
        if queued && (mutation.is_some() || held.is_some()) {
            view.queue_busy = true;
            if let Some(item) = resumed {
                view.paused_items.remove(&item);
                view.stopping_items.remove(&item);
            }
        }
        if queued && workspace {
            view.workspace_busy = true;
        }
        if queued && let Some((session,op,id,options))=tree {
            view.tree_pending.insert(id);
            if op==SftpOperation::TreeStart && let Some(options)=options {
                view.trees.insert(id,cshell_application::SftpTreeSnapshot{id,session,direction:options.direction,state:cshell_application::TerminalUploadState::Preparing,completed:0,total_files:0,bytes:0,total_bytes:0,bytes_per_second:0,detail:"正在提交目录任务…".into(),retryable:false,label:"目录任务".into(),destination:options.destination.clone()});
            }
        }
        queued
    }
}

impl Drop for DesktopSftpConnection {
    fn drop(&mut self) {
        let _ = self.shutdown.send(true);
        self.sender.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

async fn run(
    config: DesktopConnectionConfig,
    shared: Arc<Mutex<SftpView>>,
    mut receiver: tokio::sync::mpsc::Receiver<SftpClientCommand>,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    let mut poll = tokio::time::interval(std::time::Duration::from_millis(500));
    loop {
        tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() { break; }
            }
            _ = poll.tick() => {
                tokio::select! { _ = lifecycle::poll(&config,&shared) => {}, _ = shutdown.changed() => break }
                if {let view=shared.lock().unwrap_or_else(std::sync::PoisonError::into_inner);view.connection_profile.is_some() && view.connection_phase!=ConnectionPhase::Connected} {continue;}

                tokio::select! { _ = workspace::poll(&config, &shared) => {}, _ = shutdown.changed() => break }
                tokio::select! { _ = tree::poll(&config, &shared) => {}, _ = shutdown.changed() => break }
                let queued = {
                    let view = shared.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                    view.queue.as_ref().filter(|q| !view.queue_busy && (!q.paused || q.items.iter().any(|i| i.state == cshell_application::SftpQueueState::Running))).map(|q| q.session_id)
                };
                if let Some(session_id) = queued {
                    let query = request(SftpOperation::QueueGet, session_id, String::new(), String::new(), Vec::new());
                    let result = tokio::select! { result = send(&config, query) => result, _ = shutdown.changed() => break };
                    publish_queue(&shared, session_id, result, false, None);
                }
                tokio::select! { _ = transfer_controls::poll(&config, &shared) => {}, _ = shutdown.changed() => break }
                let active = {
                    let view = shared.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                    view.session_id.zip(view.transfer.as_ref().filter(|transfer| {
                        transfer.state == SftpTransferState::Running as i32
                    }).map(|transfer| transfer.transfer_id.clone()))
                };
                if let Some((session_id, transfer_id)) = active {
                    let request = request(SftpOperation::TransferStatus, session_id, String::new(), String::new(), transfer_id);
                    let result = tokio::select! {
                        result = send(&config, request) => result,
                        _ = shutdown.changed() => break,
                    };
                    publish(&shared, session_id, result, false);
                }
            }
            command = receiver.recv() => {
                let Some(command) = command else { break };
                let scope=command.session();
                if let SftpClientCommand::Sync(session,operation,id,data)=command {
                    tokio::spawn(sync::run(config.clone(),shared.clone(),session,operation,id,data));continue;
                }
                if let SftpClientCommand::Tree(session,operation,id,options)=command {
                    tokio::select!{_=tree::run(&config,&shared,session,operation,id,options)=>{},_=shutdown.changed()=>break};continue;
                }
                if {let view=shared.lock().unwrap_or_else(std::sync::PoisonError::into_inner);view.connection_profile.is_some() && view.session_id!=Some(scope)} {continue;}

                if let SftpClientCommand::LiveQueue(session, pause) = command {
                    tokio::select!{_=tree::pause_all(&config,&shared,session,pause)=>{},_=shutdown.changed()=>break}
                    tokio::select! { _ = transfer_controls::queue_control(&config, &shared, session, pause) => {}, _ = shutdown.changed() => break }
                    continue;
                }
                if let SftpClientCommand::ResumeQueueItem(session, item, revision) = command {
                    tokio::select! { _ = transfer_controls::resume(&config, &shared, session, item, revision) => {}, _ = shutdown.changed() => break }
                    continue;
                }
                if matches!(&command,SftpClientCommand::ConnectProfile{..}|SftpClientCommand::Disconnect(..)) {
                    tokio::select! { _ = lifecycle::run(&config,&shared,command) => {}, _ = shutdown.changed() => break }
                    continue;
                }
                if let SftpClientCommand::OpenBrowser(session) = command {
                    tokio::select! { _ = lifecycle::load_browser(&config,&shared,session) => {}, _ = shutdown.changed() => break }
                    continue;
                }
                if let SftpClientCommand::QueueBatch(session, action, items, revision) = command {
                    tokio::select! { _ = queue_batch::run(&config, &shared, session, action, items, revision) => {}, _ = shutdown.changed() => break }
                    continue;
                }
                if let SftpClientCommand::TransferBatch(session, drafts, revision, start) = command {
                    tokio::select! { _ = transfer_controls::batch(&config, &shared, session, drafts, revision, start) => {}, _ = shutdown.changed() => break }
                    continue;
                }
                if let SftpClientCommand::StopQueueItem(session, item, revision, pause) = command {
                    tokio::select! { _ = transfer_controls::stop(&config, &shared, session, item, revision, pause) => {}, _ = shutdown.changed() => break }
                    continue;
                }
                if let SftpClientCommand::Workspace(session, command) = command {
                    tokio::select! { _ = workspace::run_command(&config, &shared, session, command) => {}, _ = shutdown.changed() => break }
                    continue;
                }
                if let SftpClientCommand::TransferOne(session, draft, revision) = command {
                    tokio::select! { _ = workspace::transfer_one(&config,&shared,session,draft,revision) => {}, _ = shutdown.changed() => break }
                    continue;
                }
                if matches!(command, SftpClientCommand::List(..) | SftpClientCommand::LocalList(..) | SftpClientCommand::BrowseNext(..) | SftpClientCommand::BrowseClose(..) | SftpClientCommand::BrowseManaged(..) | SftpClientCommand::BrowseLocal(..)) {
                    let result = tokio::select! { result = browse_command(&config, &shared, command) => result, _ = shutdown.changed() => break };
                    if let Err(error) = result { let mut view = shared.lock().unwrap_or_else(std::sync::PoisonError::into_inner); if view.session_id==Some(scope) {view.error = Some(error); view.generation = view.generation.wrapping_add(1);} }
                    continue;
                }
                let extended = matches!(command, SftpClientCommand::LocalList(..) | SftpClientCommand::QueueGet(..) | SftpClientCommand::QueueAdd(..) | SftpClientCommand::QueueControl(..) | SftpClientCommand::QueueWrite(..));
                if extended {
                    let expected = match &command { SftpClientCommand::QueueAdd(id, _, revision) | SftpClientCommand::QueueControl(id, _, revision, _) | SftpClientCommand::QueueWrite(id, _, _, revision, _) => Some((*id, *revision)), _ => None };
                    if let Some((id, revision)) = expected {
                        let mut view = shared.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                        if view.connection_profile.is_some() && view.session_id!=Some(id) {continue;}
                        if !view.queue_writable || view.session_id != Some(id) || view.queue.as_ref().is_none_or(|q| q.revision != revision) {
                            view.queue_busy = false; view.error = Some("保存状态变化或前次写入未确认；请重新加载队列".into()); continue;
                        }
                    }
                    let (session, query, reload, revision, local) = match command {
                        SftpClientCommand::LocalList(id, path) => (id, request(SftpOperation::LocalList, id, String::new(), path, Vec::new()), false, None, true),
                        SftpClientCommand::QueueGet(id) => (id, request(SftpOperation::QueueGet, id, String::new(), String::new(), Vec::new()), true, None, false),
                        SftpClientCommand::QueueAdd(id, draft, revision) => {
                            let query = SftpRequest { operation: if draft.verified { SftpOperation::QueueAddManaged } else { SftpOperation::QueueAdd } as i32, session_id: id.as_uuid().as_bytes().to_vec(),
                                document_json: serde_json::to_vec(&draft).unwrap_or_default(), expected_revision: revision, confirmed: true, ..Default::default() };
                            (id, query, false, Some(revision), false)
                        }
                        SftpClientCommand::QueueControl(id, operation, revision, item) => {
                            let query = SftpRequest { operation: operation as i32, session_id: id.as_uuid().as_bytes().to_vec(),
                                expected_revision: revision, confirmed: true, transfer_id: item, ..Default::default() };
                            (id, query, false, Some(revision), false)
                        }
                        SftpClientCommand::QueueWrite(id, operation, document, revision, item) => (id, SftpRequest { operation: operation as i32, session_id: id.as_uuid().as_bytes().to_vec(), document_json: document, expected_revision: revision, confirmed: true, transfer_id: item, ..Default::default() }, false, Some(revision), false),
                        _ => unreachable!("extended command"),
                    };
                    let result = tokio::select! { result = send(&config, query) => result, _ = shutdown.changed() => break };
                    if local { publish_local(&shared, session, result); } else { publish_queue(&shared, session, result, reload, revision); }
                    continue;
                }
                let (session_id, request, listing) = match command {
                    SftpClientCommand::List(id, path) => (id, request(SftpOperation::List, id, path, String::new(), Vec::new()), true),
                    SftpClientCommand::Recover(id) => (id, request(SftpOperation::CurrentTransfer, id, String::new(), String::new(), Vec::new()), false),
                    SftpClientCommand::Upload(id, local, remote) => (id, request(SftpOperation::Upload, id, remote, local, Vec::new()), false),
                    SftpClientCommand::Download(id, remote, local) => (id, request(SftpOperation::Download, id, remote, local, Vec::new()), false),
                    SftpClientCommand::Cancel(id, transfer_id) => (id, request(SftpOperation::CancelTransfer, id, String::new(), String::new(), transfer_id), false),
                    _ => unreachable!("extended commands handled above"),
                };
                let result = tokio::select! {
                    result = send(&config, request) => result,
                    _ = shutdown.changed() => break,
                };
                publish(&shared, session_id, result, listing);
            }
        }
    }
    lifecycle::shutdown(&config,&shared).await;
}

fn request(
    operation: SftpOperation,
    session_id: SessionId,
    remote_path: String,
    local_path: String,
    transfer_id: Vec<u8>,
) -> SftpRequest {
    SftpRequest {
        operation: operation as i32,
        session_id: session_id.as_uuid().as_bytes().to_vec(),
        remote_path,
        local_path,
        max_entries: if matches!(operation, SftpOperation::List | SftpOperation::LocalList) {
            512
        } else {
            0
        },
        transfer_id,
        ..Default::default()
    }
}

fn publish(
    shared: &Arc<Mutex<SftpView>>,
    session_id: SessionId,
    result: Result<SftpResponse, String>,
    listing: bool,
) {
    let mut view = shared
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if view.connection_profile.is_some() && view.session_id!=Some(session_id) {return;}
    view.generation = view.generation.wrapping_add(1);
    match result {
        Ok(response) if response.status == SftpStatus::Ok as i32 => {
            reset_scope(&mut view, session_id);
            view.error = None;
            if listing {
                view.entries = response.entries;
                view.truncated = response.truncated;
                view.status = format!("已列出 {} 项", view.entries.len());
            }
            if let Some(transfer) = response.transfer {
                view.status = match SftpTransferState::try_from(transfer.state) {
                    Ok(SftpTransferState::Running) => "传输中".into(),
                    Ok(SftpTransferState::Succeeded) => "传输完成".into(),
                    Ok(SftpTransferState::Cancelled) => "传输已取消".into(),
                    _ if transfer.destination_committed => "目标已提交，清理失败".into(),
                    _ => "传输失败".into(),
                };
                if transfer.state == SftpTransferState::Failed as i32 {
                    view.error = Some(transfer.detail.clone());
                }
                view.transfer = Some(transfer);
            }
        }
        Ok(response) => {
            if listing {
                view.entries.clear();
                view.truncated = false;
            }
            view.error = Some(response.detail);
        }
        Err(error) => {
            if listing {
                view.entries.clear();
                view.truncated = false;
            }
            view.error = Some(error);
        }
    }
}
fn reset_scope(view: &mut SftpView, session: SessionId) {
    if view.session_id!=Some(session){view.sync_pending=None;view.sync_preview=None;view.sync_error=None;}
    if view.session_id != Some(session) {
        view.session_id = Some(session);
        view.entries.clear();
        view.trees.clear();view.tree_pending.clear();
        view.local_entries.clear();
        view.local_directory = None;
        view.local_cursor = None;
        view.remote_cursor = None;
        view.remote_loaded_directory = None;
        view.remote_requested_directory = None;
        view.local_truncated = false;
        view.truncated = false;
        view.transfer = None;
        view.queue_transfers.clear();
        view.paused_items.clear();
        view.queue_hold = false;
        view.stopping_items.clear();
        view.queue = None;
        view.queue_writable = false;
        view.queue_busy = false;
        view.peer = None;
        view.batch_status = None;
        view.migration_source = None;
        view.file_job = None;
        view.edit = None;
        view.edit_review = None;
        view.connection_trace = None;
        view.edit_text = None;
        view.history.clear();
        view.history_offset = 0;
        view.history_more = false;
        view.file_writable = false;
        view.edit_writable = false;
        view.workspace_busy = false;
        view.workspace_epoch = view.workspace_epoch.wrapping_add(1);
    }
}
fn publish_queue(
    shared: &Arc<Mutex<SftpView>>,
    session: SessionId,
    result: Result<SftpResponse, String>,
    reload: bool,
    revision: Option<u64>,
) {
    let result = result.and_then(|reply| {
        if reply.status != SftpStatus::Ok as i32
            || !reply.entries.is_empty()
            || reply.transfer.is_some()
        {
            return Err("队列被拒绝；请重新载入保存状态".into());
        }
        let queue = decode_sftp_queue(&reply.document_json).map_err(|_| "队列回复无效")?;
        if queue.session_id != session
            || revision.is_some_and(|r| r.checked_add(1) != Some(queue.revision))
        {
            return Err("队列会话或版本不匹配".into());
        }
        Ok(queue)
    });
    let mut view = shared
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if view.connection_profile.is_some() && view.session_id!=Some(session) {return;}
    reset_scope(&mut view, session);
    if revision.is_some() || reload {
        view.queue_busy = false;
    }
    view.generation = view.generation.wrapping_add(1);
    match result {
        Ok(queue)
            if view
                .queue
                .as_ref()
                .is_none_or(|old| queue.revision >= old.revision) =>
        {
            view.queue = Some(Arc::new(queue));
            if reload {
                view.queue_writable = true;
            }
            view.error = None;
            view.status = "持久队列状态已载入".into();
        }
        _ => {
            if revision.is_some() {
                view.queue_writable = false;
            }
            view.error =
                Some("队列版本变化或请求结果不明；明确重新加载后再修改，不自动重发".into());
        }
    }
}
fn publish_local(
    shared: &Arc<Mutex<SftpView>>,
    session: SessionId,
    result: Result<SftpResponse, String>,
) {
    let mut view = shared
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if view.connection_profile.is_some() && view.session_id!=Some(session) {return;}
    reset_scope(&mut view, session);
    view.generation = view.generation.wrapping_add(1);
    match result {
        Ok(reply)
            if reply.status == SftpStatus::Ok as i32
                && reply.entries.len() <= 512
                && reply.transfer.is_none() =>
        {
            match serde_json::from_slice::<String>(&reply.document_json) {
                Ok(path)
                    if std::path::Path::new(&path).is_absolute()
                        && path.len() <= 4096
                        && reply.entries.iter().all(|e| {
                            e.path.len() <= 4096
                                && !e.path.contains('\0')
                                && std::path::Path::new(&e.path).parent()
                                    == Some(std::path::Path::new(&path))
                        }) =>
                {
                    view.local_directory = Some(path);
                    view.local_entries = reply.entries;
                    view.local_truncated = reply.truncated;
                    view.error = None;
                }
                _ => {
                    view.local_entries.clear();
                    view.error = Some("本机目录回复不匹配".into());
                }
            }
        }
        _ => {
            view.local_entries.clear();
            view.error = Some("本机目录不可用或请求失败".into());
        }
    }
}

async fn browse_command(
    config: &DesktopConnectionConfig,
    shared: &Arc<Mutex<SftpView>>,
    command: SftpClientCommand,
) -> Result<(), String> {
    let managed_options = if let SftpClientCommand::BrowseManaged(_, options) = &command {
        Some(options.clone())
    } else {
        None
    };
    let follow=match &command {SftpClientCommand::BrowseManaged(_,options)|SftpClientCommand::BrowseLocal(_,options)=>options.links==SftpLinkPolicy::Follow,_=>false};
    let (session, local, operation, mut path, expected) = match command {
        SftpClientCommand::BrowseManaged(id, options) => (
            id,
            options.local,
            SftpOperation::BrowseManaged,
            options.path,
            None,
        ),
        SftpClientCommand::BrowseLocal(id,options)=>(id,true,SftpOperation::LocalBrowse,options.path,None),
        SftpClientCommand::List(id, path) => (id, false, SftpOperation::RemoteBrowse, path, None),
        SftpClientCommand::LocalList(id, path) => {
            (id, true, SftpOperation::LocalBrowse, path, None)
        }
        SftpClientCommand::BrowseNext(id, local, cursor) => (
            id,
            local,
            SftpOperation::BrowseNext,
            cursor.directory.clone(),
            Some(cursor),
        ),
        SftpClientCommand::BrowseClose(id, local, cursor) => (
            id,
            local,
            SftpOperation::BrowseClose,
            cursor.directory.clone(),
            Some(cursor),
        ),
        _ => return Err("无效目录操作".into()),
    };
    if operation==SftpOperation::LocalBrowse && follow && !path.is_empty() {
        path=tokio::fs::canonicalize(&path).await.map_err(|_|"无法解析本地目录链接")?.to_str().ok_or("本地目录路径无法读取")?.to_owned();
    }
    let release = {
        let mut view = shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if view.connection_profile.is_some() && view.session_id!=Some(session) {return Ok(());}
        if let Some(expected) = &expected {
            let current = if local {
                &view.local_cursor
            } else {
                &view.remote_cursor
            };
            if view.session_id != Some(session)
                || current.as_ref().is_none_or(|c| {
                    c.id != expected.id || c.revision != expected.revision || c.directory != path
                })
            {
                return Err("目录版本已变化；请重新列出".into());
            }
        }
        let mut release = Vec::new();
        if let Some(previous) = view.session_id {
            if previous != session {
                if let Some(c) = view.local_cursor.take() {
                    release.push((previous, c));
                }
                if let Some(c) = view.remote_cursor.take() {
                    release.push((previous, c));
                }
            } else if expected.is_none() {
                let current = if local {
                    &mut view.local_cursor
                } else {
                    &mut view.remote_cursor
                };
                if let Some(c) = current.take() {
                    release.push((previous, c));
                }
            }
        }
        reset_scope(&mut view, session);
        if !local && expected.is_none() {view.remote_requested_directory=Some(path.clone());}
        if local {
            view.local_cursor = None;
            view.local_entries.clear();
            view.local_truncated = false;
            view.local_directory = None;
        } else {
            view.remote_cursor = None;
            view.entries.clear();
            view.truncated = false;
            view.remote_loaded_directory = None;
        }
        view.generation = view.generation.wrapping_add(1);
        release
    };
    for (previous, c) in release {
        let query = SftpRequest {
            operation: SftpOperation::BrowseClose as i32,
            session_id: previous.as_uuid().as_bytes().to_vec(),
            transfer_id: c.id,
            expected_revision: c.revision,
            ..Default::default()
        };
        let _ = send(config, query).await;
    }
    let query = SftpRequest {
        operation: operation as i32,
        session_id: session.as_uuid().as_bytes().to_vec(),
        remote_path: if operation == SftpOperation::RemoteBrowse {
            path.clone()
        } else {
            String::new()
        },
        local_path: if operation == SftpOperation::LocalBrowse {
            path.clone()
        } else {
            String::new()
        },
        max_entries: if operation == SftpOperation::BrowseClose {
            0
        } else {
            512
        },
        document_json: managed_options
            .as_ref()
            .map(|o| serde_json::to_vec(o).unwrap_or_default())
            .unwrap_or_default(),
        transfer_id: expected.as_ref().map(|c| c.id.clone()).unwrap_or_default(),
        expected_revision: expected.as_ref().map_or(0, |c| c.revision),
        ..Default::default()
    };
    let reply = send(config, query).await?;
    if reply.status != SftpStatus::Ok as i32
        || reply.entries.len() > 512
        || reply.transfer.is_some()
        || !reply.document_json.is_empty()
        || reply.browse_id.len() != 16
        || reply.browse_id.iter().all(|b| *b == 0)
        || reply.browse_local != local
        || reply.browse_revision
            != expected
                .as_ref()
                .map_or(Some(1), |c| c.revision.checked_add(1))
                .ok_or("目录版本溢出")?
        || expected.as_ref().is_some_and(|c| c.id != reply.browse_id)
        || !follow && !path.is_empty() && reply.browse_directory != path
        || reply.browse_directory.len() > 4096
        || reply.browse_directory.chars().any(char::is_control)
        || local && !std::path::Path::new(&reply.browse_directory).is_absolute()
        || !local && !reply.browse_directory.starts_with('/')
        || operation == SftpOperation::BrowseClose && (!reply.entries.is_empty() || reply.truncated)
        || !reply.entries.iter().all(|e| {
            !e.name.is_empty()
                && e.name != "."
                && e.name != ".."
                && !e.name.contains(['/', '\\'])
                && !e.name.chars().any(char::is_control)
                && e.name.len() <= 4096
                && e.path.len() <= 4096
                && !e.path.chars().any(char::is_control)
                && (1..=4).contains(&e.kind)
                && if local {
                    std::path::Path::new(&e.path).parent()
                        == Some(std::path::Path::new(&reply.browse_directory))
                } else {
                    e.path
                        == format!(
                            "{}/{}",
                            reply.browse_directory.trim_end_matches('/'),
                            e.name
                        )
                }
        })
    {
        return Err("目录回复无效或版本不匹配；请重新列出".into());
    }
    let mut view = shared
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if view.session_id != Some(session) {
        return Err("目录会话已变化".into());
    }
    let closed = operation == SftpOperation::BrowseClose;
    let cursor = reply.truncated.then_some(BrowseHandle {
        id: reply.browse_id,
        revision: reply.browse_revision,
        directory: reply.browse_directory.clone(),
    });
    if local {
        view.local_entries = reply.entries;
        view.local_truncated = reply.truncated;
        view.local_cursor = cursor;
        view.local_directory = (!closed).then_some(reply.browse_directory);
    } else {
        view.entries = reply.entries;
        view.truncated = reply.truncated;
        view.remote_cursor = cursor;
        view.remote_loaded_directory = (!closed).then_some(reply.browse_directory);
    }
    view.error = None;
    view.status = if closed {
        "目录浏览已关闭".into()
    } else if reply.truncated {
        "当前页已载入；可继续下一页（游标两分钟内有效）".into()
    } else {
        "当前页已载入；已读到目录末尾".into()
    };
    view.generation = view.generation.wrapping_add(1);
    Ok(())
}

pub(crate) async fn send(
    config: &DesktopConnectionConfig,
    request: SftpRequest,
) -> Result<SftpResponse, String> {
    let seconds=if matches!(SftpOperation::try_from(request.operation),Ok(SftpOperation::EditReview|SftpOperation::EditRebase)){40} else if matches!(SftpOperation::try_from(request.operation),Ok(SftpOperation::SyncScan|SftpOperation::SyncApply)){70} else if request.operation==SftpOperation::ConnectProfile as i32 {330} else if request.operation>=SftpOperation::TerminalUploadStart as i32 {5} else {40};
    tokio::time::timeout(
        std::time::Duration::from_secs(seconds),
        send_inner(config, request),
    )
    .await
    .map_err(|_| "SFTP request timed out; check transfer status before retrying".to_owned())?
}

async fn send_inner(
    config: &DesktopConnectionConfig,
    mut request: SftpRequest,
) -> Result<SftpResponse, String> {
    let extended = request.operation >= SftpOperation::LocalList as i32;
    let browsing = (SftpOperation::RemoteBrowse as i32..=SftpOperation::BrowseClose as i32)
        .contains(&request.operation)
        || request.operation == SftpOperation::BrowseManaged as i32;
    let workspace = request.operation >= SftpOperation::QueueEdit as i32;
    let verified = request.operation >= SftpOperation::QueueAddVerified as i32;
    let resolved = config.resolve().map_err(|error| error.to_string())?;
    let mut stream = transport::connect(&resolved.endpoint)
        .await
        .map_err(|error| error.to_string())?;
    let mut handshake = Handshake::new(
        resolved.daemon_instance_id.to_vec(),
        resolved.instance_token.to_vec(),
    );
    handshake.feature_bits = features::VAULT_CONTROL | features::SSH_PROFILE_SESSION | features::SFTP_CONTROL
        | features::SFTP_LIVE_CONTROL
        | if workspace {
            features::SFTP_WORKSPACE
        } else {
            0
        }
        | if verified {
            features::SFTP_VERIFIED_QUEUE
        } else {
            0
        }
        | if browsing {
            features::SFTP_DIRECTORY_CURSOR
        } else {
            0
        }
        | if extended {
            features::SFTP_BROWSER_QUEUE
        } else {
            0
        };
    let negotiated = client_handshake(&mut stream, 1, handshake)
        .await
        .map_err(|error| error.to_string())?;
    if negotiated.feature_bits & features::SFTP_CONTROL == 0 {
        return Err("daemon does not support SFTP control; restart the daemon".into());
    }
    if extended && negotiated.feature_bits & features::SFTP_BROWSER_QUEUE == 0 {
        return Err("daemon 不支持双栏浏览或持久队列".into());
    }
    if browsing && negotiated.feature_bits & features::SFTP_DIRECTORY_CURSOR == 0 {
        return Err("daemon 不支持完整目录分页；需要升级并重启 daemon".into());
    }
    if verified && negotiated.feature_bits & features::SFTP_VERIFIED_QUEUE == 0 {
        return Err("daemon 不支持会话校验和显式恢复；需要升级并重启 daemon".into());
    }
    if negotiated.protocol_minor<65 && request.document_json.windows(b"\"verification\"".len()).any(|part|part==b"\"verification\"") {
        return Err("请一起更新 GUI 和后台，以支持新的 SFTP 传输校验策略".into());
    }
    if workspace && negotiated.feature_bits & features::SFTP_WORKSPACE == 0 {
        return Err("daemon 不支持完整 SFTP 工作区；需要升级并重启 daemon".into());
    }
    if (request.operation >= SftpOperation::PauseTransfer as i32
        || request.operation == SftpOperation::QueuePause as i32
        || request.operation == SftpOperation::QueueStartManaged as i32)
        && negotiated.feature_bits & features::SFTP_LIVE_CONTROL == 0
    {
        return Err("当前后台尚未支持实时暂停，请更新后台后重试。".into());
    }
    if request.operation == SftpOperation::QueueRetryMany as i32 && negotiated.protocol_minor < 59 {
        return Err("当前后台不支持失败重试，请更新后台后重试。".into());
    }
    if request.operation >= SftpOperation::ConnectProfile as i32 && negotiated.protocol_minor < 60 {
        return Err("当前后台不支持独立 SFTP，请更新后台后重试。".into());
    }
    if request.operation>=SftpOperation::SyncScan as i32 && negotiated.protocol_minor<76 {return Err("请一起更新 GUI 与后台，以支持目录比较和同步".into());}
    if request.operation>=SftpOperation::TreeStart as i32 && negotiated.protocol_minor<73 {return Err("当前后台不支持目录任务，请一起更新 GUI 与后台".into());}
    if request.operation >= SftpOperation::TerminalUploadStart as i32 && negotiated.protocol_minor < 69 {
        return Err("请一起更新 GUI 和后台，以支持 SSH 终端拖放上传".into());
    }
    if request.operation==SftpOperation::ConnectProfile as i32 && negotiated.protocol_minor<64
        && serde_json::from_slice::<cshell_application::SftpConnectRequest>(&request.document_json).is_ok_and(|r|r.discover_directory) {
        return Err("请一起更新 GUI 和后台，以支持 SSH 当前目录与远端主目录导航".into());
    }
    if request.operation==SftpOperation::ConnectProfile as i32 && negotiated.protocol_minor<63
        && serde_json::from_slice::<cshell_application::SftpConnectRequest>(&request.document_json).is_ok_and(|r|r.source_ssh_session.is_some()) {
        return Err("后台需要更新后才能复用 SSH 打开 SFTP".into());
    }
    request.connection_diagnostics=negotiated.protocol_minor>=77&&request.operation==SftpOperation::ConnectProfile as i32&&crate::connection_diagnostics::requested();
    write_envelope(
        &mut stream,
        &Envelope {
            request_id: 2,
            payload: Some(envelope::Payload::SftpRequest(request)),
            ..Default::default()
        },
    )
    .await
    .map_err(|error| error.to_string())?;
    let response = crate::connection_diagnostics::read_response(&mut stream,2)
        .await
        .map_err(|error| error.to_string())?;
    if response.request_id != 2 {
        return Err("daemon returned a mismatched SFTP response".into());
    }
    match response.payload {
        Some(envelope::Payload::SftpResponse(response)) => Ok(response),
        _ => Err("daemon returned an unexpected SFTP response".into()),
    }
}

#[cfg(test)]
mod host_identity_tests;
