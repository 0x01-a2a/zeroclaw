//! ZeroX1 mesh network channel.
//!
//! Connects ZeroClaw to a running `zerox1-node` process over its local REST +
//! WebSocket API.  Inbound envelopes arrive on `GET /ws/inbox` (local mode)
//! or `GET /ws/hosted/inbox` (hosted-agent mode) and are translated into
//! [`ChannelMessage`]s.  Outbound replies are sent as `FEEDBACK` envelopes
//! via `POST /envelopes/send` or `POST /hosted/send`.
//!
//! # Configuration
//!
//! ```toml
//! [channels_config.zerox1]
//! node_api_url = "http://127.0.0.1:9090"   # local node (default)
//! # token = "hex64"                         # set only for hosted-agent mode
//! # min_fee_usdc   = 0.01
//! # min_reputation = 50
//! # auto_accept    = false
//! # capabilities   = ["summarization", "qa"]
//! ```

use crate::channels::traits::{Channel, ChannelMessage, SendMessage};
use anyhow::{Context, Result};
use async_trait::async_trait;
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use futures_util::StreamExt;
use plugin_zerox1::{InboundEnvelope, Zerox1Client};
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::http;
use uuid::Uuid;

/// ZeroClaw channel backed by a `zerox1-node` process.
pub struct Zerox1Channel {
    client: Zerox1Client,
    /// Hosted-agent token; `None` in local-node mode.
    token: Option<String>,
    /// Bearer token for authenticating to the local node API (`/ws/inbox`, `/envelopes/send`).
    /// Used only in local mode (when `token` is `None`).
    api_secret: Option<String>,
    /// Named gossipsub topics to subscribe to in addition to the personal inbox.
    topics: Vec<String>,
}

impl Zerox1Channel {
    /// Create a new channel pointing at `node_api_url`.
    ///
    /// Supply `token` only when running in hosted-agent mode.
    /// Supply `api_secret` for the local node's `--api-secret` bearer token.
    /// Supply `topics` to subscribe to named gossipsub topics via `/ws/topics?topic=<slug>`.
    pub fn new(
        node_api_url: impl Into<String>,
        token: Option<String>,
        api_secret: Option<String>,
        topics: Vec<String>,
    ) -> Result<Self> {
        let url = node_api_url.into();
        let client = Zerox1Client::new(url, token.clone())?;
        Ok(Self { client, token, api_secret, topics })
    }
}

#[async_trait]
impl Channel for Zerox1Channel {
    fn name(&self) -> &str {
        "zerox1"
    }

    /// Ping the node.  Uses `GET /hosted/ping` which is always available.
    async fn health_check(&self) -> bool {
        self.client.ping().await
    }

    /// Send `message.content` as a `FEEDBACK` envelope to `message.recipient`.
    ///
    /// `message.thread_ts` is used as the `conversation_id` when present;
    /// otherwise a fresh UUID v4 is generated.
    async fn send(&self, message: &SendMessage) -> Result<()> {
        let conv_id = message
            .thread_ts
            .clone()
            .unwrap_or_else(|| Uuid::new_v4().simple().to_string());

        let payload = message.content.as_bytes();
        let recipient = Some(message.recipient.as_str());

        if let Some(ref tok) = self.token {
            self.client
                .hosted_send(tok, "FEEDBACK", recipient, &conv_id, payload)
                .await
                .context("Zerox1Channel hosted_send failed")?;
        } else {
            self.client
                .send_envelope("FEEDBACK", recipient, &conv_id, payload)
                .await
                .context("Zerox1Channel send_envelope failed")?;
        }

        Ok(())
    }

