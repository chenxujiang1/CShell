use prost::Message;
/// Only emitted for an explicitly opted-in creation request, before its final response.
#[derive(Clone, PartialEq, Message)]
pub struct ConnectionDiagnosticUpdate {
    #[prost(bytes = "vec", tag = "1")]
    pub document_json: Vec<u8>,
}
