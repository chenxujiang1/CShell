use super::*;
use cshell_application::*;

async fn request(
    f: &Fixture,
    session: SessionId,
    doc: Option<SessionId>,
    operation: SftpOperation,
    revision: u64,
    data: Vec<u8>,
) -> SftpResponse {
    let mutation = operation == SftpOperation::EditSave || operation == SftpOperation::EditRebase;
    let envelope::Payload::SftpResponse(reply) = f
        .call(envelope::Payload::SftpRequest(SftpRequest {
            session_id: session.as_uuid().as_bytes().to_vec(),
            transfer_id: doc
                .map(|d| d.as_uuid().as_bytes().to_vec())
                .unwrap_or_default(),
            operation: operation as i32,
            expected_revision: revision,
            confirmed: mutation,
            document_json: data,
            ..Default::default()
        }))
        .await
    else {
        panic!("SFTP reply")
    };
    reply
}
fn decode(reply: SftpResponse) -> SftpWorkspaceReply {
    assert_eq!(reply.status, SftpStatus::Ok as i32, "{}", reply.detail);
    serde_json::from_slice(&reply.document_json).expect("workspace reply")
}
async fn ready(f: &Fixture, session: SessionId, id: SessionId) -> SftpEditDocument {
    for _ in 0..180 {
        if let SftpWorkspaceReply::Edit { document, .. } =
            decode(request(f, session, Some(id), SftpOperation::EditGet, 0, vec![]).await)
            && document.state == SftpEditState::Ready
            && document.queued_item.is_none()
        {
            return document;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("editor did not become ready")
}
async fn review(
    f: &Fixture,
    session: SessionId,
    doc: &SftpEditDocument,
    backup: Option<cshell_domain::SftpQueueItemId>,
) -> SftpEditReview {
    let reply = request(
        f,
        session,
        Some(doc.id),
        SftpOperation::EditReview,
        0,
        serde_json::to_vec(&SftpEditReviewRequest {
            revision: doc.revision,
            backup,
        })
        .unwrap(),
    )
    .await;
    let SftpWorkspaceReply::Review(value) = decode(reply) else {
        panic!("review reply")
    };
    value.validate().unwrap();
    value
}
async fn write_remote(client: &cshell_sftp::SftpClient, path: &str, bytes: &[u8]) {
    let mut reader = bytes;
    client
        .upload_atomic(
            &mut reader,
            path,
            Some(bytes.len() as u64),
            &cshell_sftp::TransferControl::default(),
            |_| {},
        )
        .await
        .unwrap();
}
async fn save(
    f: &Fixture,
    session: SessionId,
    doc: &SftpEditDocument,
    text: &str,
) -> SftpEditDocument {
    let reply = request(f, session, None, SftpOperation::QueueGet, 0, vec![]).await;
    assert_eq!(reply.status, SftpStatus::Ok as i32, "{}", reply.detail);
    let queue: SftpQueueSnapshot = serde_json::from_slice(&reply.document_json).unwrap();
    decode(
        request(
            f,
            session,
            Some(doc.id),
            SftpOperation::EditSave,
            doc.revision,
            serde_json::to_vec(&SftpEditSave {
                content: Some(text.into()),
                queue_revision: queue.revision,
                accept_exclusive_replace: true,
            })
            .unwrap(),
        )
        .await,
    );
    let updated = ready(f, session, doc.id).await;
    assert_eq!(updated.base, sftp_digest(text.as_bytes()));
    updated
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires isolated CSHELL_RUNTIME_DIR"]
async fn product_workspace_edit_conflict_and_verified_backup_restore() {
    let f = Fixture::new(None).await;
    let original = id(&f.create().await);
    let session = SessionId::new();
    assert_eq!(
        f.sftp(SftpOperation::ConnectProfile, session, Some(original))
            .await
            .status,
        SftpStatus::Ok as i32
    );
    let shell = f.service.ssh_sessions.get(original).unwrap();
    let client = shell.open_sftp().await.unwrap();
    let path = "/srv/one/editor.conf";
    write_remote(&client, path, b"version=1\n").await;
    let SftpWorkspaceReply::Edit { document, .. } = decode(
        request(
            &f,
            session,
            None,
            SftpOperation::EditOpen,
            0,
            serde_json::to_vec(&SftpBrowseOptions {
                local: false,
                path: path.into(),
                links: SftpLinkPolicy::NoFollow,
            })
            .unwrap(),
        )
        .await,
    ) else {
        panic!()
    };
    let doc = ready(&f, session, document.id).await;
    let first = review(&f, session, &doc, None).await;
    assert_eq!(first.base.as_deref(), Some("version=1\n"));
    assert_eq!(first.remote, "version=1\n");
    let doc = save(&f, session, &doc, "version=2\n").await;
    let checked = review(&f, session, &doc, None).await;
    assert_eq!(checked.backups.len(), 1);
    let backup = checked.backups[0].item;
    client.remove_entry(path, false).await.unwrap();
    write_remote(&client, path, b"version=3\n").await;
    let changed = review(&f, session, &doc, None).await;
    assert_eq!(changed.base.as_deref(), Some("version=2\n"));
    assert_ne!(changed.remote_content, doc.base);
    let queue_reply = request(&f, session, None, SftpOperation::QueueGet, 0, vec![]).await;
    let queue: SftpQueueSnapshot = serde_json::from_slice(&queue_reply.document_json).unwrap();
    let rejected = request(
        &f,
        session,
        Some(doc.id),
        SftpOperation::EditSave,
        doc.revision,
        serde_json::to_vec(&SftpEditSave {
            content: Some("local merge\n".into()),
            queue_revision: queue.revision,
            accept_exclusive_replace: true,
        })
        .unwrap(),
    )
    .await;
    assert_ne!(rejected.status, SftpStatus::Ok as i32);
    assert!(rejected.detail.contains("服务器文件已变化"));
    let retained = ready(&f, session, doc.id).await;
    assert_eq!(retained.revision, doc.revision);
    assert_eq!(retained.current, doc.current);
    client.remove_entry(path, false).await.unwrap();
    write_remote(&client, path, b"version=4\n").await;
    let stale = request(
        &f,
        session,
        Some(doc.id),
        SftpOperation::EditRebase,
        doc.revision,
        serde_json::to_vec(&SftpEditRebase {
            remote: changed.remote_content,
            permissions: changed.remote_permissions,
        })
        .unwrap(),
    )
    .await;
    assert_ne!(
        stale.status,
        SftpStatus::Ok as i32,
        "stale confirmation must be refused"
    );
    let current = review(&f, session, &doc, Some(backup)).await;
    assert_eq!(current.backup_text.as_deref(), Some("version=1\n"));
    let SftpWorkspaceReply::Edit {
        document: rebased, ..
    } = decode(
        request(
            &f,
            session,
            Some(doc.id),
            SftpOperation::EditRebase,
            doc.revision,
            serde_json::to_vec(&SftpEditRebase {
                remote: current.remote_content,
                permissions: current.remote_permissions,
            })
            .unwrap(),
        )
        .await,
    )
    else {
        panic!()
    };
    assert_eq!(rebased.base, sftp_digest(b"version=4\n"));
    assert_eq!(
        rebased.current, doc.current,
        "rebase must preserve working draft"
    );
    let doc = save(&f, session, &rebased, current.backup_text.as_ref().unwrap()).await;
    let restored = review(&f, session, &doc, None).await;
    assert_eq!(restored.remote, "version=1\n");
    assert!(
        restored
            .backups
            .iter()
            .any(|b| b.content == sftp_digest(b"version=4\n")),
        "recovery must back up current remote content"
    );
    client.close().await.unwrap();
    f.counts(1);
    f.sftp(SftpOperation::Disconnect, session, None).await;
    f.close(original).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires isolated CSHELL_RUNTIME_DIR"]
async fn product_workspace_sync_exclusions_and_mtime_roundtrip() {
    let f = Fixture::new(None).await;
    let original = id(&f.create().await);
    let session = SessionId::new();
    assert_eq!(
        f.sftp(SftpOperation::ConnectProfile, session, Some(original))
            .await
            .status,
        SftpStatus::Ok as i32
    );
    let shell = f.service.ssh_sessions.get(original).unwrap();
    let client = shell.open_sftp().await.unwrap();
    client
        .create_directory("/srv/one/presets", 0o755)
        .await
        .unwrap();
    let local = f._root.path().join("presets");
    std::fs::create_dir_all(local.join("nested/empty")).unwrap();
    std::fs::create_dir(local.join(".git")).unwrap();
    for (path, bytes) in [
        ("nested/keep.txt", b"keep".as_slice()),
        (".git/config", b"secret"),
        ("nested/skip.tmp", b"skip"),
    ] {
        std::fs::write(local.join(path), bytes).unwrap();
    }
    let seconds = 1_700_000_123;
    std::fs::File::options()
        .write(true)
        .open(local.join("nested/keep.txt"))
        .unwrap()
        .set_modified(std::time::UNIX_EPOCH + Duration::from_secs(seconds))
        .unwrap();
    let options = SftpSyncOptions {
        local: local.to_string_lossy().into(),
        remote: "/srv/one/presets".into(),
        direction: SftpQueueDirection::Upload,
        verification: Default::default(),
        compare_content: true,
        excludes: vec![".git/**".into(), "*.tmp".into()],
        preserve_modified: true,
    };
    for direction in [SftpQueueDirection::Upload, SftpQueueDirection::Download] {
        let destination = f._root.path().join("download-presets");
        if direction == SftpQueueDirection::Download {
            std::fs::create_dir(&destination).unwrap();
        }
        let mut options = options.clone();
        options.direction = direction;
        if direction == SftpQueueDirection::Download {
            options.local = destination.to_string_lossy().into();
        }
        let task = SessionId::new();
        let reply = sync_call(
            &f,
            session,
            task,
            SftpOperation::SyncScan,
            serde_json::to_vec(&options).unwrap(),
        )
        .await;
        assert_eq!(reply.status, SftpStatus::Ok as i32, "{}", reply.detail);
        let preview: SftpSyncPreview = serde_json::from_slice(&reply.document_json).unwrap();
        assert!(
            preview
                .entries
                .iter()
                .all(|e| !e.relative.contains(".git") && !e.relative.ends_with(".tmp"))
        );
        let apply = SftpSyncApply {
            selected: preview
                .entries
                .iter()
                .filter(|e| e.eligible(direction, false))
                .map(|e| e.relative.clone())
                .collect(),
            backup_replace: false,
        };
        let reply = sync_call(
            &f,
            session,
            task,
            SftpOperation::SyncApply,
            serde_json::to_vec(&apply).unwrap(),
        )
        .await;
        assert_eq!(reply.status, SftpStatus::Ok as i32, "{}", reply.detail);
        let result = tree_wait(&f, session, task).await;
        assert_eq!(
            result.state,
            TerminalUploadState::Succeeded,
            "{}",
            result.detail
        );
        if direction == SftpQueueDirection::Upload {
            assert_eq!(
                client
                    .metadata(
                        "/srv/one/presets/nested/keep.txt",
                        cshell_sftp::SymlinkPolicy::NoFollow
                    )
                    .await
                    .unwrap()
                    .modified_unix_seconds,
                Some(seconds as u32)
            );
        } else {
            assert_eq!(
                std::fs::read(destination.join("nested/keep.txt")).unwrap(),
                b"keep"
            );
            assert_eq!(
                std::fs::metadata(destination.join("nested/keep.txt"))
                    .unwrap()
                    .modified()
                    .unwrap()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_secs(),
                seconds
            );
            assert!(!destination.join(".git").exists());
        }
    }
    client.close().await.unwrap();
    f.counts(1);
    f.sftp(SftpOperation::Disconnect, session, None).await;
    f.close(original).await;
}
