//! ZeroX1 mesh-network tools.
//!
//! These tools let ZeroClaw participate in the 0x01 protocol beyond the
//! default FEEDBACK reply handled by the channel:
//!
//! | Tool | Envelope type | When to use |
//! |------|---------------|-------------|
//! | `zerox1_propose`  | PROPOSE | Initiate a new task negotiation |
//! | `zerox1_counter`  | COUNTER | Counter-propose with a different amount |
//! | `zerox1_accept`   | ACCEPT  | Formally accept an incoming PROPOSE/COUNTER |
//! | `zerox1_reject`   | REJECT  | Formally decline an incoming PROPOSE/COUNTER |
//! | `zerox1_deliver`  | DELIVER | Submit completed task results |
//!
//! ## COUNTER negotiation protocol
//!
//! Both parties may counter-propose up to `max_rounds` times (default: 2).
//! If the proposer's average reputation score is ≥ 70.0, they are allowed
//! one extra round (max_rounds = 3 instead of 2) when they initiate.
//!
//! Round numbering is 1-indexed: the first counter is round 1, second is
//! round 2, and so on.  A party that sends the final allowed counter must
//! accept or reject on the next move; it may not counter again.
//!
//! All tools resolve the node API URL from the `zerox1.node_api_url` config
//! field and authenticate with `zerox1.token` when present (hosted mode).
//!
//! ## Payload encoding
//!
//! Payload encoding is handled server-side by the node's `/negotiate/*` and
//! `/hosted/negotiate/*` endpoints. Tools post plain JSON; the node builds the
//! binary wire format (`[16-byte LE i128 amount][JSON body]`) internally.

use crate::tools::traits::{Tool, ToolResult};
use anyhow::Result;
use async_trait::async_trait;
use plugin_zerox1::Zerox1Client;
use serde_json::{json, Value};

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
            Ok(v) => v.to_string(),
            Err(e) => return Ok(ToolResult { success: false, output: String::new(), error: Some(e) }),
        };
        let message = match require_str(&args, "payload") {
            Ok(v) => v.to_string(),
            Err(e) => return Ok(ToolResult { success: false, output: String::new(), error: Some(e) }),
        };
        let conversation_id = args.get("conversation_id").and_then(Value::as_str).map(str::to_string);

        let endpoint = if self.token.is_some() {
            format!("{}/hosted/negotiate/propose", self.api_base)
        } else {
            format!("{}/negotiate/propose", self.api_base)
        };

        let mut body = serde_json::json!({
            "recipient": recipient,
            "message": message,
        });
        if let Some(ref cid) = conversation_id {
            body["conversation_id"] = Value::String(cid.clone());
        }

        let client = reqwest::Client::new();
        let mut req = client.post(&endpoint).json(&body);
        if let Some(ref tok) = self.token {
            req = req.bearer_auth(tok);
        }

        match req.send().await {
            Ok(res) if res.status().is_success() => {
                let json: Value = res.json().await.unwrap_or(Value::Null);
                let conv_id = json.get("conversation_id").and_then(Value::as_str).unwrap_or("unknown");
                Ok(ToolResult {
                    success: true,
                    output: format!("PROPOSE sent. conversation_id={conv_id}"),
                    error: None,
                })
            }
            Ok(res) => {
                let status = res.status();
                let text = res.text().await.unwrap_or_default();
                Ok(ToolResult { success: false, output: String::new(), error: Some(format!("zerox1_propose [{status}]: {text}")) })
            }
            Err(e) => Ok(send_error("zerox1_propose reqwest", e.into())),
        }
    }
}

// ── Counter ──────────────────────────────────────────────────────────────────

/// Send a `COUNTER` envelope to counter-propose different terms.
///
/// Payload wire format (mirrors the TypeScript SDK):
/// `[16-byte LE i128 amount][JSON {"round": N, "max_rounds": M, "message": "..."}]`
pub struct Zerox1CounterTool {
    api_base: String,
    token: Option<String>,
}

impl Zerox1CounterTool {
    pub fn new(api_base: impl Into<String>, token: Option<String>) -> Self {
        Self {
            api_base: api_base.into(),
            token,
        }
    }
}

#[async_trait]
impl Tool for Zerox1CounterTool {
    fn name(&self) -> &str {
        "zerox1_counter"
    }

