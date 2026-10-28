use crate::{daemon_connection::DesktopConnectionConfig, output_analysis_connection::send};
use cshell_application::*;
use cshell_domain::TaskRunId;
use cshell_ipc::{TaskOperation, TaskRequest};
use cshell_render::{LogPage, LogRow, LogSourceId};
use std::sync::{Arc, Mutex};
#[derive(Clone)]
pub(crate) struct MergedPage {
    pub metadata: Arc<OutputMergePage>,
    pub rendered: Arc<LogPage>,
}
#[derive(Clone, Default)]
pub(crate) struct MergeView {
    pub page: Option<MergedPage>,
    pub busy: bool,
    pub status: String,
    generation: u64,
}
struct ReadRequest {
    request: OutputMergeRequest,
    hash: [u8; 32],
    child: TaskRunId,
}
pub(crate) struct OutputMergeConnection {
    shared: Arc<Mutex<MergeView>>,
    sender: Option<tokio::sync::mpsc::Sender<ReadRequest>>,
    worker: Option<std::thread::JoinHandle<()>>,
}
impl OutputMergeConnection {
    pub fn start(config: DesktopConnectionConfig) -> std::io::Result<Self> {
        let shared = Arc::new(Mutex::new(MergeView::default()));
        let state = shared.clone();
        let (sender, mut receiver) = tokio::sync::mpsc::channel::<ReadRequest>(1);
        let worker = std::thread::Builder::new()
            .name("cshell-output-merge-ui".into())
            .spawn(move || {
                let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                else {
                    state
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .status = "无法启动合流连接".into();
                    return;
                };
                runtime.block_on(async move {
                    while let Some(request) = receiver.recv().await {
                        let result = read(&config, request).await;
                        let mut view = state
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        view.busy = false;
                        match result {
                            Ok(mut page) => {
                                view.generation = view.generation.wrapping_add(1);
                                Arc::make_mut(&mut page.rendered).revision = view.generation;
                                view.page = Some(page);
                                view.status = "合流字节页已载入".into();
                            }
                            Err(error) => view.status = format!("{error}；未自动重发请求"),
                        }
                    }
                });
            })?;
        Ok(Self {
            shared,
            sender: Some(sender),
            worker: Some(worker),
        })
    }
    pub fn view(&self) -> MergeView {
        self.shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
    pub fn read(&self, request: OutputMergeRequest, hash: [u8; 32], child: TaskRunId) -> bool {
        let mut view = self
            .shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if view.busy {
            return false;
        }
        if self.sender.as_ref().is_some_and(|sender| {
            sender
                .try_send(ReadRequest {
                    request,
                    hash,
                    child,
                })
                .is_ok()
        }) {
            view.busy = true;
            true
        } else {
            false
        }
    }
}
impl Drop for OutputMergeConnection {
    fn drop(&mut self) {
        self.sender.take();
        if let Some(worker) = self.worker.take() {
            let _joined = worker.join();
        }
    }
}
async fn read(
    config: &DesktopConnectionConfig,
    request: ReadRequest,
) -> Result<MergedPage, String> {
    request.request.validate().map_err(|_| "合流范围无效")?;
    let mut query = TaskRequest::default();
    query.operation = TaskOperation::OutputMerge as i32;
    query.run_id = request.request.scope.run_id.as_uuid().as_bytes().to_vec();
    query.definition_json = serde_json::to_vec(&request.request).map_err(|_| "合流请求无效")?;
    let reply = send(config, query, request.request.scope.kind).await?;
    let metadata = decode_output_merge(&reply.document_json, &reply.output)
        .map_err(|_| "合流记录或字节摘要无效")?;
    if metadata.request != request.request
        || metadata.binding.snapshot_sha256 != request.hash
        || metadata.binding.child_id != request.child
    {
        return Err("合流范围或冻结身份不一致".into());
    }
    let rendered = render(&metadata, &reply.output);
    Ok(MergedPage {
        metadata: Arc::new(metadata),
        rendered,
    })
}
fn render(page: &OutputMergePage, body: &[u8]) -> Arc<LogPage> {
    use std::fmt::Write;
    let mut rows = Vec::new();
    for event in &page.events {
        let data = &body[event.page_offset..event.page_offset + event.bytes as usize];
        for (index, chunk) in data.chunks(32).enumerate() {
            let mut text = format!(
                "#{} {}ms {} {:012x}  ",
                event.sequence,
                event.recorded_at_ms,
                if event.stderr { "stderr" } else { "stdout" },
                event.stream_offset + (index * 32) as u64
            );
            for byte in chunk {
                match byte {
                    b' '..=b'~' if *byte != b'\\' => text.push(char::from(*byte)),
                    b'\\' => text.push_str("\\\\"),
                    _ => {
                        let _written = write!(text, "\\x{byte:02x}");
                    }
                }
            }
            rows.push(LogRow {
            soft_wrap: false,
                line_id: rows.len() as u64 + 1,
                text: text.into(),
                style_spans: Arc::from([]),
                truncated: false,
            });
        }
    }
    if rows.is_empty() {
        rows.push(LogRow {
            soft_wrap: false,
            line_id: 1,
            text: "此合流页没有输出记录；覆盖范围见上方元数据".into(),
            style_spans: Arc::from([]),
            truncated: false,
        });
    }
    Arc::new(LogPage { first_line_id: 1,
        source_id: LogSourceId(0),
        revision: 0,
        anchor_line_id: 1,
        total_line_count: rows.len() as u64,
        rows: rows.into(),
        has_before: page.request.after != 0,
        has_after: page.next.is_some(),
    })
}
