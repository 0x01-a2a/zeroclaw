//! Wire types for the zerox1-node REST and WebSocket API.

use serde::{Deserialize, Serialize};

/// Inbound envelope received from the `/ws/inbox` WebSocket.
///
/// Only required fields are non-optional here; the node may emit additional
/// decoded sub-fields (e.g. `feedback`, `notarize_bid`) which are ignored.
#[derive(Debug, Clone, Deserialize)]
pub struct InboundEnvelope {
    pub msg_type: String,
    /// Hex-encoded 32-byte sender agent_id.
    pub sender: String,
    /// Hex-encoded 32-byte recipient agent_id (may be the local node).
    #[serde(default)]
    pub recipient: Option<String>,
    /// Hex-encoded 16-byte conversation_id.
    pub conversation_id: String,
    /// Beacon slot the envelope was validated in.
    pub slot: u64,
    /// Per-sender nonce (replay protection).
    pub nonce: u64,
    /// Base64-encoded payload bytes.
    pub payload_b64: String,
}

/// Request body for `POST /envelopes/send` (local node, optional Bearer auth).
#[derive(Debug, Serialize)]
pub struct SendEnvelopeRequest {
    pub msg_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recipient: Option<String>,
    pub conversation_id: String,
    pub payload_b64: String,
}

/// Response from `POST /envelopes/send`.
#[derive(Debug, Deserialize)]
pub struct SendEnvelopeResponse {
    pub nonce: u64,
    pub payload_hash: String,
}

/// Request body for `POST /hosted/send` (hosted-agent mode, uses hex payload).
#[derive(Debug, Serialize)]
pub struct HostedSendRequest {
    pub msg_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recipient: Option<String>,
    pub conversation_id: String,
    pub payload_hex: String,
}
