#![allow(clippy::unwrap_used)]
use super::*;
#[test]
fn directory_plan_keeps_nested_unicode_and_empty_directories_without_following_links() {
    let root = tempfile::tempdir().unwrap();
    let folder = root.path().join("上传目录");
    std::fs::create_dir_all(folder.join("nested/empty")).unwrap();
    std::fs::write(folder.join("nested/文件.txt"), b"content").unwrap();
    let control = TransferControl::default();
    let paths = vec![folder.to_str().unwrap().to_owned()];
    let result = plan(&paths, &control).unwrap();
    assert!(result.has_directories);
    assert_eq!(result.bytes, 7);
    assert_eq!(result.files.len(), 1);
    assert_eq!(result.files[0].1, "上传目录/nested/文件.txt");
    assert_eq!(
        Path::new(&result.files[0].0),
        folder.join("nested/文件.txt").canonicalize().unwrap()
    );
    assert!(result.directories.contains(&"上传目录/nested/empty".into()));
    control.cancel();
    assert!(plan(&paths, &control).is_err());
}

#[cfg(unix)]
#[tokio::test]
async fn parent_alias_is_frozen_once_and_later_directory_links_are_rejected() {
    use std::os::unix::fs::symlink;
    let root = tempfile::tempdir().unwrap();
    let real = root.path().join("real");
    let other = root.path().join("other");
    for directory in [&real, &other] {
        std::fs::create_dir_all(directory.join("selected")).unwrap();
    }
    std::fs::write(real.join("selected/value"), b"original").unwrap();
    std::fs::write(other.join("selected/value"), b"replacement").unwrap();
    let alias = root.path().join("alias");
    symlink(&real, &alias).unwrap();
    let result = plan(
        &[
            alias.join("selected").to_str().unwrap().into(),
            real.join("selected").to_str().unwrap().into(),
        ],
        &TransferControl::default(),
    )
    .unwrap();
    assert_eq!(result.files.len(), 1, "aliases must not duplicate a source");
    let frozen = Path::new(&result.files[0].0);
    assert_eq!(frozen, real.join("selected/value").canonicalize().unwrap());
    std::fs::remove_file(&alias).unwrap();
    symlink(&other, &alias).unwrap();
    let mut source = crate::sftp_ipc::verified::LocalSource::open(frozen)
        .await
        .unwrap();
    assert_eq!(source.read(0, 8).await.unwrap(), b"original");
    drop(source);
    std::fs::rename(real.join("selected"), real.join("held")).unwrap();
    symlink(other.join("selected"), real.join("selected")).unwrap();
    assert!(
        crate::sftp_ipc::verified::LocalSource::open(frozen)
            .await
            .is_err(),
        "a link replacing a frozen ancestor must still be refused"
    );
}

