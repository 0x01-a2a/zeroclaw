//! ElevenLabs text-to-speech tool.
//!
//! Converts text to speech using the ElevenLabs API and returns a base64-encoded
//! MP3 audio clip. The tool is enabled via the `tool-elevenlabs` feature flag
//! and configured under `[elevenlabs]` in the agent config.
//!
//! ## Config
//!
//! ```toml
//! [elevenlabs]
//! api_key = "YOUR_KEY"           # or env ELEVENLABS_API_KEY
//! default_voice_id = "pNInz..."  # ElevenLabs voice ID
//! model_id = "eleven_multilingual_v2"  # optional, defaults shown
//! ```
//!
//! ## Tool parameters
//!
//! | Field | Required | Description |
//! |-------|----------|-------------|
//! | `text` | yes | Text to synthesize (max 5 000 chars) |
//! | `voice_id` | no | Override the configured default voice |
//! | `stability` | no | 0.0–1.0, default 0.5 |
//! | `similarity_boost` | no | 0.0–1.0, default 0.75 |

use crate::tools::traits::{Tool, ToolResult};
use anyhow::{bail, Result};
use async_trait::async_trait;
use base64::engine::general_purpose::STANDARD as BASE64_STD;
use base64::Engine as _;
use serde_json::{json, Value};

const ELEVENLABS_TTS_URL: &str = "https://api.elevenlabs.io/v1/text-to-speech";
const MAX_TEXT_LEN: usize = 5_000;

pub struct ElevenLabsTtsTool {
    api_key: String,
    default_voice_id: String,
    model_id: String,
}

impl ElevenLabsTtsTool {
    pub fn new(api_key: String, default_voice_id: String, model_id: String) -> Self {
        Self {
            api_key,
            default_voice_id,
            model_id,
        }
    }
}

#[async_trait]
impl Tool for ElevenLabsTtsTool {
    fn name(&self) -> &str {
        "elevenlabs_tts"
    }

    fn description(&self) -> &str {
        "Convert text to speech using ElevenLabs. Returns base64-encoded MP3 audio. \
         Use this to produce voice output for messages, announcements, or responses. \
         The output can be played back by any audio system that accepts base64 MP3."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "text": {
                    "type": "string",
                    "description": "Text to synthesize (max 5000 characters)."
                },
                "voice_id": {
                    "type": "string",
                    "description": "ElevenLabs voice ID. Omit to use the configured default."
                },
                "stability": {
                    "type": "number",
                    "description": "Voice stability 0.0–1.0. Lower = more expressive, higher = more consistent. Default 0.5."
                },
                "similarity_boost": {
                    "type": "number",
                    "description": "Similarity boost 0.0–1.0. Higher = closer to original voice. Default 0.75."
                }
            },
            "required": ["text"]
        })
    }

    async fn execute(&self, args: Value) -> Result<ToolResult> {
        let text = args["text"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("missing required field: text"))?;

        if text.is_empty() {
            bail!("text cannot be empty");
        }
        if text.len() > MAX_TEXT_LEN {
            bail!("text exceeds maximum length of {MAX_TEXT_LEN} characters");
        }

        let voice_id = args["voice_id"]
            .as_str()
            .unwrap_or(&self.default_voice_id)
            .to_string();

        if voice_id.is_empty() {
            bail!("voice_id is required — set default_voice_id in [elevenlabs] config or pass voice_id");
        }

        let stability = args["stability"].as_f64().unwrap_or(0.5).clamp(0.0, 1.0);
        let similarity_boost = args["similarity_boost"]
            .as_f64()
            .unwrap_or(0.75)
            .clamp(0.0, 1.0);

        let url = format!("{ELEVENLABS_TTS_URL}/{voice_id}");

        let body = json!({
            "text": text,
            "model_id": self.model_id,
            "voice_settings": {
                "stability": stability,
                "similarity_boost": similarity_boost
            }
        });

        let client = reqwest::Client::new();
        let response = client
            .post(&url)
            .header("xi-api-key", &self.api_key)
            .header("Content-Type", "application/json")
            .header("Accept", "audio/mpeg")
            .json(&body)
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let err_body = response.text().await.unwrap_or_default();
            bail!("ElevenLabs API error {status}: {err_body}");
        }

        let audio_bytes = response.bytes().await?;
        let b64 = BASE64_STD.encode(&audio_bytes);

        Ok(ToolResult {
            success: true,
            output: format!(
                "audio/mpeg;base64,{b64}\nvoice_id={voice_id} model={} bytes={}",
                self.model_id,
                audio_bytes.len()
            ),
            error: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_name_and_description() {
        let tool = ElevenLabsTtsTool::new(
            "test-key".to_string(),
            "default-voice".to_string(),
            "eleven_multilingual_v2".to_string(),
        );
        assert_eq!(tool.name(), "elevenlabs_tts");
        assert!(!tool.description().is_empty());
    }

    #[test]
    fn schema_requires_text() {
        let tool = ElevenLabsTtsTool::new(
            "test-key".to_string(),
            "default-voice".to_string(),
            "eleven_multilingual_v2".to_string(),
        );
        let schema = tool.parameters_schema();
        let required = schema["required"].as_array().unwrap();
        assert!(required.iter().any(|v| v.as_str() == Some("text")));
    }

    #[tokio::test]
    async fn empty_text_returns_error() {
        let tool = ElevenLabsTtsTool::new(
            "test-key".to_string(),
            "default-voice".to_string(),
            "eleven_multilingual_v2".to_string(),
        );
        let result = tool.execute(json!({"text": ""})).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn missing_voice_id_with_empty_default_returns_error() {
        let tool = ElevenLabsTtsTool::new(
            "test-key".to_string(),
            String::new(), // no default
            "eleven_multilingual_v2".to_string(),
        );
        let result = tool.execute(json!({"text": "hello"})).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("voice_id"));
    }
}