    /// Subscribe to the zerox1-node inbox WebSocket and forward every inbound
    /// envelope as a [`ChannelMessage`] on `tx`.
    ///
    /// Chooses the correct endpoint:
    /// - Local mode:  `ws://{host}/ws/inbox`
    /// - Hosted mode: `ws://{host}/ws/hosted/inbox` with `Authorization: Bearer <token>`
    ///
    /// If `topics` are configured, also spawns one WS subscriber task per topic
    /// connecting to `ws://{host}/ws/topics?topic=<slug>`.  Topic messages arrive
    /// on the same `tx` channel as inbox messages.
    ///
    /// Reconnects with exponential back-off on connection drops (handled by the
    /// channel subsystem's outer loop).
    async fn listen(&self, tx: mpsc::Sender<ChannelMessage>) -> Result<()> {
        let ws_base = self.client.ws_base();

        // ── Spawn a subscriber task per configured topic ─────────────────────
        let mut topic_tasks: JoinSet<()> = JoinSet::new();
        for topic in &self.topics {
            let ws_base_t = ws_base.clone();
            let secret_t = self.api_secret.clone();
            let topic_t = topic.clone();
            let tx_t = tx.clone();
            topic_tasks.spawn(async move {
                subscribe_topic(&ws_base_t, secret_t.as_deref(), &topic_t, tx_t).await;
            });
        }

        // ── Connect to the personal inbox ────────────────────────────────────
        // C-002: hosted mode uses Authorization: Bearer header instead of query param
        // to avoid token leakage in server access logs.
        let (ws_stream, _) = if let Some(ref tok) = self.token {
            // Hosted mode: connect to /ws/hosted/inbox with the hosted-agent token.
            let url_str = format!("{ws_base}/ws/hosted/inbox");
            let req = http::Request::builder()
                .uri(&url_str)
                .header("Authorization", format!("Bearer {tok}"))
                .body(())
                .context("failed to build hosted WS request")?;
            connect_async(req)
                .await
                .context("Failed to connect to zerox1-node hosted WebSocket")?
        } else if let Some(ref secret) = self.api_secret {
            // Local mode with API secret: pass token as query param (node accepts ?token=).
            let url_str = format!("{ws_base}/ws/inbox?token={secret}");
            connect_async(url_str.as_str())
                .await
                .context("Failed to connect to zerox1-node local WebSocket (auth)")?
        } else {
            // Local mode, no auth (node running without --api-secret).
            let url_str = format!("{ws_base}/ws/inbox");
            connect_async(url_str.as_str())
                .await
                .context("Failed to connect to zerox1-node WebSocket")?
        };

        let (_, mut read) = ws_stream.split();

        while let Some(msg_result) = read.next().await {
            match msg_result {
                Ok(tungstenite_msg) => {
                    // tungstenite 0.28: Text is Utf8Bytes, Binary is Bytes.
                    let text: String = match tungstenite_msg {
                        tokio_tungstenite::tungstenite::Message::Text(t) => t.to_string(),
                        tokio_tungstenite::tungstenite::Message::Binary(b) => {
                            match String::from_utf8(b.to_vec()) {
                                Ok(s) => s,
                                Err(_) => continue,
                            }
                        }
                        tokio_tungstenite::tungstenite::Message::Close(_) => break,
                        _ => continue,
                    };

                    match serde_json::from_str::<InboundEnvelope>(&text) {
                        Ok(env) => {
                            if let Some(msg) = envelope_to_channel_message(env) {
                                if tx.send(msg).await.is_err() {
                                    // Receiver dropped — host is shutting down.
                                    break;
                                }
                            }
                        }
                        Err(e) => {
                            tracing::debug!("zerox1: failed to parse envelope: {e}");
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!("zerox1 WS error: {e}");
                    break;
                }
            }
        }

        // Inbox closed — abort topic tasks so they don't linger.
        topic_tasks.abort_all();

        Ok(())
    }
}

/// Connect to `/ws/topics?topic=<slug>` and forward arriving envelopes as
/// [`ChannelMessage`]s on `tx`.  Runs until the connection closes or the
/// receiver is dropped; designed to be spawned as a background task.
async fn subscribe_topic(
    ws_base: &str,
    api_secret: Option<&str>,
    topic: &str,
    tx: mpsc::Sender<ChannelMessage>,
) {
    let url_str = if let Some(secret) = api_secret {
        format!("{ws_base}/ws/topics?topic={topic}&token={secret}")
    } else {
        format!("{ws_base}/ws/topics?topic={topic}")
    };

    let ws_stream = match connect_async(url_str.as_str()).await {
        Ok((stream, _)) => stream,
        Err(e) => {
            tracing::warn!("zerox1 topic '{topic}' WS connect failed: {e}");
            return;
        }
    };

    tracing::info!("zerox1: subscribed to topic '{topic}'");
    let (_, mut read) = ws_stream.split();

    while let Some(msg_result) = read.next().await {
        match msg_result {
            Ok(tungstenite_msg) => {
                let text: String = match tungstenite_msg {
                    tokio_tungstenite::tungstenite::Message::Text(t) => t.to_string(),
                    tokio_tungstenite::tungstenite::Message::Binary(b) => {
                        match String::from_utf8(b.to_vec()) {
                            Ok(s) => s,
                            Err(_) => continue,
                        }
                    }
                    tokio_tungstenite::tungstenite::Message::Close(_) => break,
                    _ => continue,
                };

                match serde_json::from_str::<InboundEnvelope>(&text) {
                    Ok(env) => {
                        if let Some(msg) = envelope_to_channel_message(env) {
                            if tx.send(msg).await.is_err() {
                                break;
                            }
                        }
                    }
                    Err(e) => {
                        tracing::debug!("zerox1 topic '{topic}': failed to parse envelope: {e}");
                    }
                }
            }
            Err(e) => {
                tracing::warn!("zerox1 topic '{topic}' WS error: {e}");
                break;
            }
        }
    }

    tracing::debug!("zerox1: topic '{topic}' WS closed");
}

/// Convert a raw [`InboundEnvelope`] into a [`ChannelMessage`] for the agent loop.
///
/// The `content` field is a compact JSON summary so the LLM sees the full
/// protocol context (msg_type, sender, conversation_id, decoded payload).
/// `thread_ts` carries the `conversation_id` so replies stay in the same thread.
fn envelope_to_channel_message(env: InboundEnvelope) -> Option<ChannelMessage> {
    // Decode payload; fall back to raw base64 if not valid UTF-8.
    let payload_text = BASE64
        .decode(&env.payload_b64)
        .ok()
        .and_then(|b| String::from_utf8(b).ok())
        .unwrap_or_else(|| env.payload_b64.clone());

    let content = serde_json::json!({
        "msg_type":        env.msg_type,
        "sender":          env.sender,
        "conversation_id": env.conversation_id,
        "payload":         payload_text,
    })
    .to_string();

    Some(ChannelMessage {
        id: format!("{}:{}", env.sender, env.nonce),
        sender: env.sender.clone(),
        reply_target: env.sender,
        content,
        channel: "zerox1".to_string(),
        timestamp: env.slot,
        thread_ts: Some(env.conversation_id),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_envelope(msg_type: &str, payload: &str) -> InboundEnvelope {
        InboundEnvelope {
            msg_type: msg_type.to_string(),
            sender: "aabbcc".repeat(5) + "aa",
            recipient: None,
            conversation_id: "deadbeef".repeat(4),
            slot: 100,
            nonce: 1,
            payload_b64: BASE64.encode(payload.as_bytes()),
        }
    }

    #[test]
    fn envelope_to_channel_message_sets_fields() {
        let env = make_envelope("PROPOSE", "Do the thing");
        let msg = envelope_to_channel_message(env.clone()).unwrap();

        assert_eq!(msg.sender, env.sender);
        assert_eq!(msg.channel, "zerox1");
        assert!(msg.content.contains("PROPOSE"));
        assert!(msg.content.contains("Do the thing"));
        assert_eq!(msg.thread_ts.as_deref(), Some(env.conversation_id.as_str()));
    }

    #[test]
    fn envelope_to_channel_message_handles_invalid_utf8_payload() {
        let mut env = make_envelope("FEEDBACK", "");
        // Replace with non-UTF8-representable raw base64
        env.payload_b64 = BASE64.encode(&[0xFF, 0xFE, 0xFD]);
        let msg = envelope_to_channel_message(env).unwrap();
        // Should fall back to raw base64 string, not panic.
        assert!(!msg.content.is_empty());
    }
}
