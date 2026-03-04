//! ZeroX1 mesh-network tools.
//!
//! These tools let ZeroClaw participate in the 0x01 protocol beyond the
//! default FEEDBACK reply handled by the channel:
//!
//! | Tool | Envelope type | When to use |
//! |------|---------------|-------------|
//! | `zerox1_propose`  | PROPOSE | Initiate a new task negotiation |
//! | `zerox1_accept`   | ACCEPT  | Formally accept an incoming PROPOSE |
//! | `zerox1_reject`   | REJECT  | Formally decline an incoming PROPOSE |
//! | `zerox1_deliver`  | DELIVER | Submit completed task results |
//!
//! All tools resolve the node API URL from the `zerox1.node_api_url` config
//! field and authenticate with `zerox1.token` when present (hosted mode).

use crate::tools::traits::{Tool, ToolResult};
use anyhow::Result;
use async_trait::async_trait;
use plugin_zerox1::Zerox1Client;
use serde_json::{json, Value};
use uuid::Uuid;

// ── helpers ──────────────────────────────────────────────────────────────────

/// Extract a required string field from `args`, returning a descriptive error
/// on missing/wrong-type fields.
fn require_str<'a>(args: &'a Value, key: &str) -> Result<&'a str, String> {
    args.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("missing required string field `{key}`"))
}

/// Build a ToolResult from a send error.
fn send_error(context: &str, err: anyhow::Error) -> ToolResult {
    ToolResult {
        success: false,
        output: String::new(),
        error: Some(format!("{context}: {err:#}")),
    }
}

/// Obtain a [`Zerox1Client`] from the tool's stored fields.
#[allow(clippy::ref_option)]
fn make_client(api_base: &str, token: &Option<String>) -> Result<Zerox1Client> {
    Zerox1Client::new(api_base, token.clone())
}

// ── Propose ──────────────────────────────────────────────────────────────────

/// Initiate a task negotiation on the 0x01 mesh by sending a `PROPOSE`
/// envelope to a specific agent.
pub struct Zerox1ProposeTool {
    api_base: String,
    token: Option<String>,
}

impl Zerox1ProposeTool {
    pub fn new(api_base: impl Into<String>, token: Option<String>) -> Self {
        Self {
            api_base: api_base.into(),
            token,
        }
    }
}

#[async_trait]
impl Tool for Zerox1ProposeTool {
    fn name(&self) -> &str {
        "zerox1_propose"
    }

    fn description(&self) -> &str {
        "Send a PROPOSE envelope to another agent on the 0x01 mesh to initiate a task negotiation. \
         The agent can respond with ACCEPT, REJECT, or COUNTER."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "recipient": {
                    "type": "string",
                    "description": "Hex-encoded 32-byte agent_id of the target agent"
                },
                "payload": {
                    "type": "string",
                    "description": "Task description or proposal details (plain text or JSON)"
                },
                "conversation_id": {
                    "type": "string",
                    "description": "Optional 16-byte hex conversation ID. Auto-generated if omitted."
                }
            },
            "required": ["recipient", "payload"]
        })
    }

    async fn execute(&self, args: Value) -> Result<ToolResult> {
        let recipient = match require_str(&args, "recipient") {
            Ok(v) => v,
            Err(e) => return Ok(ToolResult { success: false, output: String::new(), error: Some(e) }),
        };
        let payload = match require_str(&args, "payload") {
            Ok(v) => v,
            Err(e) => return Ok(ToolResult { success: false, output: String::new(), error: Some(e) }),
        };
        let conv_id = args
            .get("conversation_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let conv_id = if conv_id.is_empty() {
            Uuid::new_v4().simple().to_string()
        } else {
            conv_id
        };

        let client = match make_client(&self.api_base, &self.token) {
            Ok(c) => c,
            Err(e) => return Ok(send_error("client init", e)),
        };

        let result = if let Some(ref tok) = self.token {
            client
                .hosted_send(tok, "PROPOSE", Some(recipient), &conv_id, payload.as_bytes())
                .await
        } else {
            client
                .send_envelope("PROPOSE", Some(recipient), &conv_id, payload.as_bytes())
                .await
                .map(|_| ())
        };

        match result {
            Ok(()) => Ok(ToolResult {
                success: true,
                output: format!("PROPOSE sent. conversation_id={conv_id}"),
                error: None,
            }),
            Err(e) => Ok(send_error("zerox1_propose", e)),
        }
    }
}

// ── Accept ───────────────────────────────────────────────────────────────────

