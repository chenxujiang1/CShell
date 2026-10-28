//! Readonly full-output jobs use the same application request and document as desktop.
use cshell_application::*;
use cshell_domain::{HostId, TaskRunId};
use cshell_ipc::{TaskOperation, TaskRequest};
fn run_id(value: &str) -> Result<TaskRunId, &'static str> {
    Ok(TaskRunId::from_bytes(
        cshell_ipc::decode_id(&crate::id(value)?).map_err(|_| "invalid run ID")?,
    ))
}
fn host_id(value: &str) -> Result<HostId, &'static str> {
    Ok(HostId::from_bytes(
        cshell_ipc::decode_id(&crate::id(value)?).map_err(|_| "invalid host ID")?,
    ))
}
pub(crate) async fn run(args: &[String]) -> Result<(), &'static str> {
    if args
        .first()
        .is_some_and(|command| command == "output-analysis-request")
    {
        if args.len() != 7 && args.len() != 9 {
            return Err(crate::USAGE);
        }
        let kind = match args[1].as_str() {
            "task" => OutputAnalysisKind::Task,
            "mixed" => OutputAnalysisKind::Mixed,
            _ => return Err("select task or mixed"),
        };
        let request = OutputAnalysisRequest {
            format: "cshell.output-analysis-request".into(),
            version: 1,
            kind,
            run_id: run_id(&args[2])?,
            left: host_id(&args[3])?,
            left_revision: crate::number(&args[4])?,
            step: u32::try_from(crate::number(&args[5])?).map_err(|_| "invalid step")?,
            stderr: match args[6].as_str() {
                "stdout" => false,
                "stderr" => true,
                _ => return Err("select stdout or stderr"),
            },
            right: if args.len() == 9 {
                Some(host_id(&args[7])?)
            } else {
                None
            },
            right_revision: if args.len() == 9 {
                Some(crate::number(&args[8])?)
            } else {
                None
            },
        };
        request
            .validate()
            .map_err(|_| "invalid output analysis scope")?;
        println!(
            "{}",
            serde_json::to_string_pretty(&request)
                .map_err(|_| "invalid output analysis request")?
        );
        return Ok(());
    }
    if args.len() != 2 {
        return Err(crate::USAGE);
    }
    let mut query = TaskRequest::default();
    let bytes = crate::document(&args[1]).await?;
    let (expected, previous) = match args[0].as_str() {
        "output-analyze" => {
            let request = decode_output_analysis_request(&bytes)
                .map_err(|_| "invalid output analysis request")?;
            query.operation = TaskOperation::OutputAnalyze as i32;
            query.definition_json = bytes.to_vec();
            (request, None)
        }
        "output-analysis-get" | "output-analysis-cancel" => {
            let document = decode_output_analysis(&bytes)
                .map_err(|_| "invalid output analysis job document")?;
            query.operation = if args[0] == "output-analysis-cancel" {
                TaskOperation::OutputAnalysisCancel
            } else {
                TaskOperation::OutputAnalysisGet
            } as i32;
            query.result_id = document.job_id.as_uuid().as_bytes().to_vec();
            (document.request.clone(), Some(document))
        }
        _ => return Err(crate::USAGE),
    };
    query.run_id = expected.run_id.as_uuid().as_bytes().to_vec();
    let reply = crate::task_output_transport::send(query, expected.kind).await?;
    let document = decode_output_analysis(&reply.document_json)
        .map_err(|_| "invalid output analysis response")?;
    if document.request != expected
        || previous.as_ref().is_some_and(|previous| {
            document.job_id != previous.job_id
                || document.left != previous.left
                || document.right != previous.right
                || document.left_bytes < previous.left_bytes
                || document
                    .right_bytes
                    .zip(previous.right_bytes)
                    .is_some_and(|(now, before)| now < before)
                || previous.state != OutputAnalysisState::Running && document != *previous
        })
    {
        return Err("output analysis scope or progress mismatch");
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&document).map_err(|_| "invalid output analysis response")?
    );
    Ok(())
}