#[cfg(unix)]
#[test]
fn selected_and_nested_links_remain_rejected() {
    use std::os::unix::fs::symlink;
    let root = tempfile::tempdir().unwrap();
    let folder = root.path().join("selected");
    std::fs::create_dir(&folder).unwrap();
    std::fs::write(folder.join("value"), b"content").unwrap();
    let alias = root.path().join("selected-link");
    symlink(&folder, &alias).unwrap();
    assert!(
        plan(
            &[alias.to_str().unwrap().into()],
            &TransferControl::default()
        )
        .is_err()
    );
    symlink(folder.join("value"), folder.join("nested-link")).unwrap();
    assert!(
        plan(
            &[folder.to_str().unwrap().into()],
            &TransferControl::default()
        )
        .is_err()
    );
}
#[test]
fn duplicate_target_names_and_parent_traversal_are_rejected_before_writes() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join("one")).unwrap();
    std::fs::create_dir_all(root.path().join("two")).unwrap();
    let a = root.path().join("one/same");
    let b = root.path().join("two/same");
    std::fs::write(&a, b"a").unwrap();
    std::fs::write(&b, b"b").unwrap();
    assert!(
        plan(
            &[a.to_str().unwrap().into(), b.to_str().unwrap().into()],
            &TransferControl::default()
        )
        .is_err()
    );
    assert!(
        plan(
            &[root.path().join("one/../two/same").to_str().unwrap().into()],
            &TransferControl::default()
        )
        .is_err()
    );
}
#[tokio::test]
async fn disconnected_source_or_unknown_upload_never_creates_a_job() {
    let service = SftpIpcService::default();
    let registry = SshSessionRegistry::default();
    let source = SessionId::new();
    let id = SessionId::new();
    let request = SftpRequest {
        operation: SftpOperation::TerminalUploadStart as i32,
        session_id: source.as_uuid().as_bytes().to_vec(),
        transfer_id: id.as_uuid().as_bytes().to_vec(),
        confirmed: true,
        document_json: serde_json::to_vec(&Start {
            paths: vec!["C:/example".into()],
            protocol: Protocol::Sftp,
            verification: Default::default(),
            confirm_before_start: false,
        })
        .unwrap(),
        ..Default::default()
    };
    let response = service.handle(request, &registry).await;
    assert_eq!(response.status, SftpStatus::Unavailable as i32);
    assert!(service.uploads.0.lock().unwrap().is_empty());
    let response = service
        .handle(
            SftpRequest {
                operation: SftpOperation::TerminalUploadGet as i32,
                session_id: source.as_uuid().as_bytes().to_vec(),
                transfer_id: id.as_uuid().as_bytes().to_vec(),
                ..Default::default()
            },
            &registry,
        )
        .await;
    assert_eq!(response.status, SftpStatus::Unavailable as i32);
}

#[test]
fn retry_excludes_completed_batches_and_refuses_uncertain_publication() {
    use cshell_domain::SftpQueueItemId;
    let mut context = RetryContext::new(
        Plan {
            files: (0..5)
                .map(|i| (format!("C:/local/{i}"), format!("{i}"), 10))
                .collect(),
            directories: vec![],
            bytes: 50,
            has_directories: false,
        },
        "/original".into(),
        Default::default(),
    );
    context.next = 3;
    context.base_bytes = 30;
    assert!(
        !context.safe(),
        "an unsettled journal cannot authorize a retry"
    );
    let item = |i, state| SftpQueueItem {
        id: SftpQueueItemId::new(),
        draft: SftpQueueDraft {
            local_path: format!("C:/local/{i}"),
            remote_path: format!("/original/{i}"),
            verified: true,
            verification: Some(Default::default()),
            ..Default::default()
        },
        state,
        verified: Some(Default::default()),
        transfer_id: None,
        receipt: None,
    };
    let mut queue = SftpQueueSnapshot::initial(SessionId::new());
    queue.items = vec![
        item(3, SftpQueueState::Succeeded),
        item(4, SftpQueueState::Unknown),
    ];
    context.queue = Some(queue.clone());
    assert_eq!(context.counts(), (4, 1));
    assert!(context.safe());
    assert!(failed_queue_item(&queue).is_some());
    queue.retry_items.insert(queue.items[1].id);
    assert!(
        failed_queue_item(&queue).is_none(),
        "a confirmed retry awaits dispatch, not the previous failure receipt"
    );
    assert!(retry::validate_chunk(&queue, &context.plan.files[3..], "/original").is_ok());
    assert!(retry::validate_chunk(&queue, &context.plan.files[3..], "/changed").is_err());
    let verification = queue.items[1].verified.as_mut().unwrap();
    verification.intent = SftpVerifiedIntent::Publish;
    context.queue = Some(queue.clone());
    assert!(
        !context.safe(),
        "an unacknowledged publish must never be blindly replayed"
    );
    assert!(
        failed_queue_item(&queue).is_some(),
        "a retry marker must not hide an uncertain publication"
    );
    for state in [
        SftpQueueState::Running,
        SftpQueueState::CleanupRequired,
        SftpQueueState::Cancelled,
    ] {
        queue.items[1].state = state;
        context.queue = Some(queue.clone());
        assert!(!context.safe());
    }
    context.clear_completed = true;
    assert_eq!(
        context.counts(),
        (3, 2),
        "do not count a cleared prior batch twice"
    );
}
