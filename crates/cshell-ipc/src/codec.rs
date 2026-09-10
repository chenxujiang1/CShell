use bytes::{Buf, BufMut, BytesMut};
use prost::Message;
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::Envelope;

const LENGTH_PREFIX_SIZE: usize = 4;
pub const DEFAULT_MAX_FRAME_SIZE: usize = 16 * 1024 * 1024;

#[derive(Debug, Error)]
pub enum IpcError {
    #[error("IPC frame is {actual} bytes; limit is {limit} bytes")]
    FrameTooLarge { actual: usize, limit: usize },
    #[error("invalid IPC protobuf: {0}")]
    Decode(#[from] prost::DecodeError),
    #[error("cannot encode IPC protobuf: {0}")]
    Encode(#[from] prost::EncodeError),
    #[error("IPC I/O failed: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Clone, Copy, Debug)]
pub struct FrameCodec {
    max_frame_size: usize,
}

impl Default for FrameCodec {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_FRAME_SIZE)
    }
}

impl FrameCodec {
    #[must_use]
    pub const fn new(max_frame_size: usize) -> Self {
        Self { max_frame_size }
    }

    pub fn encode(&self, message: &Envelope, output: &mut BytesMut) -> Result<(), IpcError> {
        let encoded_len = message.encoded_len();
        if encoded_len > self.max_frame_size || encoded_len > u32::MAX as usize {
            return Err(IpcError::FrameTooLarge {
                actual: encoded_len,
                limit: self.max_frame_size,
            });
        }
        output.reserve(LENGTH_PREFIX_SIZE + encoded_len);
        output.put_u32(encoded_len as u32);
        message.encode(output)?;
        Ok(())
    }

    pub fn decode(&self, input: &mut BytesMut) -> Result<Option<Envelope>, IpcError> {
        if input.len() < LENGTH_PREFIX_SIZE {
            return Ok(None);
        }
        let frame_len = u32::from_be_bytes([input[0], input[1], input[2], input[3]]) as usize;
        if frame_len > self.max_frame_size {
            return Err(IpcError::FrameTooLarge {
                actual: frame_len,
                limit: self.max_frame_size,
            });
        }
        if input.len() < LENGTH_PREFIX_SIZE + frame_len {
            return Ok(None);
        }
        input.advance(LENGTH_PREFIX_SIZE);
        let payload = input.split_to(frame_len);
        Ok(Some(Envelope::decode(payload.freeze())?))
    }
}

pub async fn write_envelope<W>(writer: &mut W, message: &Envelope) -> Result<(), IpcError>
where
    W: AsyncWrite + Unpin,
{
    let mut frame = BytesMut::new();
    FrameCodec::default().encode(message, &mut frame)?;
    writer.write_all(&frame).await?;
    writer.flush().await?;
    Ok(())
}

pub async fn read_envelope<R>(reader: &mut R) -> Result<Envelope, IpcError>
where
    R: AsyncRead + Unpin,
{
    let frame_len = reader.read_u32().await? as usize;
    if frame_len > DEFAULT_MAX_FRAME_SIZE {
        return Err(IpcError::FrameTooLarge {
            actual: frame_len,
            limit: DEFAULT_MAX_FRAME_SIZE,
        });
    }
    let mut payload = vec![0_u8; frame_len];
    reader.read_exact(&mut payload).await?;
    Ok(Envelope::decode(payload.as_slice())?)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::{FrameCodec, IpcError};
    use crate::{Envelope, Handshake, envelope};
    use bytes::{BufMut, BytesMut};

    fn envelope() -> Envelope {
        Envelope {
            request_id: 42,
            deadline_unix_ms: 1000,
            payload: Some(envelope::Payload::Handshake(Handshake::new(
                vec![1; 16],
                vec![2; 32],
            ))),
        }
    }

    #[test]
    fn fragmented_frame_waits_for_the_remainder() {
        let codec = FrameCodec::default();
        let mut complete = BytesMut::new();
        codec.encode(&envelope(), &mut complete).unwrap();
        let mut partial = complete.split_to(3);
        assert!(codec.decode(&mut partial).unwrap().is_none());
        partial.extend_from_slice(&complete);
        let decoded = codec.decode(&mut partial).unwrap().unwrap();
        assert_eq!(decoded.request_id, 42);
        assert!(partial.is_empty());
    }

    #[test]
    fn oversized_frame_is_rejected_before_allocation() {
        let codec = FrameCodec::new(8);
        let mut input = BytesMut::new();
        input.put_u32(9);
        assert!(matches!(
            codec.decode(&mut input),
            Err(IpcError::FrameTooLarge { .. })
        ));
    }

    #[test]
    fn handshake_requires_current_major_and_full_token() {
        assert!(Handshake::new(vec![1; 16], vec![2; 32]).is_compatible());
        assert!(!Handshake::new(vec![1; 16], vec![2; 4]).is_compatible());
    }
}