    fn description(&self) -> &str {
        "Counter-propose different terms during a negotiation on the 0x01 mesh. \
         Send this after receiving a PROPOSE or COUNTER you want to modify. \
         Both parties may counter back up to max_rounds times (default: 2). \
         If you are the original proposer and your average reputation score >= 70, \
         you may set max_rounds = 3. Round numbering is 1-indexed."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "recipient": {
                    "type": "string",
                    "description": "Hex-encoded agent_id of the counterparty"
                },
                "conversation_id": {
                    "type": "string",
                    "description": "Conversation ID from the original PROPOSE"
                },
                "amount": {
                    "type": "integer",
                    "description": "Counter-offered amount in USDC microunits (e.g. 1000000 = 1 USDC)"
                },
                "round": {
                    "type": "integer",
                    "description": "Counter round number (1-indexed). Must be <= max_rounds."
                },
                "max_rounds": {
                    "type": "integer",
                    "description": "Max rounds as set in the original PROPOSE (default: 2)"
                },
                "message": {
                    "type": "string",
                    "description": "Explanation of your counter-offer"
                }
            },
            "required": ["recipient", "conversation_id", "amount", "round"]
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
        let amount = match args.get("amount").and_then(Value::as_u64) {
            Some(v) => v,
            None => return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("missing required integer field `amount`".into()),
            }),
        };
        let round = match args.get("round").and_then(Value::as_u64) {
            Some(v) => v as u8,
            None => return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("missing required integer field `round`".into()),
            }),
        };
        let max_rounds = args.get("max_rounds").and_then(Value::as_u64).unwrap_or(2) as u8;
        let message = args.get("message").and_then(Value::as_str).unwrap_or("");

        if round == 0 || round > max_rounds {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("round {round} is out of range [1, {max_rounds}]")),
            });
        }

        let endpoint = if self.token.is_some() {
            format!("{}/hosted/negotiate/counter", self.api_base)
        } else {
            format!("{}/negotiate/counter", self.api_base)
        };

        let body = serde_json::json!({
            "recipient": recipient,
            "conversation_id": conv_id,
            "amount_usdc_micro": amount,
            "round": round,
            "max_rounds": max_rounds,
            "message": message,
        });

        let client = reqwest::Client::new();
        let mut req = client.post(&endpoint).json(&body);
        if let Some(ref tok) = self.token {
            req = req.bearer_auth(tok);
        }

        match req.send().await {
            Ok(res) if res.status().is_success() || res.status().as_u16() == 204 => Ok(ToolResult {
                success: true,
                output: format!("COUNTER sent (round {round}/{max_rounds}, amount={amount}). conversation_id={conv_id}"),
                error: None,
            }),
            Ok(res) => {
                let status = res.status();
                let text = res.text().await.unwrap_or_default();
                Ok(ToolResult { success: false, output: String::new(), error: Some(format!("zerox1_counter [{status}]: {text}")) })
            }
            Err(e) => Ok(send_error("zerox1_counter reqwest", e.into())),
        }
    }
}

// ── Accept ───────────────────────────────────────────────────────────────────

