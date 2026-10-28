use cshell_application::*;
use cshell_ipc::{TaskOperation, TaskRequest};
use serde::{Deserialize, Serialize};
use zeroize::Zeroize;
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PageFile {
    metadata: OutputMergePage,
    bytes: Vec<u8>,
}
impl Drop for PageFile {
    fn drop(&mut self) {
        self.bytes.zeroize();
    }
}
pub(crate) async fn run(args: &[String]) -> Result<(), &'static str> {
    if args.len() != 2 {
        return Err(crate::USAGE);
    }
    let bytes = crate::document(&args[1]).await?;
    match args[0].as_str() {
        "output-merge-request" => {
            let scope =
                decode_output_analysis_request(&bytes).map_err(|_| "invalid merge scope")?;
            let request = OutputMergeRequest {
                format: "cshell.output-merge-request".into(),
                version: 1,
                scope,
                after: 0,
                limit: 64,
                expected_journal_sha256: None,
            };
            request
                .validate()
                .map_err(|_| "merge needs a single-target stdout scope; it reads both streams")?;
            println!(
                "{}",
                serde_json::to_string_pretty(&request).map_err(|_| "invalid merge request")?
            );
            Ok(())
        }
        "output-merge-next-request" => {
            let page: PageFile =
                serde_json::from_slice(&bytes).map_err(|_| "invalid saved merge page")?;
            page.metadata
                .validate(&page.bytes)
                .map_err(|_| "invalid saved merge page")?;
            let next = page
                .metadata
                .request
                .next(&page.metadata)
                .map_err(|_| "invalid merge continuation")?
                .ok_or("no more recorded merge events")?;
            println!(
                "{}",
                serde_json::to_string_pretty(&next).map_err(|_| "invalid merge continuation")?
            );
            Ok(())
        }
        "output-merge" => {
            let request =
                decode_output_merge_request(&bytes).map_err(|_| "invalid merge request")?;
            let mut query = TaskRequest::default();
            query.operation = TaskOperation::OutputMerge as i32;
            query.run_id = request.scope.run_id.as_uuid().as_bytes().to_vec();
            query.definition_json = bytes.to_vec();
            let mut reply = crate::task_output_transport::send(query, request.scope.kind).await?;
            let metadata = decode_output_merge(&reply.document_json, &reply.output)
                .map_err(|_| "invalid merge page or byte digest")?;
            if metadata.request != request {
                return Err("merge page scope/cursor mismatch");
            }
            let page = PageFile {
                metadata,
                bytes: std::mem::take(&mut reply.output),
            };
            println!(
                "{}",
                serde_json::to_string(&page).map_err(|_| "invalid merge page")?
            );
            Ok(())
        }
        _ => Err(crate::USAGE),
    }
}
