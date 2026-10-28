use crate::daemon_connection::DesktopConnectionConfig;
use cshell_ipc::{
    Envelope, Handshake, ZmodemRequest, ZmodemResponse, client_handshake, envelope, features,
    read_envelope, transport, write_envelope,
};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

#[derive(Clone, Default)]
pub(crate) struct View {
    pub response: ZmodemResponse,
    pub error: Option<String>,
    pub generation: u64,
}
pub(crate) struct Connection {
    shared: Arc<Mutex<View>>,
    sender: tokio::sync::mpsc::Sender<ZmodemRequest>,
    stop: tokio::sync::watch::Sender<bool>,
    worker: Option<std::thread::JoinHandle<()>>,
}
impl Connection {
    #[cfg(test)]
    pub fn preview(transfers:Vec<cshell_ipc::ZmodemTransfer>)->Self {
        let (sender,_receiver)=tokio::sync::mpsc::channel(1);
        let (stop,_stopped)=tokio::sync::watch::channel(false);
        Self{shared:Arc::new(Mutex::new(View{response:ZmodemResponse{transfers,error:String::new()},..Default::default()})),sender,stop,worker:None}
    }

    pub fn start(config: DesktopConnectionConfig) -> std::io::Result<Self> {
        let shared = Arc::new(Mutex::new(View::default()));
        let view = shared.clone();
        let (sender, mut receiver) = tokio::sync::mpsc::channel::<ZmodemRequest>(16);
        let (stop, mut stopped) = tokio::sync::watch::channel(false);
        let worker=std::thread::Builder::new().name("cshell-zmodem-ipc".into()).spawn(move||{
            let Ok(runtime)=tokio::runtime::Builder::new_current_thread().enable_all().build() else {return;};
            runtime.block_on(async{
                let mut control_error=None;
                let mut interval=tokio::time::interval(Duration::from_millis(300));
                interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                loop {
                    let command=tokio::select!{biased;_=stopped.changed()=>break,cmd=receiver.recv()=>match cmd{Some(c)=>c,None=>break},_=interval.tick()=>ZmodemRequest::default()};
                    let result=tokio::select!{_=stopped.changed()=>break,result=send(&config,&command)=>result};
                    let mut view=view.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                    match result {
                        Ok(response)=>{
                            if command.operation!=0{control_error=(!response.error.is_empty()).then(||response.error.clone());}
                            if view.response!=response || view.error!=control_error {
                                view.error=control_error.clone();view.response=response;view.generation=view.generation.wrapping_add(1);
                            }
                        }
                        Err(error)=>{if view.error.as_ref()!=Some(&error){view.error=Some(error);view.generation=view.generation.wrapping_add(1);}}
                    }
                }
            });
        })?;
        Ok(Self {
            shared,
            sender,
            stop,
            worker: Some(worker),
        })
    }
    pub fn view(&self) -> View {
        self.shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
    pub fn request(&self, request: ZmodemRequest) -> bool {
        self.sender.try_send(request).is_ok()
    }
}
impl Drop for Connection {
    fn drop(&mut self) {
        let _ = self.stop.send(true);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
async fn send(
    config: &DesktopConnectionConfig,
    request: &ZmodemRequest,
) -> Result<ZmodemResponse, String> {
    let resolved = config.resolve().map_err(|e| e.to_string())?;
    tokio::time::timeout(Duration::from_secs(5), async {
        #[cfg(windows)]
        let mut stream = transport::connect(&resolved.endpoint)
            .await
            .map_err(|_| "后台连接失败".to_owned())?;
        #[cfg(unix)]
        let mut stream = transport::connect(std::path::Path::new(&resolved.endpoint))
            .await
            .map_err(|_| "后台连接失败".to_owned())?;
        let mut hello = Handshake::new(
            resolved.daemon_instance_id.to_vec(),
            resolved.instance_token.to_vec(),
        );
        hello.feature_bits = features::ZMODEM;
        let negotiated = client_handshake(&mut stream, 1, hello)
            .await
            .map_err(|_| "后台认证失败".to_owned())?;
        if negotiated.feature_bits & features::ZMODEM == 0 {
            return Err("后台不支持 ZMODEM，请更新后重启".into());
        }
        write_envelope(
            &mut stream,
            &Envelope {
                request_id: 2,
                payload: Some(envelope::Payload::ZmodemRequest(request.clone())),
                ..Default::default()
            },
        )
        .await
        .map_err(|_| "ZMODEM 请求发送失败".to_owned())?;
        let reply = read_envelope(&mut stream)
            .await
            .map_err(|_| "ZMODEM 回复读取失败".to_owned())?;
        if reply.request_id != 2 {
            return Err("ZMODEM 回复身份不匹配".into());
        }
        match reply.payload {
            Some(envelope::Payload::ZmodemResponse(response)) => Ok(response),
            _ => Err("ZMODEM 回复格式错误".into()),
        }
    })
    .await
    .map_err(|_| "ZMODEM 后台请求超时，请检查传输状态".to_owned())?
}
