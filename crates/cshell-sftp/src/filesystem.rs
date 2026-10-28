use super::*;
#[derive(Clone, Eq, PartialEq)]
pub struct RemoteEntryStamp {
    pub metadata: RemoteMetadata,
    pub content: Option<StagedUploadContent>,
    pub link_target: Option<String>,
}
impl std::fmt::Debug for RemoteEntryStamp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteEntryStamp")
            .field("metadata", &self.metadata)
            .finish_non_exhaustive()
    }
}
impl SftpClient {
    pub async fn resolve_path(&self, path: &str) -> Result<String, SftpError> {
        let names = self.session.realpath(path).await?;
        if names.files.len() != 1 {
            return Err(SftpError::InvalidRemotePath(String::new()));
        }
        let path = names.files[0].filename.clone();
        if !valid(&path, true) {
            return Err(SftpError::InvalidRemotePath(String::new()));
        }
        Ok(path)
    }
    pub async fn directory_path(&self, path: &str, follow: bool) -> Result<String, SftpError> {
        if !valid(path, true) {
            return Err(SftpError::InvalidRemotePath(String::new()));
        }
        let path = if follow {
            self.resolve_path(path).await?
        } else {
            path.to_owned()
        };
        let mut at = String::new();
        for component in path.split('/').skip(1).filter(|p| !p.is_empty()) {
            at.push('/');
            at.push_str(component);
            if self
                .metadata(at.clone(), SymlinkPolicy::NoFollow)
                .await?
                .file_type
                != RemoteFileType::Directory
            {
                return Err(SftpError::InvalidRemotePath(String::new()));
            }
        }
        if self
            .metadata(path.clone(), SymlinkPolicy::NoFollow)
            .await?
            .file_type
            != RemoteFileType::Directory
        {
            return Err(SftpError::InvalidRemotePath(String::new()));
        }
        Ok(path)
    }
    pub async fn inspect_entry(
        &self,
        path: &str,
        maximum: u64,
    ) -> Result<Option<RemoteEntryStamp>, SftpError> {
        if !valid(path, true) {
            return Err(SftpError::InvalidRemotePath(String::new()));
        }
        let metadata = match self
            .metadata(path.to_owned(), SymlinkPolicy::NoFollow)
            .await
        {
            Ok(m) => m,
            Err(SftpError::Protocol(e)) if is_status(&e, StatusCode::NoSuchFile) => {
                return Ok(None);
            }
            Err(e) => return Err(e),
        };
        let content = if metadata.file_type == RemoteFileType::File {
            Some(
                staged::digest_content(self, path, maximum)
                    .await
                    .map_err(|_| SftpError::ChannelUnavailable)?,
            )
        } else {
            None
        };
        let link_target = if metadata.file_type == RemoteFileType::Symlink {
            let names = self.session.readlink(path).await?;
            if names.files.len() != 1 || names.files[0].filename.len() > 4096 {
                return Err(SftpError::InvalidRemotePath(String::new()));
            }
            Some(names.files[0].filename.clone())
        } else {
            None
        };
        if self
            .metadata(path.to_owned(), SymlinkPolicy::NoFollow)
            .await?
            != metadata
        {
            return Err(SftpError::ChannelUnavailable);
        }
        Ok(Some(RemoteEntryStamp {
            metadata,
            content,
            link_target,
        }))
    }
    pub async fn create_directory(&self, path: &str, permissions: u32) -> Result<(), SftpError> {
        self.valid_mutation(path).await?;
        if permissions > 0o7777 {
            return Err(SftpError::InvalidRemotePath(String::new()));
        }
        self.session
            .mkdir(
                path,
                FileAttributes {
                    permissions: Some(permissions),
                    ..Default::default()
                },
            )
            .await?;
        Ok(())
    }
    pub async fn rename_entry(&self, path: &str, target: &str) -> Result<(), SftpError> {
        self.valid_mutation(path).await?;
        self.valid_mutation(target).await?;
        if path == target {
            return Err(SftpError::InvalidRemotePath(String::new()));
        }
        match self.session.lstat(target).await {
            Err(e) if is_status(&e, StatusCode::NoSuchFile) => {}
            Ok(_) => return Err(SftpError::DestinationExists(String::new())),
            Err(e) => return Err(e.into()),
        }
        self.session.rename(path, target).await?;
        Ok(())
    }
    pub async fn remove_entry(&self, path: &str, directory: bool) -> Result<(), SftpError> {
        self.valid_mutation(path).await?;
        if directory {
            self.session.rmdir(path).await?;
        } else {
            self.session.remove(path).await?;
        }
        Ok(())
    }
    pub async fn chmod_entry(&self, path: &str, permissions: u32) -> Result<(), SftpError> {
        self.valid_mutation(path).await?;
        if permissions > 0o7777
            || !matches!(
                self.metadata(path, SymlinkPolicy::NoFollow)
                    .await?
                    .file_type,
                RemoteFileType::File | RemoteFileType::Directory
            )
        {
            return Err(SftpError::InvalidRemotePath(String::new()));
        }
        self.session
            .setstat(
                path,
                FileAttributes {
                    permissions: Some(permissions),
                    ..Default::default()
                },
            )
            .await?;
        Ok(())
    }
    pub async fn symlink_entry(&self, path: &str, target: &str) -> Result<(), SftpError> {
        self.valid_mutation(path).await?;
        if !valid(target, true) {
            return Err(SftpError::InvalidRemotePath(String::new()));
        }
        self.session.symlink(path, target).await?;
        Ok(())
    }
    async fn valid_mutation(&self, path: &str) -> Result<(), SftpError> {
        if !valid(path, false) {
            return Err(SftpError::InvalidRemotePath(String::new()));
        }
        let parent = path
            .rsplit_once('/')
            .ok_or(SftpError::InvalidRemotePath(String::new()))?
            .0;
        self.directory_path(if parent.is_empty() { "/" } else { parent }, false)
            .await?;
        Ok(())
    }
}
fn valid(path: &str, root: bool) -> bool {
    root && path == "/"
        || path.len() <= 4000
            && path.starts_with('/')
            && !path.ends_with('/')
            && !path.contains('\\')
            && !path.chars().any(char::is_control)
            && path
                .split('/')
                .skip(1)
                .all(|p| !p.is_empty() && p != "." && p != "..")
}