/// Accept an incoming `PROPOSE` envelope by sending an `ACCEPT` back.
pub struct Zerox1AcceptTool {
    api_base: String,
    token: Option<String>,
}

impl Zerox1AcceptTool {
    pub fn new(api_base: impl Into<String>, token: Option<String>) -> Self {
        Self {
            api_base: api_base.into(),
            token,
        }
    }
}

#[async_trait]
impl Tool for Zerox1AcceptTool {
    fn name(&self) -> &str {
        "zerox1_accept"
    }

    fn description(&self) -> &str {
        "Accept an incoming PROPOSE envelope on the 0x01 mesh by sending an ACCEPT reply. \
         Use the sender and conversation_id from the original PROPOSE message."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "recipient": {
                    "type": "string",
                    "description": "Hex-encoded agent_id of the proposing agent (sender of the PROPOSE)"
                },
                "conversation_id": {
                    "type": "string",
                    "description": "Conversation ID from the original PROPOSE message"
                },
                "terms": {
                    "type": "string",
                    "description": "Optional acceptance terms or confirmation message"
                }
            },
            "required": ["recipient", "conversation_id"]
        })
    }

    async fn execute(&self, args: Value) -> Result<ToolResult> {
        let recipient = match require_str(&args, "recipient") {
            Ok(v) => v,
            Err(e) => return Ok(ToolResult { success: false, output: String::new(), error: Some(e) }),
        };
        let conv_id = match require_str(&args, "conversation_id") {
            Ok(v) => v,
            Err(e) => return Ok(ToolResult { success: false, output: String::new(), error: Some(e) }),
        };
        let terms = args
            .get("terms")
            .and_then(Value::as_str)
            .unwrap_or("accepted");

        let client = match make_client(&self.api_base, &self.token) {
            Ok(c) => c,
            Err(e) => return Ok(send_error("client init", e)),
        };

        let result = if let Some(ref tok) = self.token {
            client
                .hosted_send(tok, "ACCEPT", Some(recipient), conv_id, terms.as_bytes())
                .await
        } else {
            client
                .send_envelope("ACCEPT", Some(recipient), conv_id, terms.as_bytes())
                .await
                .map(|_| ())
        };

        match result {
            Ok(()) => Ok(ToolResult {
                success: true,
                output: format!("ACCEPT sent for conversation_id={conv_id}"),
                error: None,
            }),
            Err(e) => Ok(send_error("zerox1_accept", e)),
        }
    }
}

// ── Reject ───────────────────────────────────────────────────────────────────

/// Decline an incoming `PROPOSE` envelope by sending a `REJECT`.
pub struct Zerox1RejectTool {
    api_base: String,
    token: Option<String>,
}

impl Zerox1RejectTool {
    pub fn new(api_base: impl Into<String>, token: Option<String>) -> Self {
        Self {
            api_base: api_base.into(),
            token,
        }
    }
}

#[async_trait]
impl Tool for Zerox1RejectTool {
    fn name(&self) -> &str {
        "zerox1_reject"
    }

    fn description(&self) -> &str {
        "Reject an incoming PROPOSE envelope on the 0x01 mesh by sending a REJECT reply. \
         Optionally include a reason."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "recipient": {
                    "type": "string",
                    "description": "Hex-encoded agent_id of the proposing agent"
                },
                "conversation_id": {
                    "type": "string",
                    "description": "Conversation ID from the original PROPOSE message"
                },
                "reason": {
                    "type": "string",
                    "description": "Optional reason for declining"
                }
            },
            "required": ["recipient", "conversation_id"]
        })
    }

    async fn execute(&self, args: Value) -> Result<ToolResult> {
        let recipient = match require_str(&args, "recipient") {
            Ok(v) => v,
            Err(e) => return Ok(ToolResult { success: false, output: String::new(), error: Some(e) }),
        };
        let conv_id = match require_str(&args, "conversation_id") {
            Ok(v) => v,
            Err(e) => return Ok(ToolResult { success: false, output: String::new(), error: Some(e) }),
        };
        let reason = args
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or("rejected");

        let client = match make_client(&self.api_base, &self.token) {
            Ok(c) => c,
            Err(e) => return Ok(send_error("client init", e)),
        };

        let result = if let Some(ref tok) = self.token {
            client
                .hosted_send(tok, "REJECT", Some(recipient), conv_id, reason.as_bytes())
                .await
        } else {
            client
                .send_envelope("REJECT", Some(recipient), conv_id, reason.as_bytes())
                .await
                .map(|_| ())
        };

        match result {
            Ok(()) => Ok(ToolResult {
                success: true,
                output: format!("REJECT sent for conversation_id={conv_id}"),
                error: None,
            }),
            Err(e) => Ok(send_error("zerox1_reject", e)),
        }
    }
}

