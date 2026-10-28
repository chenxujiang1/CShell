use cshell_application::*;
use cshell_ipc::{TaskOperation, TaskRequest};
pub(crate) async fn run(args: &[String]) -> Result<(), &'static str> {
    if args
        .first()
        .is_some_and(|command| command == "output-search-request")
    {
        if args.len() != 4 {
            return Err(crate::USAGE);
        }
        let scope = decode_output_analysis_request(&crate::document(&args[1]).await?)
            .map_err(|_| "invalid output scope")?;
        let hex = match args[2].as_str() {
            "utf8" => false,
            "hex" => true,
            _ => return Err("select utf8 or hex"),
        };
        let pattern = output_search_pattern(&args[3], hex)
            .map_err(|_| "invalid or oversized search bytes")?;
        let request = OutputSearchRequest {
            format: "cshell.output-search-request".into(),
            version: 1,
            scope,
            pattern,
            start: 0,
            maximum_matches: 128,
            expected_stream_sha256: None,
        };
        request
            .validate()
            .map_err(|_| "search needs a single-target stopped output scope")?;
        println!(
            "{}",
            serde_json::to_string_pretty(&request).map_err(|_| "invalid search request")?
        );
        return Ok(());
    }
    if args
        .first()
        .is_some_and(|command| command == "output-search-next-request")
    {
        if args.len() != 3 {
            return Err(crate::USAGE);
        }
        let request = decode_output_search_request(&crate::document(&args[1]).await?)
            .map_err(|_| "invalid search request")?;
        let document = decode_output_search(&crate::document(&args[2]).await?)
            .map_err(|_| "invalid search job")?;
        let next = request
            .next(&document)
            .map_err(|_| "job does not match query, or is not completed")?
            .ok_or("no more matches after this page")?;
        println!(
            "{}",
            serde_json::to_string_pretty(&next).map_err(|_| "invalid search continuation")?
        );
        return Ok(());
    }
    if args.len() != 2 {
        return Err(crate::USAGE);
    }
    let bytes = crate::document(&args[1]).await?;
    let mut query = TaskRequest::default();
    let (expected, previous) = match args[0].as_str() {
        "output-search" => {
            let request = decode_output_search_request(&bytes)
                .map_err(|_| "invalid output search request")?;
            let descriptor = request
                .descriptor()
                .map_err(|_| "invalid output search request")?;
            query.operation = TaskOperation::OutputSearch as i32;
            query.definition_json = bytes.to_vec();
            (descriptor, None)
        }
        "output-search-get" | "output-search-cancel" => {
            let document = decode_output_search(&bytes).map_err(|_| "invalid search job")?;
            query.operation = if args[0] == "output-search-cancel" {
                TaskOperation::OutputSearchCancel
            } else {
                TaskOperation::OutputSearchGet
            } as i32;
            query.result_id = document.job_id.as_uuid().as_bytes().to_vec();
            (document.request.clone(), Some(document))
        }
        _ => return Err(crate::USAGE),
    };
    query.run_id = expected.scope.run_id.as_uuid().as_bytes().to_vec();
    let reply = crate::task_output_transport::send(query, expected.scope.kind).await?;
    let document =
        decode_output_search(&reply.document_json).map_err(|_| "invalid search response")?;
    if document.request != expected
        || previous.as_ref().is_some_and(|previous| {
            document.job_id != previous.job_id
                || document.binding != previous.binding
                || document.bytes < previous.bytes
                || previous.state != OutputAnalysisState::Running && document != *previous
        })
    {
        return Err("search scope or progress mismatch");
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&document).map_err(|_| "invalid search response")?
    );
    Ok(())
}