/// Accept an incoming `PROPOSE` or `COUNTER` envelope by sending an `ACCEPT`
/// that encodes the agreed amount so both parties use the same value for
/// `lockPayment` on-chain.
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
        "Accept an incoming PROPOSE or COUNTER on the 0x01 mesh. \
         Supply the agreed `amount` (the most-recent COUNTER amount, or the original \
         PROPOSE amount if no counter was sent). This amount is encoded in the ACCEPT \
         payload so both parties use the same value when calling lockPayment on-chain."
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
                "amount": {
                    "type": "integer",
                    "description": "Agreed amount in USDC microunits (most-recent COUNTER amount, or original PROPOSE amount)"
                },
                "message": {
                    "type": "string",
                    "description": "Optional acceptance confirmation message"
                }
            },
            "required": ["recipient", "conversation_id", "amount"]
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
        let amount = match args.get("amount").and_then(Value::as_u64) {
            Some(v) => v,
            None => return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("missing required integer field `amount`".into()),
            }),
        };
        let message = args.get("message").and_then(Value::as_str).unwrap_or("");

        let endpoint = if self.token.is_some() {
            format!("{}/hosted/negotiate/accept", self.api_base)
        } else {
            format!("{}/negotiate/accept", self.api_base)
        };

        let body = serde_json::json!({
            "recipient": recipient,
            "conversation_id": conv_id,
            "amount_usdc_micro": amount,
            "message": message,
        });

        let client = reqwest::Client::new();
        let mut req = client.post(&endpoint).json(&body);
        if let Some(ref tok) = self.token {
            req = req.bearer_auth(tok);
        }

        match req.send().await {
            Ok(res) if res.status().is_success() || res.status().as_u16() == 204 => Ok(ToolResult {
                success: true,
                output: format!("ACCEPT sent (amount={amount} USDC microunits) for conversation_id={conv_id}"),
                error: None,
            }),
            Ok(res) => {
                let status = res.status();
                let text = res.text().await.unwrap_or_default();
                Ok(ToolResult { success: false, output: String::new(), error: Some(format!("zerox1_accept [{status}]: {text}")) })
            }
            Err(e) => Ok(send_error("zerox1_accept reqwest", e.into())),
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

// ── Jupiter Swap ─────────────────────────────────────────────────────────────

/// Default list of Solana token mints allowed in agent-to-agent swaps.
/// Prevents agents from being deceived into swapping into fraudulent tokens.
/// Both mainnet and devnet mints are included.
const DEFAULT_SWAP_WHITELIST: &[&str] = &[
    // SOL (wrapped)
    "So11111111111111111111111111111111111111112",
    // USDC — mainnet
    "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
    // USDC — devnet
    "4zMMC9srt5Ri5X14GAgXhaHii3GnPAEERYPJgZJDncDU",
    // USDT — mainnet
    "Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB",
    // JUP
    "JUPyiwrYJFskUPiHa7hkeR8VUtAeFoSYbKedZNsDvCN",
    // BONK
    "DezXAZ8z7PnrnRJjz3wXBoRgixCa6xjnB7YaB1pPB263",
    // RAY
    "4k3Dyjzvzp8eMZWUXbBCjEvwSkkk59S5iCNLY3QrkX6R",
    // WIF
    "EKpQGSJtjMFqKZ9KQanSqYXRcF8fBopzLHYxdM65zcjm",
    // BAGS — mainnet
    "Bags4uLBdNscWBnHmqBozrjSScnEqPx5qZBzLiqnRVN7",
];

/// Execute a Jupiter exchange swap via the local node API.
pub struct Zerox1JupiterSwapTool {
    api_base: String,
    token: Option<String>,
    /// Override swap whitelist. `None` = use DEFAULT_SWAP_WHITELIST.
    /// `Some(empty vec)` = whitelist disabled (allow any mint).
    swap_whitelist: Option<Vec<String>>,
}

impl Zerox1JupiterSwapTool {
    pub fn new(api_base: impl Into<String>, token: Option<String>) -> Self {
        Self {
            api_base: api_base.into(),
            token,
            swap_whitelist: None,
        }
    }

    /// Override the default token whitelist. Pass an empty vec to disable enforcement.
    pub fn with_whitelist(mut self, whitelist: Vec<String>) -> Self {
        self.swap_whitelist = Some(whitelist);
        self
    }
}

#[async_trait]
impl Tool for Zerox1JupiterSwapTool {
    fn name(&self) -> &str {
        "execute_jupiter_swap"
    }

    fn description(&self) -> &str {
        "Execute a token swap on Solana via the Jupiter DEX aggregator using the node's local wallet signature. \
         This should ONLY be used when the user explicitly asks to trade or swap cryptocurrencies."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "input_mint": {
                    "type": "string",
                    "description": "The mint address of the input token (e.g. So11111111111111111111111111111111111111112 for SOL or EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v for USDC)"
                },
                "output_mint": {
                    "type": "string",
                    "description": "The mint address of the output token to receive"
                },
                "amount": {
                    "type": "integer",
                    "description": "The source token amount in base minimum units (e.g. lamports for SOL)"
                },
                "slippage_bps": {
                    "type": "integer",
                    "description": "Optional acceptable slippage in basic points (e.g. 50 for 0.5%)"
                }
            },
            "required": ["input_mint", "output_mint", "amount"]
        })
    }

    async fn execute(&self, args: Value) -> Result<ToolResult> {
        let input_mint = match require_str(&args, "input_mint") {
            Ok(v) => v,
            Err(e) => return Ok(ToolResult { success: false, output: String::new(), error: Some(e) }),
        };
        let output_mint = match require_str(&args, "output_mint") {
            Ok(v) => v,
            Err(e) => return Ok(ToolResult { success: false, output: String::new(), error: Some(e) }),
        };
        let amount = match args.get("amount").and_then(Value::as_u64) {
            Some(v) => v,
            None => return Ok(ToolResult { success: false, output: String::new(), error: Some("amount must be an integer".into()) }),
        };
        let slippage_bps = args.get("slippage_bps").and_then(Value::as_u64).map(|v| v as u16);

        // Whitelist check — prevent swaps into fraudulent tokens.
        let mint_allowed = |mint: &str| -> bool {
            match &self.swap_whitelist {
                // Custom whitelist: empty = disabled; non-empty = must be in list.
                Some(custom) => custom.is_empty() || custom.iter().any(|s| s == mint),
                // Default: check against built-in constant list.
                None => DEFAULT_SWAP_WHITELIST.contains(&mint),
            }
        };
        if !mint_allowed(input_mint) {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("input_mint {input_mint} is not in the token whitelist")),
            });
        }
        if !mint_allowed(output_mint) {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("output_mint {output_mint} is not in the token whitelist")),
            });
        }

        let url = format!("{}/trade/swap", self.api_base);
        let client = reqwest::Client::new();
        let mut req = client.post(&url);

        if let Some(ref tok) = self.token {
            req = req.bearer_auth(tok);
        }

        let body = json!({
            "input_mint": input_mint,
            "output_mint": output_mint,
            "amount": amount,
            "slippage_bps": slippage_bps,
        });

        match req.json(&body).send().await {
            Ok(res) => {
                let status = res.status();
                if status.is_success() {
                    let text = res.text().await.unwrap_or_default();
                    Ok(ToolResult {
                        success: true,
                        output: format!("Jupiter swap successful: {text}"),
                        error: None,
                    })
                } else {
                    let err_text = res.text().await.unwrap_or_default();
                    Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some(format!("Swap failed [{status}]: {err_text}")),
                    })
                }
            }
            Err(e) => Ok(send_error("execute_jupiter_swap reqwest", e.into())),
        }
    }
}

