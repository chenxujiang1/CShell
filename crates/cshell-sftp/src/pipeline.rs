use super::*;
use futures::{StreamExt, stream};

impl SftpClient {
    /// Read a known, bounded range in order, allowing short protocol reads.
    pub(super) async fn read_range(
        &self,
        handle: &str,
        offset: u64,
        length: usize,
    ) -> Result<Vec<u8>, SftpError> {
        if length == 0 || length > SFTP_TRANSFER_BATCH_BYTES {
            return Err(SftpError::InvalidServerLimits);
        }
        offset
            .checked_add(length as u64)
            .ok_or(SftpError::OffsetOverflow)?;
        let chunk = self.read_chunk_bytes()? as usize;
        let session = self.session.clone();
        let handle = handle.to_owned();
        let requests = stream::iter((0..length).step_by(chunk)).map(move |start| {
            let session = session.clone();
            let handle = handle.clone();
            async move {
                let count = chunk.min(length - start);
                let mut bytes = Vec::with_capacity(count);
                while bytes.len() < count {
                    let data = session
                        .read(
                            handle.to_owned(),
                            offset + (start + bytes.len()) as u64,
                            (count - bytes.len()) as u32,
                        )
                        .await?
                        .data;
                    if data.is_empty() || data.len() > count - bytes.len() {
                        return Err(SftpError::EmptyRead);
                    }
                    bytes.extend_from_slice(&data);
                }
                Ok(bytes)
            }
        });
        let mut results = requests.buffered(SFTP_PIPELINE_REQUESTS);
        // Drain all issued requests before reporting an error or closing a handle.
        let mut bytes = Vec::with_capacity(length);
        let mut error = None;
        while let Some(result) = results.next().await {
            match result {
                Ok(data) if error.is_none() => bytes.extend(data),
                Ok(_) => {}
                Err(cause) => {
                    error.get_or_insert(cause);
                }
            }
        }
        match error {
            Some(error) => Err(error),
            None => Ok(bytes),
        }
    }

    pub(super) async fn write_range(
        &self,
        handle: &str,
        offset: u64,
        bytes: &[u8],
    ) -> Result<(), SftpError> {
        if bytes.is_empty() || bytes.len() > SFTP_TRANSFER_BATCH_BYTES {
            return Err(SftpError::InvalidServerLimits);
        }
        offset
            .checked_add(bytes.len() as u64)
            .ok_or(SftpError::OffsetOverflow)?;
        let chunk = self.write_chunk_bytes(handle)?;
        let session = self.session.clone();
        let handle = handle.to_owned();
        let bytes = bytes.to_vec();
        let requests = stream::iter((0..bytes.len()).step_by(chunk)).map(move |start| {
            let session = session.clone();
            let handle = handle.clone();
            let data = bytes[start..bytes.len().min(start + chunk)].to_vec();
            async move { session.write(handle, offset + start as u64, data).await }
        });
        let mut results = requests.buffer_unordered(SFTP_PIPELINE_REQUESTS);
        let mut error = None;
        while let Some(result) = results.next().await {
            if let Err(cause) = result {
                error.get_or_insert(SftpError::from(cause));
            }
        }
        match error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}
