use cshell_application::*;
use cshell_ipc::{ExportStaging, ExportTransferControl, TaskOperation, TaskRequest};
use std::{
    io::{self, Seek, SeekFrom},
    path::PathBuf,
    time::{Duration, Instant},
};
pub(crate) async fn run(args: &[String]) -> Result<(), &'static str> {
    if args
        .first()
        .is_some_and(|command| command == "output-archive-inspect")
    {
        if args.len() != 2 {
            return Err(crate::USAGE);
        }
        let path = PathBuf::from(&args[1]);
        let artifact = tokio::task::spawn_blocking(move || {
            let metadata =
                std::fs::symlink_metadata(&path).map_err(|_| "cannot inspect archive file")?;
            if !metadata.is_file()
                || metadata.file_type().is_symlink()
                || metadata.len() > MAX_OUTPUT_ARCHIVE_BYTES
            {
                return Err("archive must be a bounded regular file");
            }
            let mut file = std::fs::File::open(&path).map_err(|_| "cannot open archive")?;
            inspect_output_archive(&mut file)
                .map_err(|_| "archive manifest, entry, footer or EOF validation failed")
        })
        .await
        .map_err(|_| "archive inspection worker failed")??;
        println!(
            "{}",
            serde_json::to_string_pretty(&artifact).map_err(|_| "invalid archive artifact")?
        );
        return Ok(());
    }
    if args
        .first()
        .is_some_and(|command| command == "output-archive-export")
    {
        if args.len() != 3 {
            return Err(crate::USAGE);
        }
        let document = decode_output_archive(&crate::document(&args[1]).await?)
            .map_err(|_| "invalid archive job")?;
        let artifact = document
            .artifact
            .ok_or("archive capture is not completed")?;
        return export(artifact, PathBuf::from(&args[2])).await;
    }
    if args.len() != 2 {
        return Err(crate::USAGE);
    }
    let bytes = crate::document(&args[1]).await?;
    let mut query = TaskRequest::default();
    let (scope, previous) = match args[0].as_str() {
        "output-archive" => {
            let scope =
                decode_output_analysis_request(&bytes).map_err(|_| "invalid archive scope")?;
            if scope.right.is_some() || scope.stderr {
                return Err("archive needs a single-target stdout scope; captures both streams");
            }
            query.operation = TaskOperation::OutputArchive as i32;
            query.definition_json = bytes.to_vec();
            (scope, None)
        }
        "output-archive-get" | "output-archive-cancel" => {
            let document = decode_output_archive(&bytes).map_err(|_| "invalid archive job")?;
            query.operation = if args[0] == "output-archive-cancel" {
                TaskOperation::OutputArchiveCancel
            } else {
                TaskOperation::OutputArchiveGet
            } as i32;
            query.result_id = document.job_id.as_uuid().as_bytes().to_vec();
            (document.scope.clone(), Some(document))
        }
        _ => return Err(crate::USAGE),
    };
    query.run_id = scope.run_id.as_uuid().as_bytes().to_vec();
    let reply = crate::task_output_transport::send(query, scope.kind).await?;
    let document =
        decode_output_archive(&reply.document_json).map_err(|_| "invalid archive response")?;
    if document.scope != scope
        || previous.as_ref().is_some_and(|previous| {
            document.job_id != previous.job_id
                || document.binding != previous.binding
                || document.bytes < previous.bytes
                || document.source_bytes < previous.source_bytes
                || previous.state != OutputAnalysisState::Running && document != *previous
        })
    {
        return Err("archive scope or progress mismatch");
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&document).map_err(|_| "invalid archive response")?
    );
    Ok(())
}
async fn export(artifact: OutputArchiveArtifact, path: PathBuf) -> Result<(), &'static str> {
    artifact
        .validate()
        .map_err(|_| "invalid archive artifact")?;
    let mut staging = tokio::task::spawn_blocking(move || ExportStaging::prepare(&path))
        .await
        .map_err(|_| "archive staging worker failed")?
        .map_err(|_| "cannot stage archive; choose a new absolute file")?;
    let file = staging
        .file_mut()
        .map_err(|_| "archive staging unavailable")?
        .try_clone()
        .map_err(|_| "archive staging unavailable")?;
    let mut output = tokio::fs::File::from_std(file);
    let started = Instant::now();
    let run_id = artifact.manifest.scope.run_id;
    let archive_id = artifact.manifest.archive_id;
    let kind = artifact.manifest.scope.kind;
    let control = std::sync::Arc::new(ExportTransferControl::default());
    copy_output_archive(
        &artifact,
        &mut output,
        |offset, limit| async move {
            let mut query = TaskRequest::default();
            query.operation = TaskOperation::OutputArchiveRead as i32;
            query.run_id = run_id.as_uuid().as_bytes().to_vec();
            query.result_id = archive_id.as_uuid().as_bytes().to_vec();
            query.offset = offset;
            query.limit = limit as u32;
            let mut reply = crate::task_output_transport::send(query, kind)
                .await
                .map_err(io::Error::other)?;
            Ok((
                std::mem::take(&mut reply.document_json),
                OutputArchiveBytes(std::mem::take(&mut reply.output)),
            ))
        },
        || {
            if started.elapsed() > Duration::from_secs(1800) {
                Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "archive download deadline",
                ))
            } else {
                Ok(())
            }
        },
        |bytes| control.record_bytes(bytes),
    )
    .await
    .map_err(
        |_| "archive transfer failed or expired; destination not published, no automatic retry",
    )?;
    output
        .sync_all()
        .await
        .map_err(|_| "archive staging sync failed; destination not published")?;
    drop(output);
    tokio::task::spawn_blocking(move || {
        let file = staging
            .file_mut()
            .map_err(|_| "archive staging unavailable")?;
        file.seek(SeekFrom::Start(0))
            .map_err(|_| "archive staging cannot be inspected")?;
        let actual = inspect_output_archive_checked(file, || {
            if started.elapsed() > Duration::from_secs(1800) {
                Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "archive inspection deadline",
                ))
            } else {
                Ok(())
            }
        })
        .map_err(|_| "archive validation failed; destination not published")?;
        if actual != artifact {
            return Err("archive differs from capture; destination not published");
        }
        staging.publish().map_err(
            |_| "archive publication failed or uncertain; inspect destination before retrying",
        )
    })
    .await
    .map_err(|_| "archive publication worker failed; inspect destination")??;
    println!("raw output archive validated and saved to a new file");
    Ok(())
}
