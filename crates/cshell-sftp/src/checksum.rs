//! Optional draft check-file-handle SHA-256. Unsupported/error replies fall back to a read.
use super::*;
pub(super) fn advertised(extensions: &std::collections::HashMap<String, String>) -> bool {
    extensions.contains_key("check-file-handle")
        || extensions
            .get("supported2")
            .is_some_and(|value| supported2(value.as_bytes()).unwrap_or(false))
}
fn supported2(data: &[u8]) -> Option<bool> {
    if data.len() > 32768 {
        return None;
    }
    let mut data = data.get(24..)?; // five u32 masks/limits and two u16 vectors
    fn number(data: &mut &[u8]) -> Option<usize> {
        let value = u32::from_be_bytes(data.get(..4)?.try_into().ok()?) as usize;
        *data = data.get(4..)?;
        Some(value)
    }
    fn string<'a>(data: &mut &'a [u8]) -> Option<&'a [u8]> {
        let length = number(data)?;
        let value = data.get(..length)?;
        *data = data.get(length..)?;
        Some(value)
    }
    let attributes = number(&mut data)?;
    if attributes > 256 {
        return None;
    }
    for _ in 0..attributes {
        string(&mut data)?;
    }
    let count = number(&mut data)?;
    if count > 256 {
        return None;
    }
    let mut found = false;
    for _ in 0..count {
        found |= string(&mut data)? == b"check-file";
    }
    data.is_empty().then_some(found)
}
impl staged::StagedUpload {
    pub fn supports_server_checksum(&self) -> bool {
        self.client.checksum_handle
    }
    pub async fn server_checksum(&self) -> Option<[u8; 32]> {
        if !self.client.checksum_handle {
            return None;
        }
        let handle = self.handle.as_ref()?;
        let mut data = Vec::new();
        for value in [handle.as_bytes(), b"sha256".as_slice()] {
            data.extend_from_slice(&(value.len() as u32).to_be_bytes());
            data.extend_from_slice(value);
        }
        data.extend_from_slice(&0u64.to_be_bytes()); // offset
        data.extend_from_slice(&0u64.to_be_bytes()); // through EOF
        data.extend_from_slice(&0u32.to_be_bytes()); // one digest
        let protocol::Packet::ExtendedReply(reply) = self
            .client
            .session
            .extended("check-file-handle", data)
            .await
            .ok()?
        else {
            return None;
        };
        parse_checksum(&reply.data)
    }
}
use russh_sftp::protocol;
fn parse_checksum(data: &[u8]) -> Option<[u8; 32]> {
    let length = u32::from_be_bytes(data.get(..4)?.try_into().ok()?) as usize;
    if length != 6 || data.get(4..10)? != b"sha256" {
        return None;
    }
    // The draft reply contains the algorithm string followed by raw digest bytes.
    data.get(10..)?.try_into().ok()
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn checksum_reply_requires_exact_algorithm_and_digest() {
        let mut reply = b"\0\0\0\x06sha256".to_vec();
        reply.extend_from_slice(&[7; 32]);
        assert_eq!(parse_checksum(&reply), Some([7; 32]));
        reply.push(0);
        assert_eq!(parse_checksum(&reply), None);
        assert_eq!(parse_checksum(b"\xff\xff\xff\xff"), None);
    }
    #[test]
    fn supported2_checks_extension_names_and_rejects_truncated_counts() {
        let mut data = vec![0; 24];
        data.extend_from_slice(&0u32.to_be_bytes());
        data.extend_from_slice(&1u32.to_be_bytes());
        data.extend_from_slice(&10u32.to_be_bytes());
        data.extend_from_slice(b"check-file");
        assert_eq!(supported2(&data), Some(true));
        data.pop();
        assert_eq!(supported2(&data), None);
        assert_eq!(supported2(&[0; 24]), None);
    }
}
