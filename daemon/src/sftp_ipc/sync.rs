//! Read-only recursive previews, bound to the original transport and a short expiry.
use super::*;
use cshell_application::*;
use cshell_sftp::SymlinkPolicy;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant, UNIX_EPOCH},
};

#[derive(Clone, Debug)]
pub(super) struct Plans {
    records: Arc<Mutex<BTreeMap<SessionId, Arc<Record>>>>,
    readers: Arc<tokio::sync::Semaphore>,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn local_scan_is_read_only_and_rejects_parent_paths_and_reserved_names() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("empty")).unwrap();
        std::fs::write(root.path().join("keep"), b"unchanged").unwrap();
        let inventory = local_inventory(root.path().to_str().unwrap(),&[]).unwrap();
        assert_eq!(inventory.len(), 2);
        assert!(inventory["empty"].directory);
        assert_eq!(
            std::fs::read(root.path().join("keep")).unwrap(),
            b"unchanged"
        );
        assert!(local_inventory(root.path().join("../other").to_str().unwrap(),&[]).is_err());
        let mut seen = BTreeSet::new();
        assert!(check_name("../escape", &mut seen).is_err());
        assert!(check_name("a/b", &mut seen).is_ok());
        assert!(check_name("a/b", &mut seen).is_err());
        #[cfg(windows)]
        assert!(check_name("CON.txt", &mut seen).is_err());
    }
    #[tokio::test]
    async fn unknown_source_and_noncanonical_requests_create_no_preview_or_job() {
        let service = SftpIpcService::default();
        let registry = SshSessionRegistry::default();
        let session = SessionId::new();
        let id = SessionId::new();
        let root = tempfile::tempdir().unwrap();
        let request = SftpRequest {
            operation: SftpOperation::SyncScan as i32,
            session_id: session.as_uuid().as_bytes().to_vec(),
            transfer_id: id.as_uuid().as_bytes().to_vec(),
            document_json: serde_json::to_vec(&SftpSyncOptions {
                local: root.path().to_str().unwrap().into(),
                remote: "/home".into(),
                direction: SftpQueueDirection::Upload,
                verification: Default::default(),
                compare_content: true,excludes:Vec::new(),preserve_modified:false,
            })
            .unwrap(),
            ..Default::default()
        };
        assert_ne!(
            service.handle(request.clone(), &registry).await.status,
            SftpStatus::Ok as i32
        );
        assert_ne!(
            service
                .handle(
                    SftpRequest {
                        confirmed: true,
                        ..request
                    },
                    &registry
                )
                .await
                .status,
            SftpStatus::Ok as i32
        );
        assert!(service.sync_plans.records.lock().unwrap().is_empty());
    }
}
impl Default for Plans {
    fn default() -> Self {
        Self {
            records: Default::default(),
            readers: Arc::new(tokio::sync::Semaphore::new(2)),
        }
    }
}
struct Record {
    preview: SftpSyncPreview,
    metadata_fingerprint: [u8; 32],
    checked: BTreeMap<String, (SftpVerifiedContent, SftpVerifiedContent)>,
    source: Arc<SshSession>,
    epoch: u64,
    created: Instant,
    claimed: AtomicBool,
}
impl std::fmt::Debug for Record {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SyncPreview").finish_non_exhaustive()
    }
}
type Inventory = BTreeMap<String, SftpSyncMetadata>;
fn metadata_fingerprint(entries: &[SftpSyncEntry]) -> Result<[u8; 32], String> {
    let bytes = serde_json::to_vec(entries).map_err(|_| "比较元数据编码失败")?;
    if bytes.len() > 4 * 1024 * 1024 {
        return Err("比较结果过大，请缩小范围".into());
    }
    Ok(Sha256::digest(bytes).into())
}
async fn compare_contents(
    source: &Arc<SshSession>,
    options: &SftpSyncOptions,
    entries: &mut [SftpSyncEntry],
) -> BTreeMap<String, (SftpVerifiedContent, SftpVerifiedContent)> {
    let mut checked = BTreeMap::new();
    if !options.compare_content {
        return checked;
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut files = 0usize;
    let mut bytes = 0u64;
    for entry in entries {
        let (Some(local), Some(remote)) = (&entry.local, &entry.remote) else {
            continue;
        };
        if entry.difference != SftpSyncDifference::Changed
            || local.directory
            || remote.directory
            || local.size != remote.size
            || local.size > 1024 * 1024
        {
            continue;
        }
        if files >= 32
            || bytes.saturating_add(local.size) > 8 * 1024 * 1024
            || tokio::time::Instant::now() >= deadline
        {
            break;
        }
        files += 1;
        bytes += local.size;
        let local_path = local_child(&options.local, &entry.relative);
        let remote_path = remote_child(&options.remote, &entry.relative);
        let limit = (tokio::time::Instant::now() + Duration::from_secs(2)).min(deadline);
        let result = tokio::time::timeout_at(limit, async {
            let mut file = super::verified::LocalSource::open_bounded(
                std::path::Path::new(&local_path),
                1024 * 1024,
            )
            .await?;
            let a = file.digest().await?;
            if a.size != local.size {
                return Err("本地文件已变化");
            }
            let client = source.open_sftp().await.map_err(|_| "读取连接不可用")?;
            let mut reader = client.staged_upload();
            let b = reader.probe(&remote_path, 1024 * 1024).await;
            reader.close().await;
            let b = b.map_err(|_| "远端内容核对失败")?.ok_or("远端文件已变化")?;
            if b.size != remote.size {
                return Err("远端文件已变化");
            }
            Ok((
                a,
                SftpVerifiedContent {
                    size: b.size,
                    sha256: b.sha256,
                },
            ))
        })
        .await;
        if let Ok(Ok((a, b))) = result {
            entry.difference = if a == b {
                SftpSyncDifference::ContentMatch
            } else {
                SftpSyncDifference::ContentChanged
            };
            if a != b {
                checked.insert(entry.relative.clone(), (a, b));
            }
        }
    }
    checked
}
fn options_valid(v: &SftpSyncOptions) -> bool {
    valid_sftp_exclusions(&v.excludes) && v.local.len() <= 4096
        && v.remote.len() <= 4096
        && !v.local.chars().any(char::is_control)
        && sftp_remote_path(&v.remote, true)
        && PathBuf::from(&v.local).is_absolute()
}
fn remote_child(root: &str, relative: &str) -> String {
    format!("{}/{}", root.trim_end_matches('/'), relative)
}
fn local_child(root: &str, relative: &str) -> String {
    PathBuf::from(root)
        .join(relative.replace('/', std::path::MAIN_SEPARATOR_STR))
        .to_string_lossy()
        .into_owned()
}
fn check_name(relative: &str, seen: &mut BTreeSet<String>) -> Result<(), String> {
    if relative.len() > 3500
        || relative.split('/').count() > 64
        || !relative.split('/').all(super::tree::name)
        || !seen.insert(if cfg!(windows) {
            relative.to_lowercase()
        } else {
            relative.into()
        })
    {
        return Err("目录含不支持的名称、重复目标或超过 64 层；请缩小比较范围".into());
    }
    if seen.len() > 10000 {
        return Err("每侧最多比较 10000 项，请缩小范围".into());
    }
    Ok(())
}
fn local_inventory(root: &str,rules:&[String]) -> Result<Inventory, String> {
    super::tree::local_directory(std::path::Path::new(root), false)?;
    let mut out = Inventory::new();
    let mut seen = BTreeSet::new();
    let mut pending = vec![(PathBuf::from(root), String::new())];
    while let Some((directory, prefix)) = pending.pop() {
        super::tree::local_directory(&directory, false)?;
        for entry in std::fs::read_dir(&directory).map_err(|_| "无法读取本地目录")? {
            let entry = entry.map_err(|_| "本地目录已变化")?;
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| "本地名称无法编码")?;
            let relative = if prefix.is_empty() {
                name
            } else {
                format!("{prefix}/{name}")
            };
            check_name(&relative, &mut seen)?;
            if sftp_excluded(&relative,rules){continue;}
            let m = std::fs::symlink_metadata(entry.path()).map_err(|_| "本地文件已变化")?;
            #[cfg(windows)]
            {
                use std::os::windows::fs::MetadataExt;
                if m.file_attributes() & 0x400 != 0 {
                    return Err("比较目录含链接或联接；请选择普通目录".into());
                }
            }
            if m.file_type().is_symlink() || !m.is_file() && !m.is_dir() {
                return Err("比较目录含链接或特殊文件；请选择普通目录".into());
            }
            if m.is_file() && m.len() > MAX_SFTP_VERIFIED_BYTES {
                return Err("单文件超过 64 GiB".into());
            }
            out.insert(
                relative.clone(),
                SftpSyncMetadata {
                    directory: m.is_dir(),
                    size: if m.is_dir() { 0 } else { m.len() },
                    modified: if m.is_dir() {
                        None
                    } else {
                        m.modified()
                            .ok()
                            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                            .map(|v| v.as_secs())
                    },
                },
            );
            if m.is_dir() {
                pending.push((entry.path(), relative));
            }
        }
    }
    Ok(out)
}
async fn inventory(
    source: &Arc<SshSession>,
    options: &SftpSyncOptions,
) -> Result<(Inventory, Inventory), String> {
    let local = options.local.clone();let rules=options.excludes.clone();
    let a = tokio::task::spawn_blocking(move || local_inventory(&local,&rules))
        .await
        .map_err(|_| "本地比较线程失败")??;
    let client = source.open_sftp().await.map_err(|_| "SFTP 连接不可用")?;
    let result: Result<Inventory, String> = async {
        let root = client
            .metadata(options.remote.clone(), SymlinkPolicy::NoFollow)
            .await
            .map_err(|_| "无法读取远端根目录")?;
        if root.file_type != RemoteFileType::Directory {
            return Err("远端根路径必须是普通目录".into());
        }
        let mut out = Inventory::new();
        let mut seen = BTreeSet::new();
        let mut pending = vec![String::new()];
        while let Some(prefix) = pending.pop() {
            let directory = if prefix.is_empty() {
                options.remote.clone()
            } else {
                remote_child(&options.remote, &prefix)
            };
            let directory_client = source
                .open_sftp()
                .await
                .map_err(|_| "无法打开目录读取通道")?;
            let mut cursor = directory_client
                .directory_cursor(directory)
                .await
                .map_err(|_| "无法读取远端目录")?;
            loop {
                let page = cursor
                    .next_page(128)
                    .await
                    .map_err(|_| "远端目录读取失败")?;
                for entry in page.entries {
                    let relative = if prefix.is_empty() {
                        entry.name
                    } else {
                        format!("{prefix}/{}", entry.name)
                    };
                    check_name(&relative, &mut seen)?;
                    if sftp_excluded(&relative,&options.excludes){continue;}
                    let directory = match entry.metadata.file_type {
                        RemoteFileType::Directory => true,
                        RemoteFileType::File => false,
                        _ => return Err("远端含链接或特殊文件；请缩小比较范围".into()),
                    };
                    let size = if directory {
                        0
                    } else {
                        entry.metadata.size.ok_or("远端未提供文件大小")?
                    };
                    if size > MAX_SFTP_VERIFIED_BYTES {
                        return Err("单文件超过 64 GiB".into());
                    }
                    out.insert(
                        relative.clone(),
                        SftpSyncMetadata {
                            directory,
                            size,
                            modified: if directory {
                                None
                            } else {
                                entry.metadata.modified_unix_seconds.map(u64::from)
                            },
                        },
                    );
                    if directory {
                        pending.push(relative);
                    }
                }
                if !page.truncated {
                    break;
                }
            }
            let _ = cursor.close().await;
        }
        Ok(out)
    }
    .await;
    let _ = client.close().await;
    Ok((a, result?))
}
impl SftpIpcService {
    pub(super) async fn sync_request(
        &self,
        request: SftpRequest,
        session: SessionId,
        op: SftpOperation,
        sessions: &SshSessionRegistry,
    ) -> SftpResponse {
        let result = self.sync_inner(&request, session, op, sessions).await;
        match result {
            Ok(document_json) => SftpResponse {
                document_json,
                ..Default::default()
            },
            Err(error) => SftpResponse::with_status(SftpStatus::Conflict, error),
        }
    }
    async fn sync_inner(
        &self,
        request: &SftpRequest,
        session: SessionId,
        op: SftpOperation,
        sessions: &SshSessionRegistry,
    ) -> Result<Vec<u8>, String> {
        let id = SessionId::from_bytes(
            request
                .transfer_id
                .as_slice()
                .try_into()
                .map_err(|_| "比较标识无效")?,
        );
        let expected = SftpRequest {
            operation: op as i32,
            session_id: request.session_id.clone(),
            transfer_id: request.transfer_id.clone(),
            document_json: request.document_json.clone(),
            confirmed: op == SftpOperation::SyncApply,
            ..Default::default()
        };
        if request != &expected
            || id == session
            || id.as_uuid().is_nil()
            || request.document_json.len() > 1024 * 1024
        {
            return Err("比较请求无效".into());
        }
        let _permit = self
            .sync_plans
            .readers
            .clone()
            .try_acquire_owned()
            .map_err(|_| "比较任务已满，请稍后重试")?;
        if op == SftpOperation::SyncScan {
            let options: SftpSyncOptions =
                serde_json::from_slice(&request.document_json).map_err(|_| "比较参数无效")?;
            if !options_valid(&options) {
                return Err("请选择有效的本地和远端目录".into());
            }
            let source = sessions
                .get_sftp(session)
                .map_err(|_| "原 SFTP 会话不可用")?;
            if !source.running() {
                return Err("SFTP 已断开，请重新连接".into());
            }
            let epoch = source.generation();
            let (local, remote) =
                tokio::time::timeout(Duration::from_secs(45), inventory(&source, &options))
                    .await
                    .map_err(|_| "目录比较超时，请缩小范围")??;
            if !source.running() || source.generation() != epoch {
                return Err("比较期间连接发生变化，请重新比较".into());
            }
            let mut entries = sftp_sync_entries(&local, &remote);
            let metadata_fingerprint = metadata_fingerprint(&entries)?;
            let checked = compare_contents(&source, &options, &mut entries).await;
            if !source.running() || source.generation() != epoch {
                return Err("内容核对期间连接发生变化，请重新比较".into());
            }
            let preview = SftpSyncPreview {
                id,
                session,
                options,
                entries,
            };
            if preview.entries.len() > 10000 {
                return Err("两侧合计差异超过 10000 项，请缩小范围".into());
            }
            let bytes = serde_json::to_vec(&preview).map_err(|_| "比较结果编码失败")?;
            if bytes.len() > 4 * 1024 * 1024 {
                return Err("比较结果过大，请缩小范围".into());
            }
            let mut records = self
                .sync_plans
                .records
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            records.retain(|_, r| r.created.elapsed() < Duration::from_secs(300));
            if records.contains_key(&id) || records.len() >= 16 {
                return Err("比较标识已用或比较结果已满，请重新打开比较".into());
            }
            records.insert(
                id,
                Arc::new(Record {
                    preview,
                    metadata_fingerprint,
                    checked,
                    source,
                    epoch,
                    created: Instant::now(),
                    claimed: AtomicBool::new(false),
                }),
            );
            Ok(bytes)
        } else {
            let apply: SftpSyncApply =
                serde_json::from_slice(&request.document_json).map_err(|_| "同步选择无效")?;
            let record = self
                .sync_plans
                .records
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(&id)
                .cloned()
                .ok_or("比较已失效，请重新比较")?;
            let current = sessions.get_sftp(session).map_err(|_| "SFTP 会话已断开")?;
            if record.preview.session != session
                || !Arc::ptr_eq(&current, &record.source)
                || !current.running()
                || current.generation() != record.epoch
                || record.created.elapsed() > Duration::from_secs(300)
            {
                return Err("连接或比较已过期，请重新比较".into());
            }
            if record.claimed.swap(true, Ordering::AcqRel) {
                return Err("此比较已提交，请查看原同步任务；不要重复提交".into());
            }
            let keys = apply.selected.iter().collect::<BTreeSet<_>>();
            if keys.is_empty() || keys.len() != apply.selected.len() || keys.len() > 10000 {
                return Err("请选择有效且不重复的差异项目".into());
            }
            let options = &record.preview.options;
            let (a, b) =
                tokio::time::timeout(Duration::from_secs(45), inventory(&record.source, options))
                    .await
                    .map_err(|_| "重新核对超时，请重新比较")??;
            if metadata_fingerprint(&sftp_sync_entries(&a, &b))? != record.metadata_fingerprint
                || !current.running()
                || current.generation() != record.epoch
            {
                return Err("目录或连接已变化，未提交传输，请重新比较".into());
            }
            let mut files = Vec::new();
            let mut directories = BTreeSet::new();
            let mut total_bytes = 0u64;
            for key in keys {
                let entry = record
                    .preview
                    .entries
                    .iter()
                    .find(|e| &e.relative == key)
                    .filter(|e| e.eligible(options.direction, apply.backup_replace))
                    .ok_or("选择包含不可同步或冲突项目，请重新比较")?;
                let local = local_child(&options.local, key);
                let remote = remote_child(&options.remote, key);
                let metadata = if options.direction == SftpQueueDirection::Upload {
                    entry.local.as_ref()
                } else {
                    entry.remote.as_ref()
                }
                .ok_or("源项目不存在")?;
                if metadata.directory {
                    directories.insert(if options.direction == SftpQueueDirection::Upload {
                        remote
                    } else {
                        local
                    });
                } else {
                    total_bytes = total_bytes
                        .checked_add(metadata.size)
                        .ok_or("同步总大小溢出")?;
                    let modified_unix_seconds=if options.preserve_modified{Some(metadata.modified.and_then(|t|u32::try_from(t).ok()).ok_or("源未提供可保留的修改时间，请关闭保留时间后重新比较")?)}else{None};
                    files.push(SftpQueueDraft {
                        modified_unix_seconds,
                        direction: options.direction,
                        local_path: local,
                        remote_path: remote,
                        verified: true,
                        verification: Some(options.verification),
                        expected_source: record.checked.get(key.as_str()).map(|(a, b)| {
                            if options.direction == SftpQueueDirection::Upload {
                                *a
                            } else {
                                *b
                            }
                        }),
                        expected_destination: record.checked.get(key.as_str()).map(|(a, b)| {
                            if options.direction == SftpQueueDirection::Upload {
                                *b
                            } else {
                                *a
                            }
                        }),
                        publication_policy: if matches!(
                            entry.difference,
                            SftpSyncDifference::Changed | SftpSyncDifference::ContentChanged
                        ) {
                            Some(FilePublicationPolicy::BackupReplace)
                        } else {
                            None
                        },
                        ..Default::default()
                    });
                }
                let mut ancestor = key.rsplit_once('/').map(|(p, _)| p);
                while let Some(parent) = ancestor {
                    if record.preview.entries.iter().any(|e| {
                        e.relative == parent && e.difference == SftpSyncDifference::TypeConflict
                    }) {
                        return Err("父路径存在文件与目录冲突，未提交".into());
                    }
                    directories.insert(if options.direction == SftpQueueDirection::Upload {
                        remote_child(&options.remote, parent)
                    } else {
                        local_child(&options.local, parent)
                    });
                    ancestor = parent.rsplit_once('/').map(|(p, _)| p);
                }
            }
            let mut directories = directories.into_iter().collect::<Vec<_>>();
            directories.sort_by_key(|p| p.len());
            let snapshot = self.start_sync_tree(
                record.source.clone(),
                session,
                id,
                SftpTreeStart {
                    direction: options.direction,
                    paths: apply.selected,
                    destination: if options.direction == SftpQueueDirection::Upload {
                        options.remote.clone()
                    } else {
                        options.local.clone()
                    },
                    verification: options.verification,
                },
                files,
                directories,
                total_bytes,
                record.epoch,
            )?;
            serde_json::to_vec(&snapshot).map_err(|_| "同步状态编码失败".into())
        }
    }
}
