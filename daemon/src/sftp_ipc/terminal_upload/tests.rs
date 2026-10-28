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
    assert!(result.directories.contains(&"上传目录/nested/empty".into()));
    control.cancel();
    assert!(plan(&paths, &control).is_err());
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
