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
use std::time::Duration;

const ELEVENLABS_TTS_URL: &str = "https://api.elevenlabs.io/v1/text-to-speech";
/// ElevenLabs enforces a 5 000 *character* (not byte) limit per request.
const MAX_TEXT_CHARS: usize = 5_000;
/// Reasonable wall-clock cap for a single TTS call (large texts can be slow).
const REQUEST_TIMEOUT_SECS: u64 = 60;

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
        let char_count = text.chars().count();
        if char_count > MAX_TEXT_CHARS {
            bail!(
                "text is {char_count} characters — exceeds the {MAX_TEXT_CHARS}-character limit"
            );
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

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS))
            .build()?;
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
        let size_bytes = audio_bytes.len();
        let b64 = BASE64_STD.encode(&audio_bytes);

        let result = json!({
            "content_type": "audio/mpeg",
            "encoding": "base64",
            "data": b64,
            "voice_id": voice_id,
            "model_id": self.model_id,
            "size_bytes": size_bytes,
        });

        Ok(ToolResult {
            success: true,
            output: result.to_string(),
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

    #[tokio::test]
    async fn text_over_char_limit_returns_error() {
        let tool = ElevenLabsTtsTool::new(
            "test-key".to_string(),
            "default-voice".to_string(),
            "eleven_multilingual_v2".to_string(),
        );
        // 5001 ASCII chars — over limit
        let long_text = "a".repeat(MAX_TEXT_CHARS + 1);
        let result = tool.execute(json!({"text": long_text})).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("character"));
    }

    #[tokio::test]
    async fn unicode_text_at_char_limit_accepted() {
        // "à" is 2 bytes but 1 char — 5000 "à" should pass the char check
        // (it will fail later at the HTTP call with a fake key, but not on validation)
        let tool = ElevenLabsTtsTool::new(
            "test-key".to_string(),
            "default-voice".to_string(),
            "eleven_multilingual_v2".to_string(),
        );
        let text = "à".repeat(MAX_TEXT_CHARS);
        assert_eq!(text.chars().count(), MAX_TEXT_CHARS);
        // The char-limit validation must pass (error comes from HTTP, not validation)
        let result = tool.execute(json!({"text": text})).await;
        // reqwest will fail because "test-key" is invalid — but NOT with a char-limit error
        if let Err(e) = result {
            assert!(!e.to_string().contains("character"), "unexpected char-limit error: {e}");
        }
    }
}
