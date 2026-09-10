use crate::{
    Envelope, Handshake, HandshakeAck, IpcError, PROTOCOL_MAJOR, PROTOCOL_MINOR, envelope,
    read_envelope, write_envelope,
};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncWrite};

const INSTANCE_ID_LENGTH: usize = 16;
const TOKEN_LENGTH: usize = 32;

#[derive(Clone)]
pub struct HandshakePolicy {
    expected_token: [u8; TOKEN_LENGTH],
    expected_daemon_instance_id: Option<[u8; INSTANCE_ID_LENGTH]>,
    supported_feature_bits: u64,
}

impl std::fmt::Debug for HandshakePolicy {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HandshakePolicy")
            .field("expected_token", &"[REDACTED]")
            .field(
                "expected_daemon_instance_id",
                &self.expected_daemon_instance_id,
            )
            .field("supported_feature_bits", &self.supported_feature_bits)
            .finish()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NegotiatedHandshake {
    pub protocol_minor: u32,
    pub feature_bits: u64,
    pub daemon_instance_id: [u8; INSTANCE_ID_LENGTH],
}

impl NegotiatedHandshake {
    #[must_use]
    pub fn acknowledgement(&self) -> HandshakeAck {
        HandshakeAck {
            protocol_major: PROTOCOL_MAJOR,
            protocol_minor: self.protocol_minor,
            negotiated_feature_bits: self.feature_bits,
            daemon_instance_id: self.daemon_instance_id.to_vec(),
        }
    }
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum HandshakeError {
    #[error("invalid CShell IPC magic")]
    InvalidMagic,
    #[error("incompatible IPC protocol major")]
    IncompatibleMajor,
    #[error("invalid daemon instance identifier")]
    InvalidInstanceId,
    #[error("IPC endpoint authentication failed")]
    AuthenticationFailed,
}

#[derive(Debug, Error)]
pub enum HandshakeProtocolError {
    #[error(transparent)]
    Ipc(#[from] IpcError),
    #[error(transparent)]
    Rejected(#[from] HandshakeError),
    #[error("expected an IPC handshake message")]
    ExpectedHandshake,
    #[error("expected an IPC handshake acknowledgement")]
    ExpectedAcknowledgement,
    #[error("handshake acknowledgement does not match the request")]
    InvalidAcknowledgement,
}

impl HandshakePolicy {
    #[must_use]
    pub const fn new(expected_token: [u8; TOKEN_LENGTH], supported_feature_bits: u64) -> Self {
        Self {
            expected_token,
            expected_daemon_instance_id: None,
            supported_feature_bits,
        }
    }

    #[must_use]
    pub const fn with_instance_id(
        expected_token: [u8; TOKEN_LENGTH],
        expected_daemon_instance_id: [u8; INSTANCE_ID_LENGTH],
        supported_feature_bits: u64,
    ) -> Self {
        Self {
            expected_token,
            expected_daemon_instance_id: Some(expected_daemon_instance_id),
            supported_feature_bits,
        }
    }

    pub fn validate(&self, handshake: &Handshake) -> Result<NegotiatedHandshake, HandshakeError> {
        if handshake.magic != Handshake::MAGIC {
            return Err(HandshakeError::InvalidMagic);
        }
        if handshake.protocol_major != PROTOCOL_MAJOR {
            return Err(HandshakeError::IncompatibleMajor);
        }
        let daemon_instance_id: [u8; INSTANCE_ID_LENGTH] = handshake
            .daemon_instance_id
            .as_slice()
            .try_into()
            .map_err(|_| HandshakeError::InvalidInstanceId)?;
        if self
            .expected_daemon_instance_id
            .is_some_and(|expected| expected != daemon_instance_id)
        {
            return Err(HandshakeError::AuthenticationFailed);
        }
        if !constant_time_token_matches(&handshake.instance_token, &self.expected_token) {
            return Err(HandshakeError::AuthenticationFailed);
        }
        Ok(NegotiatedHandshake {
            protocol_minor: handshake.protocol_minor.min(PROTOCOL_MINOR),
            feature_bits: handshake.feature_bits & self.supported_feature_bits,
            daemon_instance_id,
        })
    }
}

fn constant_time_token_matches(candidate: &[u8], expected: &[u8; TOKEN_LENGTH]) -> bool {
    let mut difference = candidate.len() ^ expected.len();
    for (index, expected_byte) in expected.iter().enumerate() {
        let candidate_byte = candidate.get(index).copied().unwrap_or(0);
        difference |= usize::from(candidate_byte ^ expected_byte);
    }
    difference == 0
}

pub async fn server_handshake<S>(
    stream: &mut S,
    policy: &HandshakePolicy,
) -> Result<NegotiatedHandshake, HandshakeProtocolError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let envelope = read_envelope(stream).await?;
    let Some(envelope::Payload::Handshake(handshake)) = envelope.payload else {
        return Err(HandshakeProtocolError::ExpectedHandshake);
    };
    let negotiated = policy.validate(&handshake)?;
    let acknowledgement = Envelope {
        request_id: envelope.request_id,
        deadline_unix_ms: 0,
        payload: Some(envelope::Payload::HandshakeAck(
            negotiated.acknowledgement(),
        )),
    };
    write_envelope(stream, &acknowledgement).await?;
    Ok(negotiated)
}

pub async fn client_handshake<S>(
    stream: &mut S,
    request_id: u64,
    handshake: Handshake,
) -> Result<NegotiatedHandshake, HandshakeProtocolError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let expected_instance_id = handshake.daemon_instance_id.clone();
    let requested_minor = handshake.protocol_minor;
    let requested_features = handshake.feature_bits;
    write_envelope(
        stream,
        &Envelope {
            request_id,
            deadline_unix_ms: 0,
            payload: Some(envelope::Payload::Handshake(handshake)),
        },
    )
    .await?;

