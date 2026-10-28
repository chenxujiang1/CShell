use cshell_application::*;
use cshell_ipc::{ExportDirectoryStaging, ExportStaging, TaskOperation, TaskRequest};
use std::{
    io::{Read, Seek, SeekFrom, Write},
    path::PathBuf,
    time::{Duration, Instant},
};
use tokio::io::AsyncReadExt;
async fn document(path: &str) -> Result<Vec<u8>, &'static str> {
    let file = tokio::fs::File::open(path)
        .await
        .map_err(|_| "cannot open workspace document")?;
    let mut bytes = Vec::new();
    file.take(MAX_WORKSPACE_DOCUMENT as u64 + 1)
        .read_to_end(&mut bytes)
        .await
        .map_err(|_| "cannot read workspace document")?;
    if bytes.len() > MAX_WORKSPACE_DOCUMENT {
        return Err("workspace document limit");
    }
    Ok(bytes)
}
pub(crate) async fn run(args: &[String]) -> Result<(), &'static str> {
    if args
        .first()
        .is_some_and(|command| command == "output-workspace-scopes")
    {
        if args.len() != 5 {
            return Err(crate::USAGE);
        }
        let bytes = document(&args[2]).await?;
        let requested_step = if args[3] == "all" {
            None
        } else {
            Some(args[3].parse::<usize>().map_err(|_| "invalid step index")?)
        };
        let streams: &[bool] = match args[4].as_str() {
            "stdout" => &[false],
            "stderr" => &[true],
            "both" => &[false, true],
            _ => return Err(crate::USAGE),
        };
        let mut sources = Vec::new();
        match args[1].as_str() {
            "task" => {
                let view = decode_task_view(&bytes).map_err(|_| "invalid task status document")?;
                if view.runner_active || !task_terminal(view.meta.state) {
                    return Err("task is not stopped");
                }
                for target in &view.targets {
                    for (i, step) in target.steps.iter().enumerate() {
                        if target_terminal(target.state)
                            && target_terminal(step.state)
                            && requested_step.is_none_or(|index| index == i)
                        {
                            for stderr in streams {
                                sources.push(OutputAnalysisRequest {
                                    format: "cshell.output-analysis-request".into(),
                                    version: 1,
                                    kind: OutputAnalysisKind::Task,
                                    run_id: view.id,
                                    left: target.host_id,
                                    left_revision: view.meta.revision,
                                    right: None,
                                    right_revision: None,
                                    step: i as u32,
                                    stderr: *stderr,
                                });
                            }
                        }
                    }
                }
            }
            "mixed" => {
                let page = decode_mixed_status(&bytes).map_err(|_| "invalid mixed status page")?;
                if page.active {
                    return Err("mixed run is active");
                }
                for row in &page.targets {
                    if row.run.state == MixedTaskState::Running {
                        continue;
                    }
                    for (i, step) in row.run.steps.iter().enumerate() {
                        if requested_step.is_none_or(|index| index == i)
                            && let MixedReceipt::Command { target, .. } = &step.receipt
                            && target_terminal(target.state)
                            && target_terminal(target.steps[0].state)
                        {
                            for stderr in streams {
                                sources.push(OutputAnalysisRequest {
                                    format: "cshell.output-analysis-request".into(),
                                    version: 1,
                                    kind: OutputAnalysisKind::Mixed,
                                    run_id: page.run_id,
                                    left: row.run.host_id,
                                    left_revision: row.run.revision,
                                    right: None,
                                    right_revision: None,
                                    step: i as u32,
                                    stderr: *stderr,
                                });
                            }
                        }
                    }
                }
            }
            _ => return Err(crate::USAGE),
        }
        if sources.is_empty() || sources.len() > MAX_WORKSPACE_SOURCES {
            return Err("no stopped outputs or too many scopes; choose a step/subset");
        }
        println!(
            "{}",
            serde_json::to_string_pretty(&sources).map_err(|_| "scope encoding failed")?
        );
        return Ok(());
    }
    if args
        .first()
        .is_some_and(|command| command == "output-index-read")
    {
        if args.len() != 4 {
            return Err(crate::USAGE);
        }
        let scope = decode_output_analysis_request(&document(&args[1]).await?)
            .map_err(|_| "invalid index scope")?;
        let entry: OutputIndexEntry = serde_json::from_slice(&document(&args[2]).await?)
            .map_err(|_| "invalid index entry")?;
        let offset = args[3].parse().map_err(|_| "invalid index offset")?;
        let mut query = TaskRequest::default();
        query.operation = TaskOperation::OutputIndexRead as i32;
        query.run_id = scope.run_id.as_uuid().as_bytes().to_vec();
        query.offset = offset;
        query.definition_json = serde_json::to_vec(&scope).map_err(|_| "invalid scope")?;
        query.expected_targets_json = serde_json::to_vec(&entry).map_err(|_| "invalid entry")?;
        let reply = crate::task_output_transport::send(query, scope.kind).await?;
        let page: OutputIndexPage =
            serde_json::from_slice(&reply.document_json).map_err(|_| "invalid index page")?;
        page.validate().map_err(|_| "invalid index page")?;
        if page.scope != scope || page.entry != entry || page.offset as u64 != offset {
            return Err("index page binding mismatch");
        }
        println!(
            "{}",
            std::str::from_utf8(&reply.document_json).map_err(|_| "invalid index reply")?
        );
        return Ok(());
    }
    let command = args.first().ok_or(crate::USAGE)?.as_str();
    if command == "output-container-inspect" || command == "output-container-extract" {
        if command == "output-container-inspect" && args.len() != 2
            || command == "output-container-extract" && (args.len() != 4 || args[3] != "--confirm")
        {
            return Err(crate::USAGE);
        }
        let source = PathBuf::from(&args[1]);
        let destination = args.get(2).map(PathBuf::from);
        let message = tokio::task::spawn_blocking(move || {
            let meta = std::fs::symlink_metadata(&source).map_err(|_| "cannot inspect container")?;
            if !meta.is_file() || meta.file_type().is_symlink() || meta.len() > MAX_OUTPUT_BUNDLE_BYTES { return Err("container must be bounded regular file"); }
            #[cfg(windows)] { use std::os::windows::fs::MetadataExt; if meta.file_attributes() & 0x400 != 0 { return Err("container reparse point"); } }
            let mut file = std::fs::File::open(source).map_err(|_| "cannot open container")?;
            let artifact = inspect_output_container(&mut file).map_err(|_| "container checksum, metadata or EOF failed")?;
            if let Some(destination) = destination {
                let staging = ExportDirectoryStaging::prepare(&destination).map_err(|_| "cannot reserve extraction staging; choose new absolute directory")?;
                extract_output_container(&mut file, &artifact, |manifest, kind| {
                    let stream = match kind { OutputArchiveEntryKind::Stdout => "stdout", OutputArchiveEntryKind::Stderr => "stderr", OutputArchiveEntryKind::Journal => "journal" };
                    let name = format!("{}-{}-{}-{stream}.bin", manifest.scope.run_id, manifest.scope.left, manifest.scope.step);
                    staging.create(&name).map(|f| Box::new(f) as Box<dyn Write>).map_err(|_| std::io::Error::other("extraction staging"))
                }).map_err(|_| "extraction validation failed; destination not published")?;
                let mut manifest = staging.create("manifest.json").map_err(|_| "cannot stage manifest")?;
                serde_json::to_writer_pretty(&mut manifest, &artifact).map_err(|_| "cannot stage manifest")?;
                staging.publish().map_err(|_| "extraction publication failed; inspect possible partial directory before another attempt")?;
            }
            serde_json::to_string_pretty(&artifact).map_err(|_| "cannot print artifact")
        }).await.map_err(|_| "container worker failed")??;
        println!("{message}");
        return Ok(());
    }
    if command == "output-workspace-request" {
        if !(3..=4).contains(&args.len()) {
            return Err(crate::USAGE);
        }
        let sources: Vec<OutputAnalysisRequest> =
            serde_json::from_slice(&document(&args[1]).await?)
                .map_err(|_| "invalid scope array")?;
        let action = match args[2].as_str() {
            "index" => OutputWorkspaceAction::Index,
            "search" => OutputWorkspaceAction::Search,
            "diff" => OutputWorkspaceAction::TextDiff,
            "bundle" => OutputWorkspaceAction::Bundle,
            "retention-preview" => OutputWorkspaceAction::RetentionPreview,
            _ => return Err(crate::USAGE),
        };
        let request = OutputWorkspaceRequest {
            format: "cshell.output-workspace-request".into(),
            version: 1,
            action,
            sources,
            query: args.get(3).cloned(),
            retention: None,
        };
        request
            .validate()
            .map_err(|_| "invalid workspace action/scopes/query")?;
        println!(
            "{}",
            serde_json::to_string_pretty(&request)
                .map_err(|_| "cannot encode workspace request")?
        );
        return Ok(());
    }
    if args.len() < 2 {
        return Err(crate::USAGE);
    }
    let bytes = document(&args[1]).await?;
    if command == "output-workspace-export" {
        if args.len() != 3 {
            return Err(crate::USAGE);
        }
        let doc = decode_output_workspace(&bytes).map_err(|_| "invalid workspace job")?;
        let artifact = doc
            .artifact
            .as_ref()
            .ok_or("workspace bundle is not completed")?
            .clone();
        let path = PathBuf::from(&args[2]);
        let mut staging = ExportStaging::prepare(&path)
            .map_err(|_| "cannot stage bundle; choose new absolute file")?;
        let started = Instant::now();
        let mut offset = 0;
        while offset < artifact.bytes {
            if started.elapsed() > Duration::from_secs(1800) {
                return Err("bundle transfer deadline");
            }
            let mut request = TaskRequest::default();
            request.operation = TaskOperation::OutputWorkspaceRead as i32;
            request.run_id = doc.sources[0].run_id.as_uuid().as_bytes().to_vec();
            request.result_id = doc.job_id.as_uuid().as_bytes().to_vec();
            request.offset = offset;
            request.expected_targets_json = serde_json::to_vec(&doc)
                .map_err(|_| "invalid workspace job")?;
            let reply = crate::task_output_transport::send(request, doc.sources[0].kind).await?;
            if decode_output_workspace(&reply.document_json)
                .map_err(|_| "invalid download reply")?
                != doc
                || reply.output.is_empty()
                || offset + reply.output.len() as u64 > artifact.bytes
            {
                return Err("bundle download binding/length mismatch");
            }
            staging
                .file_mut()
                .map_err(|_| "bundle staging unavailable")?
                .write_all(&reply.output)
                .map_err(|_| "bundle staging write failed")?;
            offset += reply.output.len() as u64;
        }
        staging
            .file_mut()
            .map_err(|_| "bundle staging unavailable")?
            .seek(SeekFrom::Start(0))
            .map_err(|_| "bundle staging seek failed")?;
        if inspect_output_bundle(
            staging
                .file_mut()
                .map_err(|_| "bundle staging unavailable")?,
        )
        .map_err(|_| "bundle container validation failed")?
            != artifact
        {
            return Err("bundle artifact mismatch");
        }
        staging
            .publish()
            .map_err(|_| "bundle publication failed; inspect destination before another attempt")?;
        return Ok(());
    }
    let mut query = TaskRequest::default();
    let mut previous = None;
    let mut submitted = None;
    let scope = match command {
        "output-workspace" if args.len() == 2 => {
            let request =
                decode_output_workspace_request(&bytes).map_err(|_| "invalid workspace request")?;
            if request.action == OutputWorkspaceAction::RetentionApply {
                return Err("use output-retention-apply with explicit confirmation");
            }
            query.operation = TaskOperation::OutputWorkspace as i32;
            query.definition_json = bytes.clone();
            let scope = request.sources[0].clone();
            submitted = Some(request);
            scope
        }
        "output-retention-apply" if args.len() == 3 && args[2] == "--confirm" => {
            let plan: OutputRetentionPlan =
                serde_json::from_slice(&bytes).map_err(|_| "invalid retention plan")?;
            plan.validate().map_err(|_| "invalid retention plan")?;
            let request = OutputWorkspaceRequest {
                format: "cshell.output-workspace-request".into(),
                version: 1,
                action: OutputWorkspaceAction::RetentionApply,
                sources: plan.sources.clone(),
                query: None,
                retention: Some(plan.clone()),
            };
            query.operation = TaskOperation::OutputWorkspace as i32;
            query.confirmed = true;
            query.definition_json =
                serde_json::to_vec(&request).map_err(|_| "invalid retention request")?;
            let scope = plan.sources[0].clone();
            submitted = Some(request);
            scope
        }
        "output-retention-receipt" if args.len() == 2 => {
            let plan: OutputRetentionPlan =
                serde_json::from_slice(&bytes).map_err(|_| "invalid retention plan")?;
            plan.validate().map_err(|_| "invalid retention plan")?;
            query.operation = TaskOperation::OutputRetentionReceipt as i32;
            query.result_id = plan.id.as_uuid().as_bytes().to_vec();
            plan.sources[0].clone()
        }
        "output-workspace-get" | "output-workspace-cancel" | "output-workspace-page" => {
            if command == "output-workspace-page" && args.len() != 3
                || command != "output-workspace-page" && args.len() != 2
            {
                return Err(crate::USAGE);
            }
            let doc = decode_output_workspace(&bytes).map_err(|_| "invalid workspace job")?;
            query.operation = match command {
                "output-workspace-get" => TaskOperation::OutputWorkspaceGet,
                "output-workspace-cancel" => TaskOperation::OutputWorkspaceCancel,
                _ => TaskOperation::OutputWorkspacePage,
            } as i32;
            query.result_id = doc.job_id.as_uuid().as_bytes().to_vec();
            if command == "output-workspace-page" {
                query.offset = args[2].parse().map_err(|_| "invalid page offset")?;
                query.expected_targets_json = bytes;
            }
            let scope = doc.sources[0].clone();
            previous = Some(doc);
            scope
        }
        _ => return Err(crate::USAGE),
    };
    query.run_id = scope.run_id.as_uuid().as_bytes().to_vec();
    let reply = crate::task_output_transport::send(query, scope.kind).await?;
    if command == "output-workspace-page" {
        let page: OutputWorkspacePage =
            serde_json::from_slice(&reply.document_json).map_err(|_| "invalid workspace page")?;
        page.validate(previous.as_ref().ok_or("missing workspace job")?)
            .map_err(|_| "workspace page mismatch")?;
    } else if command == "output-retention-receipt" {
        let receipt: OutputRetentionReceipt = serde_json::from_slice(&reply.document_json)
            .map_err(|_| "invalid retention receipt")?;
        receipt
            .validate()
            .map_err(|_| "invalid retention receipt")?;
    } else {
        let doc =
            decode_output_workspace(&reply.document_json).map_err(|_| "invalid workspace reply")?;
        if submitted
            .as_ref()
            .is_some_and(|request| doc.sources != request.sources || doc.action != request.action)
            || previous.as_ref().is_some_and(|old| {
                doc.job_id != old.job_id
                    || doc.sources != old.sources
                    || doc.bindings != old.bindings
                    || doc.bytes < old.bytes
                    || old.state != OutputAnalysisState::Running && doc != *old
            })
        {
            return Err("workspace reply scope/progress mismatch");
        }
    }
    println!(
        "{}",
        std::str::from_utf8(&reply.document_json).map_err(|_| "invalid workspace reply encoding")?
    );
    Ok(())
}
