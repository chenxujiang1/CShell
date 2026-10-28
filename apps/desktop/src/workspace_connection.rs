//! Serialized autosave with an explicit read barrier and reviewed recovery.
use crate::daemon_connection::DesktopConnectionConfig;
use cshell_application::{
    WorkspaceRestorePlan, WorkspaceSnapshot, decode_workspace, encode_workspace,
};
use cshell_domain::WorkspaceDocument;
use cshell_ipc::{
    Envelope, Handshake, WorkspaceOperation, WorkspaceRequest, WorkspaceResponse, WorkspaceStatus,
    client_handshake, envelope, features, read_envelope, transport, write_envelope,
};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
#[derive(Clone, Debug)]
pub struct RestoreReceipt {
    pub plan: Arc<WorkspaceRestorePlan>,
    pub snapshot: WorkspaceSnapshot,
    pub epoch: u64,
}
#[derive(Clone, Debug, Default)]
pub struct WorkspaceView {
    pub ready: bool,
    pub snapshot: Option<WorkspaceSnapshot>,
    pub error: Option<String>,
    pub status: String,
    pub generation: u64,
    pub busy: bool,
    pub paused: bool,
    pub blocked: bool,
    pub restored: Option<Arc<RestoreReceipt>>,
}
#[derive(Debug)]
enum Control {
    Reload,
    Restore(Arc<WorkspaceRestorePlan>, u64),
}
#[derive(Debug)]
pub struct WorkspaceConnection {
    shared: Arc<Mutex<WorkspaceView>>,
    sender: Option<tokio::sync::watch::Sender<Option<WorkspaceDocument>>>,
    controls: tokio::sync::mpsc::Sender<Control>,
    stop: tokio::sync::watch::Sender<bool>,
    worker: Option<std::thread::JoinHandle<()>>,
}
impl WorkspaceConnection {
    #[cfg(test)]
    pub(crate) fn paused_for_test() -> Self {
        Self {
            shared: Arc::new(Mutex::new(WorkspaceView { ready: true, paused: true, ..Default::default() })),
            sender: None,
            controls: tokio::sync::mpsc::channel(1).0,
            stop: tokio::sync::watch::channel(false).0,
            worker: None,
        }
    }