// ── Skill Install ────────────────────────────────────────────────────────────

/// Install a new skill from a public HTTPS URL and hot-reload the agent.
///
/// Flow:
///   1. POST /skill/install-url  — node fetches the SKILL.toml and writes it to disk
///   2. POST /agent/reload       — sends SIGTERM; NodeService auto-restarts ZeroClaw
///      with the new skill loaded
///
/// The agent will be briefly unavailable while restarting (~2-3 s).
pub struct Zerox1SkillInstallTool {
    api_base: String,
    token: Option<String>,
}

impl Zerox1SkillInstallTool {
    pub fn new(api_base: impl Into<String>, token: Option<String>) -> Self {
        Self { api_base: api_base.into(), token }
    }
}

#[async_trait]
impl Tool for Zerox1SkillInstallTool {
    fn name(&self) -> &str {
        "skill_install"
    }

    fn description(&self) -> &str {
        "Install a new skill from a public HTTPS URL and restart the agent so the skill \
         is immediately available. The URL must point to a raw SKILL.toml file (e.g. a \
         GitHub raw URL). After installation the agent restarts — confirm with the user \
         before calling this."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Short identifier for the skill, e.g. \"solana-token-scanner\". \
                                    Alphanumeric, hyphens and underscores only."
                },
                "url": {
                    "type": "string",
                    "description": "Public HTTPS URL pointing directly to the skill's SKILL.toml \
                                    (e.g. https://raw.githubusercontent.com/org/repo/main/SKILL.toml)"
                }
            },
            "required": ["name", "url"]
        })
    }

    async fn execute(&self, args: Value) -> Result<ToolResult> {
        let name = match require_str(&args, "name") {
            Ok(v) => v,
            Err(e) => return Ok(ToolResult { success: false, output: String::new(), error: Some(e) }),
        };
        let url = match require_str(&args, "url") {
            Ok(v) => v,
            Err(e) => return Ok(ToolResult { success: false, output: String::new(), error: Some(e) }),
        };

        let client = reqwest::Client::new();

        // ── Step 1: install the skill ─────────────────────────────────────────
        let install_url = format!("{}/skill/install-url", self.api_base);
        let body = json!({ "name": name, "url": url });
        let mut req = client.post(&install_url).json(&body);
        if let Some(ref tok) = self.token {
            req = req.bearer_auth(tok);
        }

        match req.send().await {
            Ok(res) if res.status().is_success() => { /* continue to reload */ }
            Ok(res) => {
                let status = res.status();
                let text = res.text().await.unwrap_or_default();
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("skill_install: install failed [{status}]: {text}")),
                });
            }
            Err(e) => return Ok(send_error("skill_install: install-url reqwest", e.into())),
        }

        // ── Step 2: reload the agent ──────────────────────────────────────────
        let reload_url = format!("{}/agent/reload", self.api_base);
        let mut req = client.post(&reload_url);
        if let Some(ref tok) = self.token {
            req = req.bearer_auth(tok);
        }

        match req.send().await {
            Ok(res) if res.status().is_success() || res.status().as_u16() == 204 => {
                Ok(ToolResult {
                    success: true,
                    output: format!(
                        "Skill '{name}' installed from {url}\n\
                         Agent is restarting to load the new skill (~3 s). \
                         Resume this conversation once it's back online."
                    ),
                    error: None,
                })
            }
            Ok(res) => {
                let status = res.status();
                let text = res.text().await.unwrap_or_default();
                // Skill was written but reload failed — still a partial success.
                Ok(ToolResult {
                    success: false,
                    output: format!("Skill '{name}' installed but reload failed [{status}]: {text}\n\
                                     Restart the agent manually to activate it."),
                    error: Some(format!("reload [{status}]: {text}")),
                })
            }
            Err(e) => Ok(ToolResult {
                success: false,
                output: format!("Skill '{name}' installed but reload request failed: {e}\n\
                                 Restart the agent manually to activate it."),
                error: Some(e.to_string()),
            }),
        }
    }
}

