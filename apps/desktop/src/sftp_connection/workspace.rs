use super::*;
pub(super) fn valid_edit_review(view:&SftpView,wire:SessionId,command:&WorkspaceCommand)->bool {
    command.revision==0 && view.edit.as_ref().is_some_and(|edit|edit.session==wire && !edit.closed
        && edit.id.as_uuid().as_bytes().as_slice()==command.object.as_slice()
        && serde_json::from_slice::<cshell_application::SftpEditReviewRequest>(&command.data).is_ok_and(|r|r.revision==edit.revision))
}
fn mutation(op: SftpOperation) -> bool {
    matches!(
        op,
        SftpOperation::FileApply
            | SftpOperation::FileJobCancel
            | SftpOperation::EditLaunch
            | SftpOperation::EditSave
            | SftpOperation::EditClose
            | SftpOperation::EditRebase
            | SftpOperation::EditRebind
            | SftpOperation::QueueMigrate
            | SftpOperation::QueueAddMany
            | SftpOperation::WorkspaceForget
    )
}
pub(super) async fn transfer_one(
    config: &DesktopConnectionConfig,
    shared: &Arc<Mutex<SftpView>>,
    session: SessionId,
    draft: SftpQueueDraft,
    revision: u64,
) {
    let valid = {
        let view = shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        view.session_id == Some(session)
            && view.queue_writable
            && view.queue.as_ref().is_some_and(|q| q.revision == revision)
    };
    if !valid {
        publish_queue(
            shared,
            session,
            Err("队列版本变化，请重新载入".into()),
            false,
            Some(revision),
        );
        return;
    }
    let query = SftpRequest {
        operation: SftpOperation::QueueAddManaged as i32,
        session_id: session.as_uuid().as_bytes().to_vec(),
        document_json: serde_json::to_vec(&draft).unwrap_or_default(),
        expected_revision: revision,
        confirmed: true,
        ..Default::default()
    };
    let added = send(config, query).await;
    let parsed = added.as_ref().map_err(|e| e.clone()).and_then(|r| {
        if r.status != SftpStatus::Ok as i32 {
            return Err(r.detail.clone());
        }
        decode_sftp_queue(&r.document_json).map_err(str::to_owned)
    });
    let Ok(queue) = parsed else {
        publish_queue(shared, session, added, false, Some(revision));
        return;
    };
    if queue.session_id != session
        || revision.checked_add(1) != Some(queue.revision)
        || queue.items.last().is_none_or(|i| {
            i.draft != draft || i.state != cshell_application::SftpQueueState::Pending
        })
    {
        publish_queue(
            shared,
            session,
            Err("新增条目的身份或版本不匹配；重新载入，不重复添加".into()),
            false,
            Some(revision),
        );
        return;
    }
    let item = queue.items.last().map(|i| i.id);
    let Some(item) = item else {
        return;
    };
    let start = SftpRequest {
        operation: SftpOperation::QueueStartOne as i32,
        session_id: session.as_uuid().as_bytes().to_vec(),
        transfer_id: item.as_uuid().as_bytes().to_vec(),
        expected_revision: queue.revision,
        confirmed: true,
        ..Default::default()
    };
    let result = send(config, start).await;
    if result
        .as_ref()
        .is_ok_and(|r| r.status != SftpStatus::Ok as i32)
    {
        publish_queue(shared, session, added, false, Some(revision));
        let mut view = shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if view.session_id==Some(session) {view.status = "文件已保存；已有任务结束后，从队列明确启动此条".into();}
    } else {
        publish_queue(shared, session, result, false, Some(queue.revision));
    }
}
pub(super) async fn run_command(
    config: &DesktopConnectionConfig,
    shared: &Arc<Mutex<SftpView>>,
    scope: SessionId,
    command: WorkspaceCommand,
) {
    let wire = command.source_session.unwrap_or(scope);
    let ready = {
        let view = shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        view.session_id == Some(scope)
            && view.workspace_epoch == command.epoch
            && match command.operation {
                SftpOperation::EditReview => valid_edit_review(&view,wire,&command),
                SftpOperation::FileApply | SftpOperation::FileJobCancel => {
                    view.file_writable
                        && view.file_job.as_ref().is_some_and(|j| {
                            j.session == wire
                                && j.revision == command.revision
                                && j.id.as_uuid().as_bytes().as_slice() == command.object.as_slice()
                        })
                }
                SftpOperation::EditLaunch
                | SftpOperation::EditSave
                | SftpOperation::EditClose
                | SftpOperation::EditRebase
            | SftpOperation::EditRebind => {
                    view.edit_writable
                        && view.edit.as_ref().is_some_and(|e| {
                            e.revision == command.revision
                                && e.id.as_uuid().as_bytes().as_slice() == command.object.as_slice()
                                && (command.operation == SftpOperation::EditRebind
                                    || e.session == wire)
                        })
                }
                SftpOperation::QueueAddMany => {
                    view.queue_writable
                        && !view.queue_busy
                        && view
                            .queue
                            .as_ref()
                            .is_some_and(|q| q.revision == command.revision)
                }
                SftpOperation::QueueMigrate => {
                    view.queue_writable
                        && !view.queue_busy
                        && serde_json::from_slice::<cshell_application::SftpQueueMigration>(
                            &command.data,
                        )
                        .is_ok_and(|p| {
                            p.target == scope
                                && view
                                    .queue
                                    .as_ref()
                                    .is_some_and(|q| q.revision == p.target_revision)
                                && view.peer.as_ref() == Some(&p.peer)
                        })
                }
                _ => true,
            }
    };
    if !ready {
        fault(
            shared,
            scope,
            true,
            mutation(command.operation),
            command.operation,
            "状态或权限代次变化；请重新载入后操作".into(),
        );
        return;
    }
    let query = SftpRequest {
        operation: command.operation as i32,
        session_id: wire.as_uuid().as_bytes().to_vec(),
        document_json: command.data.clone(),
        transfer_id: command.object.clone(),
        expected_revision: command.revision,
        confirmed: mutation(command.operation),
        ..Default::default()
    };
    let result = send(config, query).await.and_then(|reply| {
        if reply.status != SftpStatus::Ok as i32 {
            return Err(reply.detail);
        }
        if !reply.entries.is_empty() || reply.transfer.is_some() || !reply.browse_id.is_empty() {
            return Err("工作区回复形状无效".into());
        }
        decode_sftp_workspace(&reply.document_json)
            .map_err(str::to_owned)
            .or_else(|error| {
                if command.operation == SftpOperation::QueueGet {
                    decode_sftp_queue(&reply.document_json)
                        .map(SftpWorkspaceReply::Queue)
                        .map_err(str::to_owned)
                } else {
                    Err(error)
                }
            })
    });
    publish(shared, scope, wire, &command, result, true);
}
fn fault(
    shared: &Arc<Mutex<SftpView>>,
    scope:SessionId,
    explicit: bool,
    uncertain: bool,
    operation: SftpOperation,
    message: String,
) {
    let mut view = shared
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if view.session_id!=Some(scope) {return;}
    if explicit {
        view.workspace_busy = false;
    }
    view.generation = view.generation.wrapping_add(1);
    view.error = Some(message);
    if uncertain {
        view.workspace_epoch = view.workspace_epoch.wrapping_add(1);
        view.file_writable = false;
        view.edit_writable = false;
        if matches!(
            operation,
            SftpOperation::EditSave | SftpOperation::QueueMigrate | SftpOperation::QueueAddMany
        ) {
            view.queue_writable = false;
        }
    }
}
fn publish(
    shared: &Arc<Mutex<SftpView>>,
    scope: SessionId,
    wire: SessionId,
    command: &WorkspaceCommand,
    result: Result<SftpWorkspaceReply, String>,
    explicit: bool,
) {
    let result = result.and_then(|reply| {
        let object = |id: SessionId| {
            command.object.is_empty()
                || id.as_uuid().as_bytes().as_slice() == command.object.as_slice()
        };
        let valid = match &reply {
            SftpWorkspaceReply::Review(value)=>command.operation==SftpOperation::EditReview&&value.session==wire&&object(value.id)&&serde_json::from_slice::<cshell_application::SftpEditReviewRequest>(&command.data).is_ok_and(|r|r.revision==value.revision&&r.backup==value.selected_backup),
            SftpWorkspaceReply::File(j) => {
                matches!(
                    command.operation,
                    SftpOperation::FileInspect
                        | SftpOperation::FileApply
                        | SftpOperation::FileJobGet
                        | SftpOperation::FileJobCancel
                ) && j.session == wire
                    && object(j.id)
                    && j.revision >= command.revision
            }
            SftpWorkspaceReply::Edit { document, .. } => {
                matches!(
                    command.operation,
                    SftpOperation::EditOpen
                        | SftpOperation::EditGet
                        | SftpOperation::EditLaunch
                        | SftpOperation::EditSave
                        | SftpOperation::EditClose
                        | SftpOperation::EditRebase
            | SftpOperation::EditRebind
                ) && document.session == wire
                    && object(document.id)
                    && document.revision >= command.revision
            }
            SftpWorkspaceReply::Queue(q) => {
                if command.operation == SftpOperation::QueueGet {
                    q.session_id == wire
                } else {
                    matches!(
                        command.operation,
                        SftpOperation::QueueMigrate | SftpOperation::QueueAddMany
                    ) && q.session_id == scope
                        && (command.operation == SftpOperation::QueueMigrate
                            || command.revision.checked_add(1) == Some(q.revision))
                }
            }
            SftpWorkspaceReply::Peer { session, .. } => {
                command.operation == SftpOperation::PeerGet && *session == wire
            }
            SftpWorkspaceReply::History {
                session, offset, ..
            } => {
                command.operation == SftpOperation::WorkspaceList
                    && *session == wire
                    && serde_json::from_slice::<cshell_application::SftpWorkspacePage>(
                        &command.data,
                    )
                    .is_ok_and(|p| p.offset == *offset)
            }
            SftpWorkspaceReply::Forgotten { session, id, .. } => {
                command.operation == SftpOperation::WorkspaceForget
                    && *session == wire
                    && object(*id)
            }
        };
        if !valid {
            Err("工作区回复身份、版本或操作不匹配".into())
        } else {
            Ok(reply)
        }
    });
    let Ok(reply) = result else {
        fault(
            shared,
            scope,
            explicit,
            explicit && mutation(command.operation),
            command.operation,
            result.err().unwrap_or_else(|| "工作区不可用".into()),
        );
        return;
    };
    let mut view = shared
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if view.session_id != Some(scope) {
        return;
    }
    if explicit {
        view.workspace_busy = false;
    }
    let authority = explicit && command.epoch == view.workspace_epoch;
    match reply {
        SftpWorkspaceReply::Review(value)=>{
            if authority && view.edit.as_ref().is_some_and(|edit|edit.id==value.id&&edit.revision==value.revision&&edit.session==value.session){view.edit_writable=true;}
            view.edit_review=Some(Arc::new(value));
        }
        SftpWorkspaceReply::File(job) => {
            if view
                .file_job
                .as_ref()
                .is_some_and(|old| old.id == job.id && old.revision > job.revision)
            {
                view.error = Some("文件操作版本回退；请重新载入".into());
                return;
            }
            view.file_job = Some(Arc::new(job));
            if authority
                && matches!(
                    command.operation,
                    SftpOperation::FileInspect | SftpOperation::FileJobGet
                )
            {
                view.file_writable = true;
            }
        }
        SftpWorkspaceReply::Edit { document, text } => {
            if view
                .edit
                .as_ref()
                .is_some_and(|old| old.id == document.id && old.revision > document.revision)
            {
                view.error = Some("编辑记录版本回退；请重新载入".into());
                return;
            }
            view.edit_text = text.map(Arc::new).or_else(|| {
                view.edit_text.take().filter(|text| {
                    document.state == SftpEditState::Ready
                        && cshell_application::sftp_digest(text.as_bytes()) == document.current
                })
            });
            view.edit = Some(Arc::new(document));
            if authority
                && matches!(
                    command.operation,
                    SftpOperation::EditRebase | SftpOperation::EditOpen | SftpOperation::EditGet | SftpOperation::EditRebind
                )
            {
                view.edit_writable = true;
            }
            if command.operation == SftpOperation::EditSave {
                view.queue_writable = false;
            }
        }
        SftpWorkspaceReply::Queue(queue) => {
            if command.operation == SftpOperation::QueueGet && wire != scope {
                view.migration_source = Some(Arc::new(queue));
            } else {
                view.queue = Some(Arc::new(queue));
                if authority {
                    view.queue_writable = true;
                }
            }
        }
        SftpWorkspaceReply::Peer { peer, .. } => view.peer = Some(peer),
        SftpWorkspaceReply::History {
            offset,
            more,
            items,
            ..
        } => {
            view.history = items;
            view.history_offset = offset;
            view.history_more = more;
        }
        SftpWorkspaceReply::Forgotten {
            id, retained_files, ..
        } => {
            if view.file_job.as_ref().is_some_and(|j| j.id == id) {
                view.file_job = None;
                view.file_writable = false;
            }
            if view.edit.as_ref().is_some_and(|e| e.id == id) {
                view.edit = None;
                view.edit_text = None;
                view.edit_writable = false;
            }
            view.history.retain(|i| i.id != id);
            view.status = if retained_files {
                "记录已移除；私有工作文件已保留"
            } else {
                "记录及所选私有文件已移除"
            }
            .into();
        }
    }
    view.error = None;
    view.generation = view.generation.wrapping_add(1);
}
pub(super) async fn poll(config: &DesktopConnectionConfig, shared: &Arc<Mutex<SftpView>>) {
    let requests = {
        let view = shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if view.workspace_busy {
            return;
        }
        let Some(scope) = view.session_id else {
            return;
        };
        let mut requests = Vec::new();
        if let Some(job) = &view.file_job
            && matches!(
                job.state,
                SftpFileState::Inspecting | SftpFileState::Applying
            )
        {
            requests.push((
                scope,
                job.session,
                SftpOperation::FileJobGet,
                job.id,
                view.workspace_epoch,
            ));
        }
        if let Some(edit) = &view.edit
            && !edit.closed
            && matches!(edit.state, SftpEditState::Preparing | SftpEditState::Ready)
        {
            requests.push((
                scope,
                edit.session,
                SftpOperation::EditGet,
                edit.id,
                view.workspace_epoch,
            ));
        }
        requests
    };
    for (scope, wire, operation, id, epoch) in requests {
        let command = WorkspaceCommand {
            operation,
            data: Vec::new(),
            revision: 0,
            object: id.as_uuid().as_bytes().to_vec(),
            source_session: Some(wire),
            epoch,
        };
        let query = SftpRequest {
            operation: operation as i32,
            session_id: wire.as_uuid().as_bytes().to_vec(),
            transfer_id: command.object.clone(),
            ..Default::default()
        };
        let result = send(config, query).await.and_then(|r| {
            if r.status != SftpStatus::Ok as i32 {
                return Err(r.detail);
            }
            decode_sftp_workspace(&r.document_json).map_err(str::to_owned)
        });
        publish(shared, scope, wire, &command, result, false);
    }
}

#[cfg(test)]
mod lifecycle_scope_tests {
    use super::*;
    #[test]
    fn old_workspace_fault_does_not_disable_new_connection_authority() {
        let new=SessionId::new();let shared=Arc::new(Mutex::new(SftpView{session_id:Some(new),workspace_epoch:12,workspace_busy:true,file_writable:true,..Default::default()}));
        fault(&shared,SessionId::new(),true,true,SftpOperation::FileApply,"old timeout".into());
        let view=shared.lock().unwrap();assert_eq!(view.workspace_epoch,12);assert!(view.workspace_busy);assert!(view.file_writable);assert!(view.error.is_none());
    }
}
