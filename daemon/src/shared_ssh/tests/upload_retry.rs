use super::*;
use cshell_application::{
    TerminalUploadProtocol, TerminalUploadRetryPlan, TerminalUploadStart, TerminalUploadState,
};

async fn retry_call(
    f: &Fixture,
    source: SessionId,
    task: SessionId,
    operation: SftpOperation,
    revision: u64,
) -> SftpResponse {
    let envelope::Payload::SftpResponse(reply) = f
        .call(envelope::Payload::SftpRequest(SftpRequest {
            operation: operation as i32,
            session_id: source.as_uuid().as_bytes().to_vec(),
            transfer_id: task.as_uuid().as_bytes().to_vec(),
            confirmed: operation == SftpOperation::TerminalUploadRetry,
            expected_revision: revision,
            ..Default::default()
        }))
        .await
    else {
        panic!()
    };
    reply
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires isolated CSHELL_RUNTIME_DIR"]
async fn terminal_drop_retry_keeps_successes_original_directory_and_frozen_plan_without_mfa() {
    let f = Fixture::new(None).await;
    let source = id(&f.create().await);
    let shell = f.service.ssh_sessions.get(source).unwrap();
    echo(&shell, "cd /srv/one").await;
    let folder = f._root.path().join("retry-batch");
    std::fs::create_dir(&folder).unwrap();
    std::fs::write(folder.join("file-003"), b"conflict-seed").unwrap();
    let options = TerminalUploadStart {
        paths: vec![folder.to_str().unwrap().into()],
        protocol: TerminalUploadProtocol::Sftp,
        verification: Default::default(),
        confirm_before_start: false,
    };
    let seed = SessionId::new();
    upload_call(
        &f,
        source,
        seed,
        SftpOperation::TerminalUploadStart,
        Some(&options),
    )
    .await;
    assert_eq!(
        upload_wait(&f, source, seed, false).await.state,
        TerminalUploadState::Succeeded
    );
    for i in 0..3 {
        std::fs::write(folder.join(format!("file-{i:03}")), b"keep-success").unwrap();
    }
    let task = SessionId::new();
    upload_call(
        &f,
        source,
        task,
        SftpOperation::TerminalUploadStart,
        Some(&options),
    )
    .await;
    let failed = upload_wait(&f, source, task, false).await;
    assert_eq!(
        failed.state,
        TerminalUploadState::Failed,
        "{}",
        failed.detail
    );
    let reply = retry_call(
        &f,
        source,
        task,
        SftpOperation::TerminalUploadRetryInspect,
        0,
    )
    .await;
    assert_eq!(reply.status, SftpStatus::Ok as i32, "{}", reply.detail);
    let review: TerminalUploadRetryPlan = serde_json::from_slice(&reply.document_json).unwrap();
    assert!(review.can_retry, "{}", review.detail);
    assert!(review.completed_files >= 3);
    assert_eq!(review.completed_files + review.retry_files, 4);
    assert_eq!(review.directory, "/srv/one");
    for (wrong_source, revision) in [
        (SessionId::new(), review.revision),
        (source, 0),
        (source, review.revision + 1),
    ] {
        let reply = retry_call(
            &f,
            wrong_source,
            task,
            SftpOperation::TerminalUploadRetry,
            revision,
        )
        .await;
        assert_ne!(reply.status, SftpStatus::Ok as i32);
    }
    // A retry must not rescan this folder, follow shell cd, or retransmit successes.
    std::fs::write(
        folder.join("file-000"),
        b"changed-local-success-must-not-be-uploaded-again",
    )
    .unwrap();
    std::fs::write(folder.join("new-after-failure"), b"not-in-the-plan").unwrap();
    echo(&shell, "cd /srv/two").await;
    let client = shell.open_sftp().await.unwrap();
    client
        .remove_entry("/srv/one/retry-batch/file-003", false)
        .await
        .unwrap();
    let reply = retry_call(
        &f,
        source,
        task,
        SftpOperation::TerminalUploadRetry,
        review.revision,
    )
    .await;
    assert_eq!(reply.status, SftpStatus::Ok as i32, "{}", reply.detail);
    let replay = retry_call(
        &f,
        source,
        task,
        SftpOperation::TerminalUploadRetry,
        review.revision,
    )
    .await;
    assert_ne!(replay.status, SftpStatus::Ok as i32);
    let done = upload_wait(&f, source, task, false).await;
    assert_eq!(
        done.state,
        TerminalUploadState::Succeeded,
        "{}",
        done.detail
    );
    assert_eq!(done.completed, 4);
    assert_eq!(done.bytes, done.total_bytes);
    for (path, size) in [
        ("/srv/one/retry-batch/file-000", 12),
        ("/srv/one/retry-batch/file-003", 13),
    ] {
        assert_eq!(
            client
                .metadata(path, cshell_sftp::SymlinkPolicy::NoFollow)
                .await
                .unwrap()
                .size,
            Some(size)
        );
    }
    for path in [
        "/srv/one/retry-batch/new-after-failure",
        "/srv/two/retry-batch",
    ] {
        assert!(
            client
                .metadata(path, cshell_sftp::SymlinkPolicy::NoFollow)
                .await
                .is_err()
        );
    }
    client.close().await.unwrap();
    echo(&shell, "terminal-still-usable-after-retry").await;
    f.counts(1);
    // The original transport is mandatory even when a failed plan is retained.
    let failed_task = SessionId::new();
    echo(&shell, "cd /srv/one").await;
    upload_call(
        &f,
        source,
        failed_task,
        SftpOperation::TerminalUploadStart,
        Some(&options),
    )
    .await;
    assert_eq!(
        upload_wait(&f, source, failed_task, false).await.state,
        TerminalUploadState::Failed
    );
    f.close(source).await;
    let reply = retry_call(
        &f,
        source,
        failed_task,
        SftpOperation::TerminalUploadRetryInspect,
        0,
    )
    .await;
    let review: TerminalUploadRetryPlan = serde_json::from_slice(&reply.document_json).unwrap();
    assert!(!review.can_retry);
    assert_ne!(
        retry_call(
            &f,
            source,
            failed_task,
            SftpOperation::TerminalUploadRetry,
            review.revision
        )
        .await
        .status,
        SftpStatus::Ok as i32
    );
}
