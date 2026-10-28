//! Optional attempt-scoped observations. No prompts, secrets or peer banners.
use cshell_domain::ConnectionStage;
use std::{future::Future, sync::Arc};
#[derive(Clone, Debug)]
pub enum Event {
    Stage(ConnectionStage),
    At {
        stage: ConnectionStage,
        endpoint: String,
    },
    Peer {
        hop: u8,
        endpoint: String,
    },
}
#[derive(Clone)]
pub struct Observer(Arc<dyn Fn(Event) + Send + Sync>);
impl std::fmt::Debug for Observer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ConnectionObserver")
    }
}
impl Observer {
    pub fn new(callback: impl Fn(Event) + Send + Sync + 'static) -> Self {
        Self(Arc::new(callback))
    }
    pub fn stage(&self, stage: ConnectionStage) {
        (self.0)(Event::Stage(stage));
    }
}
tokio::task_local! {static CURRENT:Observer;}
pub async fn scope<T>(observer: Observer, work: impl Future<Output = T>) -> T {
    CURRENT.scope(observer, work).await
}
pub fn current() -> Option<Observer> {
    CURRENT.try_with(Clone::clone).ok()
}
pub fn stage(stage: ConnectionStage) {
    let _ = CURRENT.try_with(|o| o.stage(stage));
}
pub fn peer(hop: u8, host: &str, port: u16) {
    let _ = CURRENT.try_with(|o| {
        (o.0)(Event::Peer {
            hop,
            endpoint: endpoint(host, port),
        })
    });
}
pub fn at(stage: ConnectionStage, host: &str, port: u16) {
    let _ = CURRENT.try_with(|o| {
        (o.0)(Event::At {
            stage,
            endpoint: endpoint(host, port),
        })
    });
}
fn endpoint(host: &str, port: u16) -> String {
    if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}