    pub fn start(config: DesktopConnectionConfig) -> Result<Self, std::io::Error> {
        let shared = Arc::new(Mutex::new(WorkspaceView {
            status: "Loading workspace".into(),
            ..Default::default()
        }));
        let state = shared.clone();
        let (sender, mut saves) = tokio::sync::watch::channel::<Option<WorkspaceDocument>>(None);
        let (controls, mut commands) = tokio::sync::mpsc::channel::<Control>(2);
        let (stop, mut stopped) = tokio::sync::watch::channel(false);
        let worker=std::thread::Builder::new().name("cshell-workspace-ipc".into()).spawn(move||{
            let runtime=match tokio::runtime::Builder::new_current_thread().enable_all().build(){Ok(r)=>r,Err(_)=>{let mut view=state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);view.ready=true;view.blocked=true;view.error=Some("Workspace worker unavailable".into());return;}};
            runtime.block_on(async{
                if config.resolve().is_err(){let _=config.start_companion_daemon();}
                let mut last_start=std::time::Instant::now();
                let loaded=loop{
                    let result=tokio::select!{biased;_=stopped.changed()=>return,result=send(&config,WorkspaceRequest::default())=>result};
                    if result.is_ok(){break result.and_then(decode_response);}
                    if last_start.elapsed()>=Duration::from_secs(5){let _=config.start_companion_daemon();last_start=std::time::Instant::now();}
                    state.lock().unwrap_or_else(std::sync::PoisonError::into_inner).status="Waiting for workspace storage".into();
                    tokio::select!{biased;_=stopped.changed()=>return,_=tokio::time::sleep(Duration::from_secs(1))=>{}}
                };
                let mut revision=0;
                {let mut view=state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);view.ready=true;view.generation=view.generation.wrapping_add(1);match loaded{
                    Ok(snapshot)=>{revision=snapshot.as_ref().map_or(0,|s|s.revision);view.status="工作区已就绪；启动时不恢复上次标签".into();view.snapshot=snapshot;},
                    Err(error)=>{view.blocked=true;view.paused=true;view.error=Some(error);}
                }}
                loop{
                    enum Work {Control(Control),Save(WorkspaceDocument)}
                    let work=tokio::select!{biased;_=stopped.changed()=>break,
                        command=commands.recv()=>match command{Some(c)=>Work::Control(c),None=>break},
                        changed=saves.changed()=>{if changed.is_err(){break;}let document=saves.borrow_and_update().clone();let Some(document)=document else{continue;};Work::Save(document)}
                    };
                    match work {
                        Work::Control(Control::Reload)=>{
                            // An in-flight autosave has completed before this barrier.
                            let _=saves.borrow_and_update();
                            let result=tokio::select!{biased;_=stopped.changed()=>break,r=send(&config,WorkspaceRequest::default())=>r.and_then(decode_response)};
                            let mut view=state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);view.busy=false;view.paused=true;view.restored=None;view.generation=view.generation.wrapping_add(1);
                            match result{Ok(snapshot)=>{revision=snapshot.as_ref().map_or(0,|s|s.revision);view.snapshot=snapshot;view.blocked=false;view.error=None;view.status="Autosave paused; review a layout before resuming".into();},Err(error)=>{view.blocked=true;view.error=Some(error);}}
                        },
                        Work::Control(Control::Restore(plan,epoch))=>{
                            let _=saves.borrow_and_update();
                            let prepared=plan.validate().map_err(|e|e.to_string()).and_then(|_|if plan.storage_revision==revision {encode_workspace(&plan.target).map_err(|e|e.to_string())}else{Err("Workspace revision changed; read and preview again".into())});
                            let result=match prepared{Ok(document_json)=>tokio::select!{biased;_=stopped.changed()=>break,r=send(&config,WorkspaceRequest{operation:WorkspaceOperation::Save as i32,expected_revision:revision,document_json})=>r.and_then(decode_response)},Err(error)=>Err(error)};
                            let result=result.and_then(|snapshot|{let snapshot=snapshot.ok_or("Workspace recovery returned no receipt")?;if revision.checked_add(1)!=Some(snapshot.revision)||snapshot.document!=plan.target{return Err("Workspace recovery receipt effect mismatch; outcome unknown".into());}Ok(snapshot)});
                            let mut view=state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);view.busy=false;view.paused=true;view.generation=view.generation.wrapping_add(1);
                            match result{Ok(snapshot)=>{revision=snapshot.revision;view.snapshot=Some(snapshot.clone());view.restored=Some(Arc::new(RestoreReceipt{plan,snapshot,epoch}));view.error=None;view.status="Workspace committed; awaiting local layout adoption".into();},Err(error)=>{view.blocked=true;view.restored=None;view.error=Some(format!("{error}; result may be unknown. Explicitly read before another preview."));}}
                        },
                        Work::Save(document)=>{
                            {let mut view=state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);if view.paused||view.blocked||view.snapshot.as_ref().is_some_and(|s|s.document==document){continue;}view.busy=true;}
                            let result=match encode_workspace(&document){Ok(document_json)=>tokio::select!{biased;_=stopped.changed()=>break,r=send(&config,WorkspaceRequest{operation:WorkspaceOperation::Save as i32,expected_revision:revision,document_json})=>r.and_then(decode_response)},Err(error)=>Err(error.to_string())};
                            let result=result.and_then(|snapshot|{let snapshot=snapshot.ok_or("Workspace save returned no document")?;if revision.checked_add(1)!=Some(snapshot.revision)||snapshot.document!=document{return Err("Workspace save receipt mismatch; outcome unknown".into());}Ok(snapshot)});
                            let mut view=state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);if !view.paused{view.busy=false;}view.generation=view.generation.wrapping_add(1);
                            match result{Ok(snapshot)=>{revision=snapshot.revision;view.snapshot=Some(snapshot);view.status="Workspace saved".into();view.error=None;},Err(error)=>{view.blocked=true;view.paused=true;view.busy=false;view.error=Some(format!("{error}; autosave paused. Open workspace recovery to read and preview."));}}
                        }
                    }
                }
            });
        })?;
        Ok(Self {
            shared,
            sender: Some(sender),
            controls,
            stop,
            worker: Some(worker),
        })
    }
    pub fn view(&self) -> WorkspaceView {
        self.shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
    pub fn save(&self, document: &WorkspaceDocument) {
        let view = self
            .shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if view.paused || view.blocked {
            return;
        }
        if let Some(sender) = &self.sender {
            sender.send_if_modified(|current| {
                if current.as_ref() == Some(document) {
                    return false;
                }
                *current = Some(document.clone());
                true
            });
        }
    }
    /// Gates all windows immediately; then drains an in-flight save and reads
    /// current CAS authority. Reading never changes the UI document.
    pub fn pause_reload(&self) -> bool {
        let mut view = self
            .shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if view.paused && view.busy {
            return false;
        }
        view.paused = true;
        view.busy = true;
        view.restored = None;
        view.generation = view.generation.wrapping_add(1);
        if self.controls.try_send(Control::Reload).is_err() {
            view.busy = false;
            view.blocked = true;
            view.error = Some("Workspace control queue unavailable".into());
            return false;
        }
        true
    }
    pub fn restore(&self, plan: Arc<WorkspaceRestorePlan>, epoch: u64) -> bool {
        let mut view = self
            .shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !view.paused
            || view.busy
            || view.blocked
            || view.restored.is_some()
            || view.snapshot.as_ref().map_or(0, |s| s.revision) != plan.storage_revision
        {
            return false;
        }
        if self
            .controls
            .try_send(Control::Restore(plan, epoch))
            .is_err()
        {
            return false;
        }
        view.busy = true;
        true
    }
    pub fn adopt(&self, receipt: &RestoreReceipt) -> bool {
        let mut view = self
            .shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if view.busy
            || view.blocked
            || view.restored.as_ref().is_none_or(|r| {
                r.epoch != receipt.epoch || r.plan != receipt.plan || r.snapshot != receipt.snapshot
            })
        {
            return false;
        }
        if let Some(sender) = &self.sender {
            let _ = sender.send(None);
        }
        view.restored = None;
        view.paused = false;
        view.error = None;
        view.status = "Workspace adopted; autosave resumed".into();
        view.generation = view.generation.wrapping_add(1);
        true
    }
}
impl Drop for WorkspaceConnection {
    fn drop(&mut self) {
        let _ = self.stop.send(true);
        self.sender.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn decode_response(response: WorkspaceResponse) -> Result<Option<WorkspaceSnapshot>, String> {
    if response.detail.len() > 4096
        || response.document_json.len() > cshell_application::MAX_WORKSPACE_BYTES
    {
        return Err("Workspace reply limit".into());
    }
    if response.status != WorkspaceStatus::Ok as i32 {
        return Err(response.detail);
    }
    if response.document_json.is_empty() {
        return if response.revision == 0 {
            Ok(None)
        } else {
            Err("Workspace revision has no document".into())
        };
    }
    if response.revision == 0 {
        return Err("Workspace document has no revision".into());
    }
    Ok(Some(WorkspaceSnapshot {
        revision: response.revision,
        document: decode_workspace(&response.document_json).map_err(|error| error.to_string())?,
    }))
}

async fn send(
    config: &DesktopConnectionConfig,
    request: WorkspaceRequest,
) -> Result<WorkspaceResponse, String> {
    tokio::time::timeout(Duration::from_secs(5), async {
        let resolved = config.resolve().map_err(|e| e.to_string())?;
        let mut stream = transport::connect(&resolved.endpoint)
            .await
            .map_err(|e| e.to_string())?;
        let mut handshake = Handshake::new(
            resolved.daemon_instance_id.to_vec(),
            resolved.instance_token.to_vec(),
        );
        handshake.feature_bits = features::WORKSPACE_CONTROL;
        let negotiated = client_handshake(&mut stream, 1, handshake)
            .await
            .map_err(|e| e.to_string())?;
        if negotiated.feature_bits & features::WORKSPACE_CONTROL == 0 {
            return Ok(WorkspaceResponse {
                status: WorkspaceStatus::Unsupported as i32,
                detail: "Workspace control requires the current daemon".into(),
                ..Default::default()
            });
        }
        write_envelope(
            &mut stream,
            &Envelope {
                request_id: 2,
                payload: Some(envelope::Payload::WorkspaceRequest(request)),
                ..Default::default()
            },
        )
        .await
        .map_err(|e| e.to_string())?;
        let response = read_envelope(&mut stream)
            .await
            .map_err(|e| e.to_string())?;
        if response.request_id != 2 {
            return Err("Mismatched workspace reply".into());
        }
        let Some(envelope::Payload::WorkspaceResponse(response)) = response.payload else {
            return Err("Unexpected workspace reply".into());
        };
        Ok(response)
    })
    .await
    .map_err(|_| "Workspace request timed out; result may be unknown".to_owned())?
}
