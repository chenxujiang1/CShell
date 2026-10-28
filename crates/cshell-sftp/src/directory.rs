//! One open directory handle; retain unread entries from each NAME packet.
use super::*;
use std::collections::VecDeque;

pub struct DirectoryCursor {
    client: SftpClient,
    path: String,
    handle: Option<String>,
    pending: VecDeque<RemoteDirectoryEntry>,
    eof: bool,
}
impl std::fmt::Debug for DirectoryCursor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DirectoryCursor")
            .field("pending", &self.pending.len())
            .field("eof", &self.eof)
            .finish_non_exhaustive()
    }
}
impl SftpClient {
    pub async fn directory_cursor(self, path: String) -> Result<DirectoryCursor, SftpError> {
        let path = match self.directory_path(&path, false).await {
            Ok(path) => path,
            Err(error) => {
                let _ = self.close().await;
                return Err(error);
            }
        };
        match self.session.opendir(path.clone()).await {
            Ok(opened) => Ok(DirectoryCursor {
                client: self,
                path,
                handle: Some(opened.handle),
                pending: VecDeque::new(),
                eof: false,
            }),
            Err(error) => {
                let _ = self.close().await;
                Err(error.into())
            }
        }
    }
}
impl DirectoryCursor {
    pub async fn next_page(&mut self, limit: usize) -> Result<DirectoryPage, SftpError> {
        if !(1..=MAX_DIRECTORY_PAGE_ENTRIES).contains(&limit) {
            return Err(SftpError::InvalidDirectoryPageLimit);
        }
        // One entry of lookahead distinguishes exactly-full last pages from more data.
        let mut entries = Vec::with_capacity(limit);
        let mut empty_packets = 0;
        loop {
            while entries.len() < limit {
                match self.pending.pop_front() {
                    Some(entry) => entries.push(entry),
                    None => break,
                }
            }
            if !self.pending.is_empty() || self.eof {
                return Ok(DirectoryPage {
                    entries,
                    truncated: !self.pending.is_empty() || !self.eof,
                });
            }
            let handle = self
                .handle
                .as_ref()
                .ok_or(SftpError::ChannelUnavailable)?
                .clone();
            match self.client.session.readdir(handle).await {
                Ok(names) => {
                    for file in names.files {
                        if file.filename == "." || file.filename == ".." {
                            continue;
                        }
                        if file.filename.is_empty()
                            || file.filename.len() > 4096
                            || file.filename.chars().any(char::is_control)
                            || file.filename.contains(['/', '\\'])
                        {
                            return Err(SftpError::InvalidRemotePath(String::new()));
                        }
                        let path = join_remote_path(&self.path, &file.filename);
                        if path.len() > 4096 {
                            return Err(SftpError::InvalidRemotePath(String::new()));
                        }
                        self.pending.push_back(RemoteDirectoryEntry {
                            path,
                            name: file.filename,
                            metadata: map_metadata(&file.attrs),
                        });
                    }
                    if self.pending.is_empty() {
                        empty_packets += 1;
                        if empty_packets > 8 {
                            return Err(SftpError::EmptyRead);
                        }
                    } else {
                        empty_packets = 0;
                    }
                }
                Err(error) if is_status(&error, StatusCode::Eof) => {
                    self.eof = true;
                    if let Some(handle) = self.handle.take() {
                        self.client.session.close(handle).await?;
                    }
                }
                Err(error) => return Err(error.into()),
            }
        }
    }
    pub async fn close(&mut self) -> Result<(), SftpError> {
        let result = if let Some(handle) = self.handle.take() {
            self.client
                .session
                .close(handle)
                .await
                .map(|_| ())
                .map_err(SftpError::from)
        } else {
            Ok(())
        };
        self.pending.clear();
        self.eof = true;
        let channel = self.client.close().await;
        result.and(channel)
    }
}
impl Drop for DirectoryCursor {
    fn drop(&mut self) {
        let _ = self.client.session.close_session();
    }
}