    let response = read_envelope(stream).await?;
    let Some(envelope::Payload::HandshakeAck(acknowledgement)) = response.payload else {
        return Err(HandshakeProtocolError::ExpectedAcknowledgement);
    };
    if response.request_id != request_id
        || acknowledgement.protocol_major != PROTOCOL_MAJOR
        || acknowledgement.protocol_minor > requested_minor.min(PROTOCOL_MINOR)
        || acknowledgement.negotiated_feature_bits & !requested_features != 0
        || acknowledgement.daemon_instance_id != expected_instance_id
    {
        return Err(HandshakeProtocolError::InvalidAcknowledgement);
    }
    let daemon_instance_id = acknowledgement
        .daemon_instance_id
        .as_slice()
        .try_into()
        .map_err(|_| HandshakeProtocolError::InvalidAcknowledgement)?;
    Ok(NegotiatedHandshake {
        protocol_minor: acknowledgement.protocol_minor,
        feature_bits: acknowledgement.negotiated_feature_bits,
        daemon_instance_id,
    })
}

#[cfg(test)]
mod tests {
    use super::{HandshakeError, HandshakePolicy};
    use crate::{Handshake, PROTOCOL_MINOR, features};

    #[test]
    fn valid_handshake_negotiates_minor_and_features() {
        let token = [7_u8; 32];
        let policy = HandshakePolicy::new(token, features::FULL_FRAME_RECOVERY);
        let mut handshake = Handshake::new(vec![1; 16], token.to_vec());
        handshake.protocol_minor = PROTOCOL_MINOR + 10;
        handshake.feature_bits = features::FULL_FRAME_RECOVERY | features::CANCELLATION;
        let negotiated = policy
            .validate(&handshake)
            .unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(negotiated.protocol_minor, PROTOCOL_MINOR);
        assert_eq!(negotiated.feature_bits, features::FULL_FRAME_RECOVERY);
        assert_eq!(negotiated.acknowledgement().daemon_instance_id, vec![1; 16]);
    }

    #[test]
    fn wrong_or_short_token_has_one_external_error() {
        let policy = HandshakePolicy::new([7; 32], 0);
        for token in [vec![8; 32], vec![7; 4]] {
            let handshake = Handshake::new(vec![1; 16], token);
            assert_eq!(
                policy.validate(&handshake),
                Err(HandshakeError::AuthenticationFailed)
            );
        }
    }

    #[test]
    fn configured_daemon_instance_id_rejects_a_stale_discovery_record() {
        let token = [7; 32];
        let policy = HandshakePolicy::with_instance_id(token, [4; 16], 0);
        let stale = Handshake::new(vec![3; 16], token.to_vec());
        assert_eq!(
            policy.validate(&stale),
            Err(HandshakeError::AuthenticationFailed)
        );

        let current = Handshake::new(vec![4; 16], token.to_vec());
        assert!(policy.validate(&current).is_ok());
    }
}