// ── Deliver ──────────────────────────────────────────────────────────────────

/// Deliver completed task results by sending a `DELIVER` envelope.
pub struct Zerox1DeliverTool {
    api_base: String,
    token: Option<String>,
}

impl Zerox1DeliverTool {
    pub fn new(api_base: impl Into<String>, token: Option<String>) -> Self {
        Self {
            api_base: api_base.into(),
            token,
        }
    }
}

#[async_trait]
impl Tool for Zerox1DeliverTool {
    fn name(&self) -> &str {
        "zerox1_deliver"
    }

    fn description(&self) -> &str {
        "Deliver the completed result of a task to the requesting agent on the 0x01 mesh \
         by sending a DELIVER envelope. Use after completing work requested via a PROPOSE."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "recipient": {
                    "type": "string",
                    "description": "Hex-encoded agent_id of the agent that sent the PROPOSE"
                },
                "conversation_id": {
                    "type": "string",
                    "description": "Conversation ID from the original PROPOSE message"
                },
                "result": {
                    "type": "string",
                    "description": "The completed task result (plain text, JSON, or summary)"
                }
            },
            "required": ["recipient", "conversation_id", "result"]
        })
    }

    async fn execute(&self, args: Value) -> Result<ToolResult> {
        let recipient = match require_str(&args, "recipient") {
            Ok(v) => v,
            Err(e) => return Ok(ToolResult { success: false, output: String::new(), error: Some(e) }),
        };
        let conv_id = match require_str(&args, "conversation_id") {
            Ok(v) => v,
            Err(e) => return Ok(ToolResult { success: false, output: String::new(), error: Some(e) }),
        };
        let result_text = match require_str(&args, "result") {
            Ok(v) => v,
            Err(e) => return Ok(ToolResult { success: false, output: String::new(), error: Some(e) }),
        };

        let client = match make_client(&self.api_base, &self.token) {
            Ok(c) => c,
            Err(e) => return Ok(send_error("client init", e)),
        };

        let send_result = if let Some(ref tok) = self.token {
            client
                .hosted_send(tok, "DELIVER", Some(recipient), conv_id, result_text.as_bytes())
                .await
        } else {
            client
                .send_envelope("DELIVER", Some(recipient), conv_id, result_text.as_bytes())
                .await
                .map(|_| ())
        };

        match send_result {
            Ok(()) => Ok(ToolResult {
                success: true,
                output: format!("DELIVER sent for conversation_id={conv_id}"),
                error: None,
            }),
            Err(e) => Ok(send_error("zerox1_deliver", e)),
        }
    }
}

// ── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tools_have_correct_names() {
        let api = "http://127.0.0.1:9090";
        assert_eq!(Zerox1ProposeTool::new(api, None).name(), "zerox1_propose");
        assert_eq!(Zerox1AcceptTool::new(api, None).name(), "zerox1_accept");
        assert_eq!(Zerox1RejectTool::new(api, None).name(), "zerox1_reject");
        assert_eq!(Zerox1DeliverTool::new(api, None).name(), "zerox1_deliver");
    }

    #[test]
    fn tools_have_schemas_with_required() {
        let api = "http://127.0.0.1:9090";
        for schema in [
            Zerox1ProposeTool::new(api, None).parameters_schema(),
            Zerox1AcceptTool::new(api, None).parameters_schema(),
            Zerox1RejectTool::new(api, None).parameters_schema(),
            Zerox1DeliverTool::new(api, None).parameters_schema(),
        ] {
            assert_eq!(schema["type"], "object");
            assert!(schema["required"].is_array());
        }
    }

    #[tokio::test]
    async fn propose_returns_error_on_missing_recipient() {
        let tool = Zerox1ProposeTool::new("http://127.0.0.1:9090", None);
        let result = tool
            .execute(json!({ "payload": "do something" }))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("recipient"));
    }

    #[tokio::test]
    async fn accept_returns_error_on_missing_conversation_id() {
        let tool = Zerox1AcceptTool::new("http://127.0.0.1:9090", None);
        let result = tool
            .execute(json!({ "recipient": "aabb" }))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("conversation_id"));
    }
}
