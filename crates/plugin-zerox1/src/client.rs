//! HTTP client for the zerox1-node REST API.

use crate::types::{
    HostedSendRequest, SendEnvelopeRequest, SendEnvelopeResponse,
};
use anyhow::{Context, Result};
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;

/// HTTP client wrapping the zerox1-node REST API.
///
/// Supports both local-node mode (`POST /envelopes/send`) and hosted-agent
/// mode (`POST /hosted/send` with Bearer token).
#[derive(Clone, Debug)]
pub struct Zerox1Client {
    /// Base HTTP URL, e.g. `"http://127.0.0.1:9090"`.
    pub api_base: String,
    /// Optional Bearer token — required only in hosted-agent mode.
    pub token: Option<String>,
    inner: reqwest::Client,
}

impl Zerox1Client {
    /// Build a new client.  Fails if the underlying HTTP client cannot be
    /// constructed (e.g. invalid TLS configuration).
    pub fn new(api_base: impl Into<String>, token: Option<String>) -> Result<Self> {
        let inner = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .context("Failed to build HTTP client")?;
        Ok(Self {
            api_base: api_base.into(),
            token,
            inner,
        })
    }

    /// `GET /hosted/ping` — lightweight reachability probe.
    /// Returns `true` when the node responds with HTTP 2xx.
    pub async fn ping(&self) -> bool {
        let url = format!("{}/hosted/ping", self.api_base);
        self.inner
            .get(&url)
            .timeout(std::time::Duration::from_secs(5))
            .send()
            .await
            .map(|r| r.status().is_success())
            .unwrap_or(false)
    }

    /// `POST /envelopes/send` — local-node mode.
    ///
    /// The `payload` bytes are base64-encoded before transmission.  An optional
    /// Bearer token is added when `self.token` is set (API secret mode).
    pub async fn send_envelope(
        &self,
        msg_type: &str,
        recipient: Option<&str>,
        conversation_id: &str,
        payload: &[u8],
    ) -> Result<SendEnvelopeResponse> {
        let url = format!("{}/envelopes/send", self.api_base);
        let mut builder = self.inner.post(&url);
        if let Some(tok) = &self.token {
            builder = builder.header("Authorization", format!("Bearer {tok}"));
        }
        let body = SendEnvelopeRequest {
            msg_type: msg_type.to_uppercase(),
            recipient: recipient.map(str::to_string),
            conversation_id: conversation_id.to_string(),
            payload_b64: BASE64.encode(payload),
        };
        let resp = builder
            .json(&body)
            .send()
            .await
            .context("POST /envelopes/send request failed")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("/envelopes/send {status}: {text}");
        }
        resp.json::<SendEnvelopeResponse>()
            .await
            .context("Failed to deserialize /envelopes/send response")
    }

    /// `POST /hosted/send` — hosted-agent mode.
    ///
    /// Uses the provided `token` as the Bearer credential.  The `payload`
    /// bytes are hex-encoded (as the hosted API expects).
    pub async fn hosted_send(
        &self,
        token: &str,
        msg_type: &str,
        recipient: Option<&str>,
        conversation_id: &str,
        payload: &[u8],
    ) -> Result<()> {
        let url = format!("{}/hosted/send", self.api_base);
        let body = HostedSendRequest {
            msg_type: msg_type.to_uppercase(),
            recipient: recipient.map(str::to_string),
            conversation_id: conversation_id.to_string(),
            payload_hex: hex::encode(payload),
        };
        let resp = self
            .inner
            .post(&url)
            .header("Authorization", format!("Bearer {token}"))
            .json(&body)
            .send()
            .await
            .context("POST /hosted/send request failed")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("/hosted/send {status}: {text}");
        }
        Ok(())
    }

    /// Derive the WebSocket base URL from `self.api_base`.
    ///
    /// `http://` → `ws://`, `https://` → `wss://`.
    pub fn ws_base(&self) -> String {
        self.api_base
            .replace("https://", "wss://")
            .replace("http://", "ws://")
    }
}