// ── Bags Launch ──────────────────────────────────────────────────────────────

/// Launch a new token on Bags.fm via the local node API.
///
/// The node handles all signing and on-chain interaction — ZeroClaw only
/// needs to supply the token metadata. The wallet used is the node's own
/// Ed25519 identity key (the hot wallet shown during onboarding).
pub struct Zerox1BagsLaunchTool {
    api_base: String,
    token: Option<String>,
}

impl Zerox1BagsLaunchTool {
    pub fn new(api_base: impl Into<String>, token: Option<String>) -> Self {
        Self {
            api_base: api_base.into(),
            token,
        }
    }
}

#[async_trait]
impl Tool for Zerox1BagsLaunchTool {
    fn name(&self) -> &str {
        "bags_launch_token"
    }

    fn description(&self) -> &str {
        "Launch a new token on Bags.fm (bags.fm) using the agent's on-chain wallet. \
         Provide the token name, ticker symbol, and description. Optionally include \
         social links and an initial SOL buy amount. The node signs and broadcasts \
         the transaction — no private key handling is required here. \
         Only use this when the user explicitly asks to launch or create a token."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Full token name, e.g. \"My Cool Token\" (max 100 chars)"
                },
                "symbol": {
                    "type": "string",
                    "description": "Ticker symbol, ASCII alphanumeric only, e.g. \"MCT\" (max 10 chars)"
                },
                "description": {
                    "type": "string",
                    "description": "Token description shown on Bags.fm (max 1000 chars)"
                },
                "image_url": {
                    "type": "string",
                    "description": "Optional HTTPS URL to the token logo image"
                },
                "website_url": {
                    "type": "string",
                    "description": "Optional project website URL"
                },
                "twitter_url": {
                    "type": "string",
                    "description": "Optional Twitter/X profile URL"
                },
                "telegram_url": {
                    "type": "string",
                    "description": "Optional Telegram group URL"
                },
                "initial_buy_lamports": {
                    "type": "integer",
                    "description": "Optional SOL amount in lamports to use for the initial token buy (e.g. 10000000 = 0.01 SOL). Omit or set 0 to skip."
                }
            },
            "required": ["name", "symbol", "description"]
        })
    }

    async fn execute(&self, args: Value) -> Result<ToolResult> {
        let name = match require_str(&args, "name") {
            Ok(v) => v,
            Err(e) => return Ok(ToolResult { success: false, output: String::new(), error: Some(e) }),
        };
        let symbol = match require_str(&args, "symbol") {
            Ok(v) => v,
            Err(e) => return Ok(ToolResult { success: false, output: String::new(), error: Some(e) }),
        };
        let description = match require_str(&args, "description") {
            Ok(v) => v,
            Err(e) => return Ok(ToolResult { success: false, output: String::new(), error: Some(e) }),
        };

        let mut body = json!({
            "name": name,
            "symbol": symbol,
            "description": description,
        });

        if let Some(u) = args.get("image_url").and_then(Value::as_str) {
            body["image_url"] = u.into();
        }
        if let Some(u) = args.get("website_url").and_then(Value::as_str) {
            body["website_url"] = u.into();
        }
        if let Some(u) = args.get("twitter_url").and_then(Value::as_str) {
            body["twitter_url"] = u.into();
        }
        if let Some(u) = args.get("telegram_url").and_then(Value::as_str) {
            body["telegram_url"] = u.into();
        }
        if let Some(lamports) = args.get("initial_buy_lamports").and_then(Value::as_u64) {
            if lamports > 0 {
                body["initial_buy_lamports"] = lamports.into();
            }
        }

        let url = format!("{}/bags/launch", self.api_base);
        let client = reqwest::Client::new();
        let mut req = client.post(&url).json(&body);

        if let Some(ref tok) = self.token {
            req = req.bearer_auth(tok);
        }

        match req.send().await {
            Ok(res) => {
                let status = res.status();
                if status.is_success() {
                    let json: Value = res.json().await.unwrap_or(Value::Null);
                    let token_mint = json.get("token_mint").and_then(Value::as_str).unwrap_or("unknown");
                    let txid = json.get("txid").and_then(Value::as_str).unwrap_or("unknown");
                    let ipfs_uri = json.get("ipfs_uri").and_then(Value::as_str).unwrap_or("");
                    Ok(ToolResult {
                        success: true,
                        output: format!(
                            "Token launched on Bags.fm!\n\
                             Name:       {name}\n\
                             Symbol:     {symbol}\n\
                             Mint:       {token_mint}\n\
                             Txid:       {txid}\n\
                             Metadata:   {ipfs_uri}\n\
                             View on Bags.fm: https://bags.fm/token/{token_mint}"
                        ),
                        error: None,
                    })
                } else {
                    let err_text = res.text().await.unwrap_or_default();
                    Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some(format!("bags_launch_token [{status}]: {err_text}")),
                    })
                }
            }
            Err(e) => Ok(send_error("bags_launch_token reqwest", e.into())),
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
        assert_eq!(Zerox1CounterTool::new(api, None).name(), "zerox1_counter");
        assert_eq!(Zerox1AcceptTool::new(api, None).name(), "zerox1_accept");
        assert_eq!(Zerox1RejectTool::new(api, None).name(), "zerox1_reject");
        assert_eq!(Zerox1DeliverTool::new(api, None).name(), "zerox1_deliver");
        assert_eq!(Zerox1BagsLaunchTool::new(api, None).name(), "bags_launch_token");
        assert_eq!(Zerox1SkillInstallTool::new(api, None).name(), "skill_install");
    }

    #[test]
    fn tools_have_schemas_with_required() {
        let api = "http://127.0.0.1:9090";
        for schema in [
            Zerox1ProposeTool::new(api, None).parameters_schema(),
            Zerox1CounterTool::new(api, None).parameters_schema(),
            Zerox1AcceptTool::new(api, None).parameters_schema(),
            Zerox1RejectTool::new(api, None).parameters_schema(),
            Zerox1DeliverTool::new(api, None).parameters_schema(),
            Zerox1BagsLaunchTool::new(api, None).parameters_schema(),
            Zerox1SkillInstallTool::new(api, None).parameters_schema(),
        ] {
            assert_eq!(schema["type"], "object");
            assert!(schema["required"].is_array());
        }
    }

    #[tokio::test]
    async fn bags_launch_returns_error_on_missing_name() {
        let tool = Zerox1BagsLaunchTool::new("http://127.0.0.1:9090", None);
        let result = tool
            .execute(json!({ "symbol": "TST", "description": "test" }))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("name"));
    }

    #[tokio::test]
    async fn bags_launch_returns_error_on_missing_symbol() {
        let tool = Zerox1BagsLaunchTool::new("http://127.0.0.1:9090", None);
        let result = tool
            .execute(json!({ "name": "Test Token", "description": "test" }))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("symbol"));
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
    async fn counter_rejects_round_out_of_range() {
        let tool = Zerox1CounterTool::new("http://127.0.0.1:9090", None);
        let result = tool
            .execute(json!({
                "recipient": "aabb",
                "conversation_id": "deadbeef",
                "amount": 1_000_000,
                "round": 5,
                "max_rounds": 2
            }))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("out of range"));
    }

    #[tokio::test]
    async fn counter_returns_error_on_missing_amount() {
        let tool = Zerox1CounterTool::new("http://127.0.0.1:9090", None);
        let result = tool
            .execute(json!({
                "recipient": "aabb",
                "conversation_id": "deadbeef",
                "round": 1
            }))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("amount"));
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
