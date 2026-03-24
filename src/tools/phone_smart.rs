//! Smart phone tools — high-level aggregators that collect raw phone data,
//! extract structured intelligence on-device, and return only the abstracted
//! result to the LLM.
//!
//! Raw SMS content, contact names, health numbers, and exact GPS coordinates
//! never appear in the tool output.  The LLM receives only inferred meaning.
//!
//! Compiled only when the `phone-smart` feature is active (default = enabled).
//! Disable with `--no-default-features` for a stripped debug build without
//! smart tools (PhoneDayBriefTool, PhoneSmsBriefTool, PhoneCommsSummaryTool,
//! PhoneContextNowTool).
//!
//! # Bridge response envelope
//! Every PhoneBridgeServer.kt endpoint returns:
//!   `{"ok": true, "data": <payload>}`
//! All field access must go through `v["data"][...]`.
//!
//! # Verified field names (from PhoneBridgeServer.kt source)
//! - SMS item:          `body`, `date`, `address`, `id`
//! - Calendar item:     `dtstart`, `dtend`, `title`, `description`, `id`
//! - Notification item: `packageName` (NOT `package`), `text`, `title`, `postTime`
//! - Recovery:          `score`, `readiness`, `last_sleep_min`, `deep_sleep_pct`, `rem_sleep_pct`, `insights`
//! - Context:           `timezone.offset_ms`, `battery.percent`, `battery.status`, `network.type`, `calendar_today`, `health_today`
//! - Carrier:           `roaming`, `call_state`
//! - Audio/profile:     `dnd_mode` ("all"|"priority"|"none"|"alarms"), `ringer_mode`
//! - Call log item:     `type`, `date`, `duration`, `number`, `id`  (no `contact_name`)
//! - Activity:          `steps_since_reboot`
//! - App usage item:    `package`, `app_name`, `foreground_ms`, `foreground_min`

use crate::tools::traits::{Tool, ToolResult};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::OnceLock;
use std::time::Duration;
use tokio::join;

// ── static regex (compiled once at first use, never per-call) ─────────────

fn amount_re() -> &'static regex_lite::Regex {
    static RE: OnceLock<regex_lite::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        regex_lite::Regex::new(
            r"(?i)(?:[\$£€¥]|USDC|USD|GBP|EUR)\s*[\d,]+(?:\.\d{2})?|[\d,]+(?:\.\d{2})?\s*(?:USDC|USD|dollars?|GBP|EUR)",
        )
        .expect("amount regex")
    })
}

fn date_re() -> &'static regex_lite::Regex {
    static RE: OnceLock<regex_lite::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        regex_lite::Regex::new(
            r"(?i)(?:due|by|before|on|pay by)[\s:]*([A-Za-z]+ \d{1,2}(?:st|nd|rd|th)?(?:,?\s*\d{4})?|\d{1,2}[\/\-]\d{1,2}(?:[\/\-]\d{2,4})?)",
        )
        .expect("date regex")
    })
}

fn otp_re() -> &'static regex_lite::Regex {
    static RE: OnceLock<regex_lite::Regex> = OnceLock::new();
    RE.get_or_init(|| regex_lite::Regex::new(r"\b\d{4,8}\b").expect("otp regex"))
}

// ── shared HTTP helpers ────────────────────────────────────────────────────

fn build_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .unwrap_or_default()
}

fn ok(v: Value) -> anyhow::Result<ToolResult> {
    Ok(ToolResult { success: true, output: v.to_string(), error: None })
}

fn err(msg: impl Into<String>) -> anyhow::Result<ToolResult> {
    Ok(ToolResult { success: false, output: String::new(), error: Some(msg.into()) })
}

/// Fire a GET to the bridge; unwrap the `{"ok":true,"data":...}` envelope
/// and return the inner `data` value, or None on any failure / non-ok response.
async fn bridge_get(
    client: &reqwest::Client,
    base: &str,
    secret: &str,
    path: &str,
) -> Option<Value> {
    let v: Value = client
        .get(format!("{base}{path}"))
        .header("X-Bridge-Token", secret)
        .send()
        .await
        .ok()?
        .json()
        .await
        .ok()?;
    // Unwrap the envelope: return `data` if ok=true, else None.
    if v["ok"].as_bool().unwrap_or(false) {
        Some(v["data"].clone())
    } else {
        None
    }
}

// ── time helpers ───────────────────────────────────────────────────────────

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

/// Compute time-of-day label from a Unix-ms timestamp and a UTC-offset in minutes.
/// Uses rem_euclid so negative offsets (UTC-N) are handled correctly.
fn time_of_day(ts_ms: i64, utc_offset_min: i64) -> &'static str {
    let offset = utc_offset_min.clamp(-840, 840);
    let local_sec = ts_ms / 1000 + offset * 60;
    let hour = local_sec.rem_euclid(86_400) / 3600;
    match hour {
        5..=11  => "morning",
        12..=16 => "afternoon",
        17..=20 => "evening",
        _       => "night",
    }
}

// ── heuristic extraction helpers ──────────────────────────────────────────

fn extract_amounts(text: &str) -> Vec<(String, Option<String>)> {
    use std::collections::HashSet;
    let mut found: HashSet<String> = HashSet::new();
    let mut results = Vec::new();

    for cap in amount_re().captures_iter(text) {
        let amount = cap[0].trim().to_string();
        if found.contains(&amount) { continue; }
        found.insert(amount.clone());

        // ±100-byte window for date search — walk to char boundaries.
        let start  = cap.get(0).map(|m| m.start()).unwrap_or(0);
        let raw_ws = start.saturating_sub(100);
        let raw_we = (start + 100).min(text.len());
        let ws = (raw_ws..=start).find(|&i| text.is_char_boundary(i)).unwrap_or(start);
        let we = (start..=raw_we).rev().find(|&i| text.is_char_boundary(i)).unwrap_or(start);
        let window = &text[ws..we];

        let due = date_re().captures(window)
            .and_then(|dc| dc.get(1))
            .map(|m| m.as_str().trim().to_string());

        results.push((amount, due));
    }
    results
}

fn needs_reply(text: &str) -> bool {
    let t = text.to_lowercase();
    text.contains('?')
        || t.contains("please reply") || t.contains("let me know")
        || t.contains("get back to me") || t.contains("respond")
        || t.contains("confirm") || t.contains("asap") || t.contains("urgent")
}

fn is_otp(text: &str) -> bool {
    let t = text.to_lowercase();
    (t.contains("code") || t.contains("otp") || t.contains("verification")
        || t.contains("one-time") || t.contains("pin") || t.contains("token"))
        && otp_re().is_match(text)
}

fn is_spam(text: &str) -> bool {
    let t = text.to_lowercase();
    t.contains("click here") || t.contains("winner") || t.contains("free gift")
        || t.contains("you have been selected") || t.contains("claim your")
        || t.contains("limited offer") || t.contains("congratulations")
        || t.contains("lottery") || t.contains("unsubscribe")
        || t.contains("promo code") || t.contains("discount code")
}

fn urgency(text: &str) -> &'static str {
    let t = text.to_lowercase();
    if t.contains("urgent") || t.contains("emergency") || t.contains("overdue")
        || t.contains("final notice") || t.contains("immediate") || t.contains("asap")
        || t.contains("past due") || t.contains("action required")
    {
        "high"
    } else if needs_reply(text) || t.contains("due soon") || t.contains("reminder") {
        "medium"
    } else {
        "low"
    }
}

/// Classify a notification package name into a category.
/// Bridge returns `packageName` (e.g. "com.whatsapp", "com.google.android.gm").
fn notif_category(pkg: &str) -> &'static str {
    let p = pkg.to_lowercase();
    if p.contains("bank") || p.contains("finance") || p.contains("wallet")
        || p.contains("coinbase") || p.contains("binance") || p.contains("paypal")
        || p.contains("cash") || p.contains("venmo") || p.contains("revolut")
    {
        "financial"
    } else if p.contains("whatsapp") || p.contains("telegram") || p.contains("signal")
        || p.contains("messenger") || p.contains("sms") || p.contains("mms")
        || p.contains("viber") || p.contains("discord")
    {
        "messaging"
    } else if p.contains("mail") || p.contains("gmail") || p.ends_with(".gm")
        || p.contains("outlook") || p.contains("proton") || p.contains("yahoo")
    {
        "email"
    } else if p.contains("calendar") || p.contains("clock") || p.contains("alarm") {
        "schedule"
    } else if p.contains("security") || p.contains("auth") || p.contains("2fa")
        || p.contains("authenticator")
    {
        "security"
    } else if p.contains("news") || p.contains("twitter") || p.contains("reddit")
        || p.contains("instagram") || p.contains("tiktok") || p.contains("facebook")
    {
        "social"
    } else {
        "app"
    }
}

fn recovery_label(score: u64) -> &'static str {
    match score {
        0..=39  => "poor",
        40..=59 => "fair",
        60..=79 => "good",
        _       => "optimal",
    }
}

// ── PhoneDayBrief ──────────────────────────────────────────────────────────

/// Aggregate calendar + recovery + location + SMS + notifications into a single
/// structured daily brief.  No raw data is returned — only inferred context.
pub struct PhoneDayBriefTool {
    bridge_url: String,
    secret:     String,
    client:     reqwest::Client,
}

impl PhoneDayBriefTool {
    pub fn new(bridge_url: String, secret: String) -> Self {
        Self { bridge_url, secret, client: build_client() }
    }
}

#[async_trait]
impl Tool for PhoneDayBriefTool {
    fn name(&self) -> &str { "phone_day_brief" }

    fn description(&self) -> &str {
        "Aggregate today's full context into a structured brief: calendar load, health recovery, \
         location type, pending comms, financial obligations, and risk flags. Use this at the \
         start of any autonomous routine instead of calling individual phone tools separately. \
         Raw SMS content, health numbers, contact names, and GPS coordinates are never included."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "lookahead_days": {
                    "type": "integer",
                    "description": "How many days ahead to scan for calendar obligations (default 7)",
                    "default": 7
                }
            }
        })
    }

    async fn execute(&self, args: Value) -> anyhow::Result<ToolResult> {
        let lookahead = args["lookahead_days"].as_u64().unwrap_or(7).clamp(1, 30);
        let ts_now = now_ms();

        // Parallel fetch:
        // - /phone/recovery     → score (0-100), sleep quality, readiness label, insights
        // - /phone/sms          → inbox for bill/alert/reply analysis
        // - /phone/notifications → current active notifications
        // - /phone/calendar     → upcoming events for focus + travel detection
        // - /phone/context      → timezone + battery + network (aggregated, single call)
        // - /phone/carrier      → roaming flag
        let cal_path = format!("/phone/calendar?days={lookahead}");
        let (recovery, sms, notifs, cal, ctx, carrier) = join!(
            bridge_get(&self.client, &self.bridge_url, &self.secret, "/phone/recovery"),
            bridge_get(&self.client, &self.bridge_url, &self.secret, "/phone/sms?box=inbox&limit=50"),
            bridge_get(&self.client, &self.bridge_url, &self.secret, "/phone/notifications"),
            bridge_get(&self.client, &self.bridge_url, &self.secret, &cal_path),
            bridge_get(&self.client, &self.bridge_url, &self.secret, "/phone/context"),
            bridge_get(&self.client, &self.bridge_url, &self.secret, "/phone/carrier"),
        );

        // ── timezone (from /phone/context → timezone.offset_ms) ──────────────
        // Bridge returns offset_ms = raw UTC offset in milliseconds.
        let utc_offset_min = ctx.as_ref()
            .and_then(|v| v["timezone"]["offset_ms"].as_i64())
            .map(|ms| ms / 60_000)
            .unwrap_or(0);
        let tod = time_of_day(ts_now, utc_offset_min);

        // ── calendar analysis ─────────────────────────────────────────────────
        // Bridge returns data as JSONArray directly for /phone/calendar.
        let events = cal
            .as_ref()
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();

        let event_count_today = events.iter().filter(|e| {
            e["dtstart"].as_i64()
                .map(|t| t >= ts_now && t < ts_now + 86_400_000)
                .unwrap_or(false)
        }).count();

        let has_travel = events.iter().any(|e| {
            let title = e["title"].as_str().unwrap_or("").to_lowercase();
            title.contains("flight") || title.contains("airport") || title.contains("hotel")
                || title.contains("train") || title.contains("ferry")
        });

        // Next free block ≥ 1 hour today.
        let focus_window = {
            let today_end = ts_now + 86_400_000;
            let mut busy: Vec<(i64, i64)> = events.iter().filter_map(|e| {
                let s = e["dtstart"].as_i64()?;
                let en = e["dtend"].as_i64().unwrap_or(s + 3_600_000);
                if s >= ts_now && s < today_end { Some((s, en)) } else { None }
            }).collect();
            busy.sort_by_key(|s| s.0);

            let mut cursor = ts_now;
            let mut window: Option<String> = None;
            for (start, end) in &busy {
                if start - cursor >= 3_600_000 {
                    let h_s = cursor.rem_euclid(86_400_000) / 3_600_000;
                    let h_e = start.rem_euclid(86_400_000)  / 3_600_000;
                    window = Some(format!("{h_s:02}:00-{h_e:02}:00"));
                    break;
                }
                cursor = cursor.max(*end);
            }
            if window.is_none() && today_end - cursor >= 3_600_000 {
                let h_s = cursor.rem_euclid(86_400_000)    / 3_600_000;
                let h_e = today_end.rem_euclid(86_400_000) / 3_600_000;
                window = Some(format!("{h_s:02}:00-{h_e:02}:00"));
            }
            window.unwrap_or_else(|| "none".to_string())
        };

        // ── recovery analysis (from /phone/recovery) ──────────────────────────
        // Fields: score (0-100), readiness ("Optimal"/"Good"/"Fair"/"Poor"),
        //         last_sleep_min, deep_sleep_pct, rem_sleep_pct, insights (array)
        let has_recovery_data = recovery.as_ref().map(|v| !v.is_null()).unwrap_or(false);
        let recovery_score = recovery.as_ref().and_then(|v| v["score"].as_u64()).unwrap_or(0);
        let sleep_min      = recovery.as_ref().and_then(|v| v["last_sleep_min"].as_i64()).unwrap_or(0);
        let sleep_hours    = sleep_min as f64 / 60.0;
        let recovery_lbl   = recovery.as_ref()
            .and_then(|v| v["readiness"].as_str())
            .map(|s| s.to_lowercase())
            .unwrap_or_else(|| recovery_label(recovery_score).to_string());
        // Surface up to 2 recovery insights to LLM (no raw health numbers).
        let recovery_insights: Vec<&str> = recovery.as_ref()
            .and_then(|v| v["insights"].as_array())
            .map(|arr| arr.iter()
                .filter_map(|s| s.as_str())
                .take(2)
                .collect())
            .unwrap_or_default();

        // ── location / roaming ─────────────────────────────────────────────────
        let roaming = carrier.as_ref().and_then(|v| v["roaming"].as_bool()).unwrap_or(false);
        let location_type = if has_travel || roaming { "travel" } else { "local" };

        // ── SMS analysis (single pass) ─────────────────────────────────────────
        // Bridge returns data as JSONArray for /phone/sms.
        // Items: {id, address, body, date}
        let messages = sms.as_ref().and_then(|v| v.as_array()).cloned().unwrap_or_default();

        let mut bills: Vec<Value> = Vec::new();
        let mut replies_needed: u64 = 0;
        let mut financial_alerts: u64 = 0;

        for msg in &messages {
            let body = msg["body"].as_str().unwrap_or("");
            if is_otp(body) || is_spam(body) { continue; }

            let amounts = extract_amounts(body);
            if !amounts.is_empty() {
                financial_alerts += 1;
                let bl = body.to_lowercase();
                if bl.contains("due") || bl.contains("payment") || bl.contains("invoice")
                    || bl.contains("bill") || bl.contains("rent") || bl.contains("subscription")
                {
                    for (amount, due) in amounts {
                        bills.push(json!({ "amount": amount, "due": due }));
                    }
                }
            }
            if needs_reply(body) { replies_needed += 1; }
        }

        // Dedup bills: sort first so adjacent dedup is correct.
        bills.sort_by(|a, b| {
            a["amount"].as_str().unwrap_or("").cmp(b["amount"].as_str().unwrap_or(""))
        });
        bills.dedup_by(|a, b| a["amount"] == b["amount"]);

        // ── notification analysis ──────────────────────────────────────────────
        // Bridge returns data as JSONArray for /phone/notifications.
        // Items: {packageName, text, title, postTime, ...}  — field is `packageName`
        let notif_list = notifs.as_ref().and_then(|v| v.as_array()).cloned().unwrap_or_default();

        let mut financial_notifs: u64 = 0;
        let mut urgent_notifs:    u64 = 0;
        for n in &notif_list {
            let pkg  = n["packageName"].as_str().unwrap_or("");
            let text = n["text"].as_str().unwrap_or("");
            if notif_category(pkg) == "financial" { financial_notifs += 1; }
            if urgency(text) == "high"            { urgent_notifs    += 1; }
        }

        // ── day state ─────────────────────────────────────────────────────────
        // day_of_week: 0=Mon … 6=Sun (Unix epoch was Thursday + 4)
        let day_of_week = ((ts_now / 86_400_000 + 4) % 7) as u8;
        let is_weekend  = day_of_week >= 5;

        let day_state = if has_travel || location_type == "travel" {
            "travel"
        } else if is_weekend && event_count_today == 0 {
            "rest_day"
        } else if event_count_today >= 4 {
            "busy_day"
        } else {
            "work_day"
        };

        // ── risk flags ────────────────────────────────────────────────────────
        let mut risk_flags: Vec<&str> = Vec::new();
        if has_recovery_data && recovery_score < 40 {
            risk_flags.push("poor_recovery_limit_trading");
        }
        if has_recovery_data && sleep_hours > 0.0 && sleep_hours < 5.0 {
            risk_flags.push("sleep_debt_limit_decisions");
        }
        if day_state == "busy_day" {
            risk_flags.push("busy_day_defer_nonurgent");
        }
        if has_travel {
            risk_flags.push("travel_mode_conservative_portfolio");
        }
        if financial_notifs + financial_alerts >= 3 {
            risk_flags.push("multiple_financial_signals_review");
        }

        ok(json!({
            "day_state":           day_state,
            "time_of_day":         tod,
            "weekend":             is_weekend,
            "location_type":       location_type,
            "recovery":            recovery_lbl,
            "recovery_insights":   recovery_insights,
            "focus_window":        focus_window,
            "events_today":        event_count_today,
            "pending": {
                "bills":            bills,
                "replies_needed":   replies_needed,
                "financial_alerts": financial_alerts,
                "urgent_notifs":    urgent_notifs,
                "financial_notifs": financial_notifs,
            },
            "risk_flags": risk_flags,
        }))
    }
}

// ── PhoneSmsBrief ─────────────────────────────────────────────────────────

/// Read SMS inbox and return structured intelligence: bills, replies needed,
/// financial alerts, OTP count, spam count.  Message bodies are never included.
pub struct PhoneSmsBriefTool {
    bridge_url: String,
    secret:     String,
    client:     reqwest::Client,
}

impl PhoneSmsBriefTool {
    pub fn new(bridge_url: String, secret: String) -> Self {
        Self { bridge_url, secret, client: build_client() }
    }
}

#[async_trait]
impl Tool for PhoneSmsBriefTool {
    fn name(&self) -> &str { "phone_sms_brief" }

    fn description(&self) -> &str {
        "Read SMS inbox and return structured intelligence: bills/payments due, \
         messages needing a reply (with urgency), financial alerts, OTP count, and spam count. \
         Message bodies, phone numbers, and contact names are never returned. Use this instead \
         of phone_sms_read when you need to decide what to act on, not read raw content."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "limit": {
                    "type": "integer",
                    "description": "Number of inbox messages to analyse (default 50, max 200)",
                    "default": 50
                },
                "hours": {
                    "type": "integer",
                    "description": "Only look at messages received in the last N hours (0 = all, default 24)",
                    "default": 24
                }
            }
        })
    }

    async fn execute(&self, args: Value) -> anyhow::Result<ToolResult> {
        let limit = args["limit"].as_u64().unwrap_or(50).clamp(1, 200);
        let hours = args["hours"].as_u64().unwrap_or(24);
        let ts_now = now_ms();

        // Bridge returns: {"ok":true,"data":<JSONArray>}
        // Items: {id, address, body, date}
        let data = match self.client
            .get(format!("{}/phone/sms?box=inbox&limit={limit}", self.bridge_url))
            .header("X-Bridge-Token", &self.secret)
            .send()
            .await
        {
            Ok(r)  => r.json::<Value>().await.unwrap_or_default(),
            Err(e) => return err(format!("bridge request failed: {e}")),
        };

        if !data["ok"].as_bool().unwrap_or(false) {
            let reason = data["error"].as_str().unwrap_or("unknown").to_string();
            return err(format!("bridge error: {reason}"));
        }

        let messages  = data["data"].as_array().cloned().unwrap_or_default();
        let cutoff_ms = if hours == 0 { 0 } else { ts_now - (hours as i64 * 3_600_000) };

        let mut bills:            Vec<Value> = Vec::new();
        let mut replies:          Vec<Value> = Vec::new();
        let mut financial_alerts: Vec<Value> = Vec::new();
        let mut otp_count:        u64 = 0;
        let mut spam_count:       u64 = 0;
        let mut total_analysed:   u64 = 0;

        for msg in &messages {
            let ts_ms = msg["date"].as_i64().unwrap_or(0);
            if cutoff_ms > 0 && ts_ms < cutoff_ms { continue; }
            total_analysed += 1;

            let body = msg["body"].as_str().unwrap_or("");

            if is_spam(body) { spam_count += 1; continue; }
            if is_otp(body)  { otp_count  += 1; continue; }

            let amounts    = extract_amounts(body);
            let body_lower = body.to_lowercase();
            let is_bill = !amounts.is_empty() && (
                body_lower.contains("due") || body_lower.contains("payment")
                    || body_lower.contains("invoice") || body_lower.contains("bill")
                    || body_lower.contains("rent") || body_lower.contains("subscription")
                    || body_lower.contains("overdue")
            );
            let is_financial = !amounts.is_empty() && (
                body_lower.contains("transaction") || body_lower.contains("transfer")
                    || body_lower.contains("received") || body_lower.contains("sent")
                    || body_lower.contains("balance") || body_lower.contains("deposit")
                    || body_lower.contains("withdrawal") || body_lower.contains("alert")
            );

            if is_bill {
                for (amount, due) in &amounts {
                    bills.push(json!({ "amount": amount, "due": due }));
                }
            } else if is_financial {
                for (amount, _) in &amounts {
                    financial_alerts.push(json!({ "amount": amount, "type": "transaction" }));
                }
            }

            if needs_reply(body) {
                replies.push(json!({ "urgency": urgency(body), "has_bill": is_bill }));
            }
        }

        bills.sort_by(|a, b| {
            a["amount"].as_str().unwrap_or("").cmp(b["amount"].as_str().unwrap_or(""))
        });
        bills.dedup_by(|a, b| a["amount"] == b["amount"]);

        replies.sort_by_key(|r| match r["urgency"].as_str().unwrap_or("low") {
            "high"   => 0,
            "medium" => 1,
            _        => 2,
        });

        ok(json!({
            "analysed":          total_analysed,
            "bills":             bills,
            "replies_needed":    replies,
            "financial_alerts":  financial_alerts,
            "otp_codes_present": otp_count,
            "spam_filtered":     spam_count,
            "summary": format!(
                "{} messages: {} bill(s), {} repl(ies) needed, {} financial alert(s), {} OTP(s), {} spam",
                total_analysed, bills.len(), replies.len(),
                financial_alerts.len(), otp_count, spam_count
            ),
        }))
    }
}

// ── PhoneCommsSummary ─────────────────────────────────────────────────────

/// Aggregate SMS + notifications + call log into a single communications summary.
pub struct PhoneCommsSummaryTool {
    bridge_url: String,
    secret:     String,
    client:     reqwest::Client,
}

impl PhoneCommsSummaryTool {
    pub fn new(bridge_url: String, secret: String) -> Self {
        Self { bridge_url, secret, client: build_client() }
    }
}

#[async_trait]
impl Tool for PhoneCommsSummaryTool {
    fn name(&self) -> &str { "phone_comms_summary" }

    fn description(&self) -> &str {
        "Aggregate SMS inbox, active notifications, and call log into a communications summary \
         with action priorities. Returns bucketed counts by category and urgency — no raw \
         message content, phone numbers, or contact names exposed."
    }

    fn parameters_schema(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }

    async fn execute(&self, _args: Value) -> anyhow::Result<ToolResult> {
        let ts_now = now_ms();

        // Note: call log endpoint is /phone/call_log (singular), NOT /phone/calls/log.
        // Response data is a JSONArray; items: {id, number, type, date, duration}.
        // There is no `contact_name` field in call log items.
        let (sms_raw, notifs_raw, calls_raw) = join!(
            bridge_get(&self.client, &self.bridge_url, &self.secret, "/phone/sms?box=inbox&limit=30"),
            bridge_get(&self.client, &self.bridge_url, &self.secret, "/phone/notifications"),
            bridge_get(&self.client, &self.bridge_url, &self.secret, "/phone/call_log?limit=20"),
        );

        // ── SMS: single pass ──────────────────────────────────────────────────
        let messages = sms_raw.as_ref().and_then(|v| v.as_array()).cloned().unwrap_or_default();

        let mut sms_urgent  = 0usize;
        let mut sms_replies = 0usize;
        let mut sms_bills   = 0usize;

        for m in &messages {
            let body = m["body"].as_str().unwrap_or("");
            if is_spam(body) || is_otp(body) { continue; }
            if urgency(body) == "high" { sms_urgent += 1; }
            if needs_reply(body)       { sms_replies += 1; }
            let bl = body.to_lowercase();
            if !extract_amounts(body).is_empty()
                && (bl.contains("due") || bl.contains("payment")
                    || bl.contains("bill") || bl.contains("invoice"))
            {
                sms_bills += 1;
            }
        }

        // ── notification buckets ───────────────────────────────────────────────
        // Items have `packageName` (full package, e.g. "com.whatsapp"), `text`
        let notifs = notifs_raw.as_ref().and_then(|v| v.as_array()).cloned().unwrap_or_default();

        let mut notif_buckets: std::collections::HashMap<&str, u64> = std::collections::HashMap::new();
        let mut notif_urgent = 0u64;
        for n in &notifs {
            let pkg  = n["packageName"].as_str().unwrap_or("");
            let text = n["text"].as_str().unwrap_or("");
            *notif_buckets.entry(notif_category(pkg)).or_insert(0) += 1;
            if urgency(text) == "high" { notif_urgent += 1; }
        }

        // ── call log: single pass ─────────────────────────────────────────────
        // Items: {id, number, type ("incoming"/"outgoing"/"missed"), date, duration}
        // NOTE: no contact_name — only raw number available (we don't use it).
        let calls      = calls_raw.as_ref().and_then(|v| v.as_array()).cloned().unwrap_or_default();
        let cutoff_24h = ts_now - 86_400_000;
        let missed_24h: u64 = calls.iter()
            .filter(|c| {
                c["type"].as_str() == Some("missed")
                    && c["date"].as_i64().unwrap_or(0) > cutoff_24h
            })
            .count() as u64;

        // ── action priority ───────────────────────────────────────────────────
        let mut actions: Vec<&str> = Vec::new();
        if sms_urgent > 0   { actions.push("urgent_sms"); }
        if missed_24h > 0   { actions.push("missed_calls"); }
        if notif_urgent > 0 { actions.push("urgent_notification"); }
        if sms_bills > 0    { actions.push("bill_payment_due"); }
        if sms_replies > 0  { actions.push("sms_awaiting_reply"); }
        if *notif_buckets.get("financial").unwrap_or(&0) > 0 {
            actions.push("financial_notification");
        }

        ok(json!({
            "action_priority": actions,
            "sms": {
                "urgent":         sms_urgent,
                "replies_needed": sms_replies,
                "bills":          sms_bills,
            },
            "notifications": {
                "total":       notifs.len(),
                "urgent":      notif_urgent,
                "by_category": notif_buckets,
            },
            "calls": {
                "missed_last_24h": missed_24h,
            },
        }))
    }
}

// ── PhoneContextNow ───────────────────────────────────────────────────────

/// Snapshot of current device context: location type, connectivity, battery,
/// time-of-day, availability signal, and activity state.
/// No coordinates, no raw sensor values, no exact numbers.
pub struct PhoneContextNowTool {
    bridge_url: String,
    secret:     String,
    client:     reqwest::Client,
}

impl PhoneContextNowTool {
    pub fn new(bridge_url: String, secret: String) -> Self {
        Self { bridge_url, secret, client: build_client() }
    }
}

#[async_trait]
impl Tool for PhoneContextNowTool {
    fn name(&self) -> &str { "phone_context_now" }

    fn description(&self) -> &str {
        "Get the current device context as structured labels — location type, connectivity, \
         battery state, time of day, availability, and activity. Use this before any \
         time-sensitive or location-sensitive action. No raw GPS coordinates, no exact battery \
         percentage, no network details."
    }

    fn parameters_schema(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }

    async fn execute(&self, _args: Value) -> anyhow::Result<ToolResult> {
        let ts_now = now_ms();

        // /phone/context aggregates timezone + battery + network + calendar_today in one call.
        // /phone/carrier provides roaming.
        // /phone/audio/profile provides DND mode (field: dnd_mode, not dnd_active).
        // /phone/activity provides step count (field: steps_since_reboot).
        let (ctx, carrier, audio, activity) = join!(
            bridge_get(&self.client, &self.bridge_url, &self.secret, "/phone/context"),
            bridge_get(&self.client, &self.bridge_url, &self.secret, "/phone/carrier"),
            bridge_get(&self.client, &self.bridge_url, &self.secret, "/phone/audio/profile"),
            bridge_get(&self.client, &self.bridge_url, &self.secret, "/phone/activity"),
        );

        // ── battery (from context.battery) ────────────────────────────────────
        // Fields: percent (int), status ("charging"/"discharging"/"full"/"not_charging"), source
        let battery_pct    = ctx.as_ref().and_then(|v| v["battery"]["percent"].as_u64()).unwrap_or(100);
        let battery_status = ctx.as_ref().and_then(|v| v["battery"]["status"].as_str()).unwrap_or("unknown");
        let charging       = battery_status == "charging" || battery_status == "full";
        let battery_label  = if charging          { "charging" }
            else if battery_pct < 15              { "critical" }
            else if battery_pct < 30              { "low" }
            else if battery_pct < 60              { "medium" }
            else                                  { "good" };

        // ── connectivity (from context.network) ───────────────────────────────
        // Fields: connected (bool), type ("wifi"/"cellular"/"ethernet"/"none")
        let network_type = ctx.as_ref()
            .and_then(|v| v["network"]["type"].as_str())
            .unwrap_or("none");
        let connectivity = match network_type {
            "wifi"     => "wifi",
            "cellular" => "mobile_data",
            "ethernet" => "ethernet",
            _          => "offline",
        };

        // ── location / roaming (from carrier) ─────────────────────────────────
        // Fields: roaming (bool), call_state ("idle"/"ringing"/"offhook")
        let roaming      = carrier.as_ref().and_then(|v| v["roaming"].as_bool()).unwrap_or(false);
        let location_type = if roaming { "travel" } else { "local" };

        // ── time context (from context.timezone) ──────────────────────────────
        // Field: offset_ms (raw UTC offset in milliseconds, e.g. -18000000 for UTC-5)
        let utc_offset_min = ctx.as_ref()
            .and_then(|v| v["timezone"]["offset_ms"].as_i64())
            .map(|ms| ms / 60_000)
            .unwrap_or(0);
        let tod         = time_of_day(ts_now, utc_offset_min);
        let day_of_week = ((ts_now / 86_400_000 + 4) % 7) as u8;
        let is_weekend  = day_of_week >= 5;

        // ── DND / availability (from audio/profile) ───────────────────────────
        // Field: dnd_mode ("all"|"priority"|"none"|"alarms")  — "all" = DND off
        // ringer_mode ("normal"/"vibrate"/"silent")
        let dnd_mode   = audio.as_ref().and_then(|v| v["dnd_mode"].as_str()).unwrap_or("all");
        let ringer_mode = audio.as_ref().and_then(|v| v["ringer_mode"].as_str()).unwrap_or("normal");
        let dnd_active = dnd_mode != "all";

        // ── in_meeting (from context.calendar_today) ──────────────────────────
        // calendar_today is a JSONArray of events with dtstart/dtend.
        let in_meeting = ctx.as_ref()
            .and_then(|v| v["calendar_today"].as_array())
            .map(|evs| evs.iter().any(|e| {
                let start = e["dtstart"].as_i64().unwrap_or(i64::MAX);
                let end   = e["dtend"].as_i64().unwrap_or(0);
                ts_now >= start && ts_now <= end
            }))
            .unwrap_or(false);

        let availability = if in_meeting || dnd_active { "busy" } else { "available" };

        // ── activity state (from /phone/activity) ─────────────────────────────
        // Field: steps_since_reboot (long). We only label the motion state.
        let steps = activity.as_ref().and_then(|v| v["steps_since_reboot"].as_i64());
        // We can't determine walking vs standing from step count alone without delta tracking,
        // so just surface whether steps are available.
        let steps_available = steps.is_some();

        ok(json!({
            "time_of_day":     tod,
            "weekend":         is_weekend,
            "location_type":   location_type,
            "connectivity":    connectivity,
            "battery":         battery_label,
            "availability":    availability,
            "in_meeting":      in_meeting,
            "dnd_mode":        dnd_mode,
            "ringer_mode":     ringer_mode,
            "steps_available": steps_available,
        }))
    }
}

// ── tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    // ── bridge mock helpers ────────────────────────────────────────────────

    const SECRET: &str = "test-bridge-secret-1234567890ab";

    /// Wrap a payload in the PhoneBridgeServer.kt envelope.
    fn ok_env(data: Value) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(json!({ "ok": true, "data": data }))
    }

    fn err_env(msg: &str) -> ResponseTemplate {
        ResponseTemplate::new(400).set_body_json(json!({ "ok": false, "error": msg }))
    }

    fn auth() -> wiremock::matchers::HeaderExactMatcher {
        header("X-Bridge-Token", SECRET)
    }

    fn tool_url(server: &MockServer) -> String {
        server.uri()
    }

    // ── heuristic unit tests (no network) ─────────────────────────────────

    #[test]
    fn extract_amounts_finds_dollar_and_usdc() {
        let text = "Your rent of $1,250.00 is due by March 15. Also pay 50 USDC for subscription.";
        let amounts = extract_amounts(text);
        let strs: Vec<&str> = amounts.iter().map(|(a, _)| a.as_str()).collect();
        assert!(strs.contains(&"$1,250.00"), "should find $1,250.00");
        assert!(strs.iter().any(|s| s.contains("USDC")), "should find USDC amount");
    }

    #[test]
    fn extract_amounts_finds_due_date_in_window() {
        let text = "Invoice #4821 for $450.00 — pay by April 5th, 2026 to avoid late fees.";
        let amounts = extract_amounts(text);
        assert_eq!(amounts.len(), 1);
        let due = amounts[0].1.as_deref().unwrap_or("");
        assert!(due.contains("April") || due.contains("5"), "due hint should mention April 5: got {due:?}");
    }

    #[test]
    fn extract_amounts_deduplicates() {
        let text = "$100 is owed. Please pay $100 immediately.";
        let amounts = extract_amounts(text);
        assert_eq!(amounts.len(), 1, "duplicate amounts should be collapsed");
    }

    #[test]
    fn is_otp_detects_code_with_digits() {
        assert!(is_otp("Your verification code is 482910"));
        assert!(is_otp("OTP: 1234 — do not share"));
        assert!(is_otp("Use this one-time PIN: 88921 to confirm"));
    }

    #[test]
    fn is_otp_rejects_plain_message() {
        assert!(!is_otp("Hey, just checking in. Call me back?"));
        assert!(!is_otp("Your order #99821 has shipped."));
    }

    #[test]
    fn is_spam_catches_marketing() {
        assert!(is_spam("Congratulations! You have been selected for a free gift."));
        assert!(is_spam("CLICK HERE to claim your limited offer. Unsubscribe anytime."));
        assert!(is_spam("You are a lottery winner! Claim now."));
    }

    #[test]
    fn is_spam_passes_legit_message() {
        assert!(!is_spam("Hi, can we reschedule tomorrow's meeting to 3pm?"));
        assert!(!is_spam("Your rent payment of $1,400 is due Friday."));
    }

    #[test]
    fn needs_reply_detects_question_and_keywords() {
        assert!(needs_reply("Can you confirm receipt of the transfer?"));
        assert!(needs_reply("Please reply with your availability ASAP."));
        assert!(needs_reply("Did you get my last message?"));
    }

    #[test]
    fn urgency_classifies_correctly() {
        assert_eq!(urgency("FINAL NOTICE: overdue payment action required"), "high");
        assert_eq!(urgency("Reminder: your bill is due soon"), "medium");
        assert_eq!(urgency("Thanks for your recent purchase"), "low");
    }

    #[test]
    fn notif_category_classifies_packages() {
        assert_eq!(notif_category("com.whatsapp"), "messaging");
        assert_eq!(notif_category("com.revolut.revolut"), "financial");
        assert_eq!(notif_category("com.google.android.gm"), "email");
        assert_eq!(notif_category("com.google.android.calendar"), "schedule");
        assert_eq!(notif_category("com.authy.authy"), "security");
        assert_eq!(notif_category("com.reddit.frontpage"), "social");
        assert_eq!(notif_category("com.example.unknownapp"), "app");
    }

    #[test]
    fn time_of_day_negative_offset() {
        // UTC-5 (New York), 14:00 UTC → 09:00 local = morning
        let ts_14_utc = 14 * 3600 * 1000_i64;
        assert_eq!(time_of_day(ts_14_utc, -300), "morning");
    }

    #[test]
    fn time_of_day_positive_offset() {
        // UTC+8 (Shanghai), 02:00 UTC → 10:00 local = morning
        let ts_2_utc = 2 * 3600 * 1000_i64;
        assert_eq!(time_of_day(ts_2_utc, 480), "morning");
    }

    #[test]
    fn time_of_day_midnight_wraps() {
        // UTC+1, 23:30 UTC → 00:30 local = night
        let ts = 23 * 3600 * 1000_i64 + 30 * 60 * 1000;
        assert_eq!(time_of_day(ts, 60), "night");
    }

    // ── use case: morning routine — busy work day with a bill due ─────────

    /// Scenario: Tuesday morning, user has 5 calendar events (busy day), a rent
    /// bill in SMS, roaming off, recovery is good. Expect: day_state=busy_day,
    /// risk flag for busy day, bills list non-empty.
    #[tokio::test]
    async fn day_brief_busy_day_with_bill() {
        let server = MockServer::start().await;

        // Unix epoch for "Tuesday 09:00 UTC" — just use a fixed anchor.
        // We'll use ts=0 (Thu Jan 1 1970 00:00 UTC) scaled to a known day.
        // Simpler: trust the tool uses now_ms() — we just need events > now_ms().
        // For the busy-day check we need event_count_today >= 4.
        // Bridge returns calendar as JSONArray directly.
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;

        let events: Vec<Value> = (0..5).map(|i| json!({
            "id": i,
            "title": format!("Meeting {i}"),
            "description": "",
            "dtstart": now + i * 3_600_000_i64,         // spaced 1h apart, all today
            "dtend":   now + i * 3_600_000_i64 + 1_800_000_i64,
        })).collect();

        Mock::given(method("GET")).and(path("/phone/recovery")).and(auth())
            .respond_with(ok_env(json!({
                "score": 72, "readiness": "Good", "last_sleep_min": 420,
                "deep_sleep_pct": 18.0, "rem_sleep_pct": 22.0, "awake_min": 10,
                "insights": ["Good day for moderate activity"]
            })))
            .mount(&server).await;

        Mock::given(method("GET")).and(path("/phone/sms")).and(auth())
            .respond_with(ok_env(json!([
                { "id": "1", "address": "+1234567890", "date": now - 3_600_000,
                  "body": "Your rent of $1,400 is due by March 31. Please pay to avoid late fees." },
                { "id": "2", "address": "+1111111111", "date": now - 7_200_000,
                  "body": "Your verification code is 294810. Do not share." },
                { "id": "3", "address": "+9876543210", "date": now - 1_800_000,
                  "body": "Congratulations! You have been selected for a free gift. Click here." },
            ])))
            .mount(&server).await;

        Mock::given(method("GET")).and(path("/phone/notifications")).and(auth())
            .respond_with(ok_env(json!([
                { "key": "k1", "packageName": "com.revolut.revolut", "postTime": now,
                  "title": "Transaction", "text": "Payment received: $50" },
            ])))
            .mount(&server).await;

        Mock::given(method("GET")).and(path("/phone/calendar")).and(auth())
            .respond_with(ok_env(json!(events)))
            .mount(&server).await;

        Mock::given(method("GET")).and(path("/phone/context")).and(auth())
            .respond_with(ok_env(json!({
                "timezone":  { "offset_ms": 0, "id": "UTC" },
                "battery":   { "percent": 80, "status": "discharging", "source": "unplugged" },
                "network":   { "connected": true, "type": "wifi" },
                "calendar_today": events,
            })))
            .mount(&server).await;

        Mock::given(method("GET")).and(path("/phone/carrier")).and(auth())
            .respond_with(ok_env(json!({ "roaming": false, "call_state": "idle" })))
            .mount(&server).await;

        let tool = PhoneDayBriefTool::new(tool_url(&server), SECRET.to_string());
        let result = tool.execute(json!({})).await.unwrap();
        assert!(result.success, "tool should succeed: {:?}", result.error);

        let out: Value = serde_json::from_str(&result.output).unwrap();
        assert_eq!(out["day_state"], "busy_day",   "5 events → busy_day");
        assert_eq!(out["recovery"], "good",        "score 72 → good");
        assert!(out["pending"]["bills"].as_array().unwrap().len() >= 1,
            "rent bill should be detected");
        assert_eq!(out["pending"]["otp_filtered"], Value::Null, "OTP not exposed");
        let flags = out["risk_flags"].as_array().unwrap();
        assert!(flags.iter().any(|f| f == "busy_day_defer_nonurgent"),
            "busy day flag should be set");
    }

    // ── use case: rest day — weekend, no events, poor recovery ───────────

    #[tokio::test]
    async fn day_brief_rest_day_poor_recovery() {
        let server = MockServer::start().await;

        Mock::given(method("GET")).and(path("/phone/recovery")).and(auth())
            .respond_with(ok_env(json!({
                "score": 28, "readiness": "Poor", "last_sleep_min": 270,
                "deep_sleep_pct": 8.0, "rem_sleep_pct": 12.0, "awake_min": 45,
                "insights": [
                    "Short sleep (4h 30m) — aim for 7-9h",
                    "Low deep sleep (8%) — aim for 15-20%"
                ]
            })))
            .mount(&server).await;

        Mock::given(method("GET")).and(path("/phone/sms")).and(auth())
            .respond_with(ok_env(json!([])))
            .mount(&server).await;

        Mock::given(method("GET")).and(path("/phone/notifications")).and(auth())
            .respond_with(ok_env(json!([])))
            .mount(&server).await;

        // No events → rest_day (calendar returns empty array).
        Mock::given(method("GET")).and(path("/phone/calendar")).and(auth())
            .respond_with(ok_env(json!([])))
            .mount(&server).await;

        Mock::given(method("GET")).and(path("/phone/context")).and(auth())
            .respond_with(ok_env(json!({
                "timezone":  { "offset_ms": 0, "id": "UTC" },
                "battery":   { "percent": 45, "status": "discharging" },
                "network":   { "connected": true, "type": "cellular" },
                "calendar_today": [],
            })))
            .mount(&server).await;

        Mock::given(method("GET")).and(path("/phone/carrier")).and(auth())
            .respond_with(ok_env(json!({ "roaming": false, "call_state": "idle" })))
            .mount(&server).await;

        let tool = PhoneDayBriefTool::new(tool_url(&server), SECRET.to_string());
        let result = tool.execute(json!({})).await.unwrap();
        assert!(result.success);

        let out: Value = serde_json::from_str(&result.output).unwrap();
        let flags = out["risk_flags"].as_array().unwrap();
        assert!(flags.iter().any(|f| f == "poor_recovery_limit_trading"),
            "poor recovery flag expected: {flags:?}");
        assert!(flags.iter().any(|f| f == "sleep_debt_limit_decisions"),
            "sleep debt flag expected: {flags:?}");
        assert_eq!(out["recovery"], "poor");
        // Recovery insights surfaced but no raw numbers.
        let insights = out["recovery_insights"].as_array().unwrap();
        assert!(!insights.is_empty(), "insights should be passed through");
    }

    // ── use case: travel day ───────────────────────────────────────────────

    #[tokio::test]
    async fn day_brief_travel_mode_via_roaming() {
        let server = MockServer::start().await;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;

        Mock::given(method("GET")).and(path("/phone/recovery")).and(auth())
            .respond_with(ok_env(json!({
                "score": 65, "readiness": "Good", "last_sleep_min": 380,
                "deep_sleep_pct": 14.0, "rem_sleep_pct": 20.0, "awake_min": 15,
                "insights": []
            })))
            .mount(&server).await;
        Mock::given(method("GET")).and(path("/phone/sms")).and(auth())
            .respond_with(ok_env(json!([])))
            .mount(&server).await;
        Mock::given(method("GET")).and(path("/phone/notifications")).and(auth())
            .respond_with(ok_env(json!([])))
            .mount(&server).await;
        Mock::given(method("GET")).and(path("/phone/calendar")).and(auth())
            .respond_with(ok_env(json!([
                { "id": 1, "title": "Flight BKK→SIN", "description": "",
                  "dtstart": now + 3_600_000, "dtend": now + 7_200_000 }
            ])))
            .mount(&server).await;
        Mock::given(method("GET")).and(path("/phone/context")).and(auth())
            .respond_with(ok_env(json!({
                "timezone":  { "offset_ms": 25200000, "id": "Asia/Bangkok" },
                "battery":   { "percent": 62, "status": "discharging" },
                "network":   { "connected": true, "type": "cellular" },
                "calendar_today": [],
            })))
            .mount(&server).await;
        Mock::given(method("GET")).and(path("/phone/carrier")).and(auth())
            .respond_with(ok_env(json!({ "roaming": true, "call_state": "idle" })))
            .mount(&server).await;

        let tool = PhoneDayBriefTool::new(tool_url(&server), SECRET.to_string());
        let result = tool.execute(json!({})).await.unwrap();
        assert!(result.success);

        let out: Value = serde_json::from_str(&result.output).unwrap();
        assert_eq!(out["day_state"],    "travel");
        assert_eq!(out["location_type"], "travel");
        let flags = out["risk_flags"].as_array().unwrap();
        assert!(flags.iter().any(|f| f == "travel_mode_conservative_portfolio"),
            "travel flag expected");
    }

    // ── use case: SMS brief — inbox with bills, OTPs, spam, and replies ───

    #[tokio::test]
    async fn sms_brief_mixed_inbox() {
        let server = MockServer::start().await;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;

        Mock::given(method("GET")).and(path("/phone/sms")).and(auth())
            .respond_with(ok_env(json!([
                // Bill
                { "id": "1", "address": "BILLING", "date": now - 3_600_000,
                  "body": "Your invoice #INV-0041 for $299.00 is due by April 10. Please pay to avoid service interruption." },
                // OTP — must be filtered
                { "id": "2", "address": "SECURITY", "date": now - 60_000,
                  "body": "Your one-time code is 857204. Never share this." },
                // Spam — must be filtered
                { "id": "3", "address": "PROMO", "date": now - 120_000,
                  "body": "Congratulations! You have been selected. Click here to claim your free gift." },
                // Financial transaction
                { "id": "4", "address": "BANK", "date": now - 300_000,
                  "body": "Transaction alert: $1,200.00 withdrawal from your account on 2026-03-24." },
                // Needs reply
                { "id": "5", "address": "+447911123456", "date": now - 600_000,
                  "body": "Hey, can you confirm if you received the payment? Please respond ASAP." },
                // Old message outside 24h window — must be excluded
                { "id": "6", "address": "+1987654321", "date": now - 90_000_000,
                  "body": "Old message with $500 due. Should be excluded by hours filter." },
            ])))
            .mount(&server).await;

        let tool = PhoneSmsBriefTool::new(tool_url(&server), SECRET.to_string());
        let result = tool.execute(json!({ "hours": 24 })).await.unwrap();
        assert!(result.success, "{:?}", result.error);

        let out: Value = serde_json::from_str(&result.output).unwrap();

        // OTP and spam are counted but not included in bills/replies.
        assert_eq!(out["otp_codes_present"], 1, "1 OTP expected");
        assert_eq!(out["spam_filtered"], 1, "1 spam expected");

        // Invoice detected as bill.
        let bills = out["bills"].as_array().unwrap();
        assert!(!bills.is_empty(), "invoice should be in bills");
        let amounts: Vec<&str> = bills.iter()
            .filter_map(|b| b["amount"].as_str())
            .collect();
        assert!(amounts.iter().any(|a| a.contains("299")), "should contain $299");

        // Financial transaction alert.
        let alerts = out["financial_alerts"].as_array().unwrap();
        assert!(!alerts.is_empty(), "bank withdrawal should be a financial alert");

        // Reply needed.
        let replies = out["replies_needed"].as_array().unwrap();
        assert!(!replies.is_empty(), "message asking to confirm should need reply");
        assert_eq!(replies[0]["urgency"], "high", "ASAP → high urgency");

        // Old message excluded.
        let analysed = out["analysed"].as_u64().unwrap();
        assert_eq!(analysed, 5, "old message outside window should be excluded");
    }

    // ── use case: SMS brief — completely clean inbox ───────────────────────

    #[tokio::test]
    async fn sms_brief_empty_inbox() {
        let server = MockServer::start().await;
        Mock::given(method("GET")).and(path("/phone/sms")).and(auth())
            .respond_with(ok_env(json!([])))
            .mount(&server).await;

        let tool = PhoneSmsBriefTool::new(tool_url(&server), SECRET.to_string());
        let result = tool.execute(json!({})).await.unwrap();
        assert!(result.success);
        let out: Value = serde_json::from_str(&result.output).unwrap();
        assert_eq!(out["analysed"], 0);
        assert!(out["bills"].as_array().unwrap().is_empty());
        assert!(out["replies_needed"].as_array().unwrap().is_empty());
    }

    // ── use case: comms summary — missed calls + urgent SMS + financial notif

    #[tokio::test]
    async fn comms_summary_missed_calls_and_urgent_sms() {
        let server = MockServer::start().await;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;

        Mock::given(method("GET")).and(path("/phone/sms")).and(auth())
            .respond_with(ok_env(json!([
                { "id": "1", "address": "+1234567890", "date": now - 3_600_000,
                  "body": "URGENT: your account is overdue. Action required immediately." },
                { "id": "2", "address": "BANK", "date": now - 1_800_000,
                  "body": "Your bill of $520 is due today. Pay now." },
            ])))
            .mount(&server).await;

        Mock::given(method("GET")).and(path("/phone/notifications")).and(auth())
            .respond_with(ok_env(json!([
                { "key": "k1", "packageName": "com.coinbase.android", "postTime": now,
                  "title": "Balance update", "text": "Your balance changed" },
                { "key": "k2", "packageName": "com.whatsapp", "postTime": now,
                  "title": "New message", "text": "How are you doing?" },
            ])))
            .mount(&server).await;

        // 3 missed calls in last 24h, 1 old one outside window.
        Mock::given(method("GET")).and(path("/phone/call_log")).and(auth())
            .respond_with(ok_env(json!([
                { "id": "c1", "number": "+111", "type": "missed", "date": now - 3_600_000,  "duration": 0 },
                { "id": "c2", "number": "+222", "type": "missed", "date": now - 7_200_000,  "duration": 0 },
                { "id": "c3", "number": "+333", "type": "missed", "date": now - 10_800_000, "duration": 0 },
                { "id": "c4", "number": "+444", "type": "missed", "date": now - 90_000_000, "duration": 0 }, // > 24h
                { "id": "c5", "number": "+555", "type": "incoming","date": now - 1_800_000, "duration": 120 },
            ])))
            .mount(&server).await;

        let tool = PhoneCommsSummaryTool::new(tool_url(&server), SECRET.to_string());
        let result = tool.execute(json!({})).await.unwrap();
        assert!(result.success, "{:?}", result.error);

        let out: Value = serde_json::from_str(&result.output).unwrap();

        // Action priority should list urgent signals first.
        let prio = out["action_priority"].as_array().unwrap();
        assert!(prio.iter().any(|p| p == "urgent_sms"), "urgent SMS expected in priority");
        assert!(prio.iter().any(|p| p == "missed_calls"), "missed calls expected");
        assert!(prio.iter().any(|p| p == "financial_notification"), "financial notif expected");

        assert_eq!(out["calls"]["missed_last_24h"], 3, "3 missed calls in window");
        assert!(out["sms"]["urgent"].as_u64().unwrap() >= 1, "urgent SMS count");
        assert!(out["sms"]["bills"].as_u64().unwrap() >= 1, "bill SMS count");

        let by_cat = &out["notifications"]["by_category"];
        assert_eq!(by_cat["financial"], 1, "1 financial notif");
        assert_eq!(by_cat["messaging"], 1, "1 messaging notif");
    }

    // ── use case: comms summary — bridge error on call log (graceful) ──────

    #[tokio::test]
    async fn comms_summary_call_log_error_still_returns_sms() {
        let server = MockServer::start().await;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;

        Mock::given(method("GET")).and(path("/phone/sms")).and(auth())
            .respond_with(ok_env(json!([
                { "id": "1", "address": "+1", "date": now, "body": "Please confirm receipt." },
            ])))
            .mount(&server).await;
        Mock::given(method("GET")).and(path("/phone/notifications")).and(auth())
            .respond_with(ok_env(json!([])))
            .mount(&server).await;
        // Call log returns bridge error (e.g. permission denied).
        Mock::given(method("GET")).and(path("/phone/call_log")).and(auth())
            .respond_with(err_env("permission denied"))
            .mount(&server).await;

        let tool = PhoneCommsSummaryTool::new(tool_url(&server), SECRET.to_string());
        let result = tool.execute(json!({})).await.unwrap();
        assert!(result.success, "should succeed even when call log fails");

        let out: Value = serde_json::from_str(&result.output).unwrap();
        // SMS still works.
        assert!(out["sms"]["replies_needed"].as_u64().unwrap() >= 1);
        // Calls section defaults to zero gracefully.
        assert_eq!(out["calls"]["missed_last_24h"], 0);
    }

    // ── use case: context now — charging, on WiFi, in meeting, DND on ─────

    #[tokio::test]
    async fn context_now_in_meeting_dnd_on() {
        let server = MockServer::start().await;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;

        // calendar_today has an event that wraps now → in_meeting = true
        let ongoing_event = json!({
            "id": 1, "title": "Investor Call", "description": "",
            "dtstart": now - 900_000,   // started 15 min ago
            "dtend":   now + 2_700_000, // ends in 45 min
        });

        Mock::given(method("GET")).and(path("/phone/context")).and(auth())
            .respond_with(ok_env(json!({
                "timezone":      { "offset_ms": 3600000, "id": "Europe/Paris" },
                "battery":       { "percent": 95, "status": "charging", "source": "usb" },
                "network":       { "connected": true, "type": "wifi" },
                "calendar_today": [ongoing_event],
            })))
            .mount(&server).await;

        Mock::given(method("GET")).and(path("/phone/carrier")).and(auth())
            .respond_with(ok_env(json!({ "roaming": false, "call_state": "idle" })))
            .mount(&server).await;

        // DND active: mode = "none" (all notifications suppressed)
        Mock::given(method("GET")).and(path("/phone/audio/profile")).and(auth())
            .respond_with(ok_env(json!({
                "dnd_mode":    "none",
                "ringer_mode": "silent",
                "streams":     {}
            })))
            .mount(&server).await;

        Mock::given(method("GET")).and(path("/phone/activity")).and(auth())
            .respond_with(ok_env(json!({ "steps_since_reboot": 3200 })))
            .mount(&server).await;

        let tool = PhoneContextNowTool::new(tool_url(&server), SECRET.to_string());
        let result = tool.execute(json!({})).await.unwrap();
        assert!(result.success, "{:?}", result.error);

        let out: Value = serde_json::from_str(&result.output).unwrap();
        assert_eq!(out["battery"],       "charging");
        assert_eq!(out["connectivity"],  "wifi");
        assert_eq!(out["in_meeting"],    true);
        assert_eq!(out["dnd_mode"],      "none");
        assert_eq!(out["availability"],  "busy");
        assert_eq!(out["location_type"], "local");
        assert_eq!(out["steps_available"], true);
    }

    // ── use case: context now — offline, critical battery, roaming ────────

    #[tokio::test]
    async fn context_now_critical_battery_roaming_offline() {
        let server = MockServer::start().await;

        Mock::given(method("GET")).and(path("/phone/context")).and(auth())
            .respond_with(ok_env(json!({
                "timezone":      { "offset_ms": 28800000, "id": "Asia/Shanghai" },
                "battery":       { "percent": 8, "status": "discharging" },
                "network":       { "connected": false, "type": "none" },
                "calendar_today": [],
            })))
            .mount(&server).await;

        Mock::given(method("GET")).and(path("/phone/carrier")).and(auth())
            .respond_with(ok_env(json!({ "roaming": true, "call_state": "idle" })))
            .mount(&server).await;

        Mock::given(method("GET")).and(path("/phone/audio/profile")).and(auth())
            .respond_with(ok_env(json!({
                "dnd_mode": "all", "ringer_mode": "normal", "streams": {}
            })))
            .mount(&server).await;

        // Activity endpoint unavailable (bridge error)
        Mock::given(method("GET")).and(path("/phone/activity")).and(auth())
            .respond_with(err_env("sensor timeout"))
            .mount(&server).await;

        let tool = PhoneContextNowTool::new(tool_url(&server), SECRET.to_string());
        let result = tool.execute(json!({})).await.unwrap();
        assert!(result.success);

        let out: Value = serde_json::from_str(&result.output).unwrap();
        assert_eq!(out["battery"],        "critical");
        assert_eq!(out["connectivity"],   "offline");
        assert_eq!(out["location_type"],  "travel");
        assert_eq!(out["dnd_mode"],       "all");          // DND off
        assert_eq!(out["availability"],   "available");
        assert_eq!(out["steps_available"], false);         // sensor error → false
    }

    // ── use case: bridge completely unreachable ────────────────────────────

    #[tokio::test]
    async fn sms_brief_bridge_unreachable_returns_error() {
        // No mocks registered → connection refused.
        let tool = PhoneSmsBriefTool::new(
            "http://127.0.0.1:19991".to_string(), // nothing listening here
            SECRET.to_string(),
        );
        let result = tool.execute(json!({})).await.unwrap();
        assert!(!result.success, "should report failure when bridge is unreachable");
        assert!(result.error.is_some());
    }

    // ── use case: bridge returns ok=false (permission denied) ─────────────

    #[tokio::test]
    async fn sms_brief_permission_denied_returns_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET")).and(path("/phone/sms")).and(auth())
            .respond_with(err_env("READ_SMS permission not granted"))
            .mount(&server).await;

        let tool = PhoneSmsBriefTool::new(tool_url(&server), SECRET.to_string());
        let result = tool.execute(json!({})).await.unwrap();
        assert!(!result.success);
        assert!(result.error.as_deref().unwrap_or("").contains("bridge error"));
    }

    // ── use case: multiple financial signals → risk flag ─────────────────

    #[tokio::test]
    async fn day_brief_multiple_financial_signals_flag() {
        let server = MockServer::start().await;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;

        Mock::given(method("GET")).and(path("/phone/recovery")).and(auth())
            .respond_with(ok_env(json!({
                "score": 80, "readiness": "Good", "last_sleep_min": 450,
                "deep_sleep_pct": 17.0, "rem_sleep_pct": 21.0, "awake_min": 5,
                "insights": []
            })))
            .mount(&server).await;

        // 4 distinct financial SMS → financial_alerts >= 3
        Mock::given(method("GET")).and(path("/phone/sms")).and(auth())
            .respond_with(ok_env(json!([
                { "id": "1", "address": "BANK", "date": now - 3_600_000,
                  "body": "Transfer of $200 from your account." },
                { "id": "2", "address": "BANK", "date": now - 7_200_000,
                  "body": "Deposit of $1,500 received." },
                { "id": "3", "address": "CARD",  "date": now - 10_800_000,
                  "body": "Spending alert: $89.99 charged to your card." },
            ])))
            .mount(&server).await;

        // 2 financial notifications → total financial_notifs = 2; combined >= 3 triggers flag
        Mock::given(method("GET")).and(path("/phone/notifications")).and(auth())
            .respond_with(ok_env(json!([
                { "key": "n1", "packageName": "com.paypal.android.p2pmobile",
                  "postTime": now, "title": "Payment", "text": "You received $25" },
            ])))
            .mount(&server).await;

        Mock::given(method("GET")).and(path("/phone/calendar")).and(auth())
            .respond_with(ok_env(json!([])))
            .mount(&server).await;

        Mock::given(method("GET")).and(path("/phone/context")).and(auth())
            .respond_with(ok_env(json!({
                "timezone": { "offset_ms": 0, "id": "UTC" },
                "battery":  { "percent": 70, "status": "discharging" },
                "network":  { "connected": true, "type": "wifi" },
                "calendar_today": [],
            })))
            .mount(&server).await;

        Mock::given(method("GET")).and(path("/phone/carrier")).and(auth())
            .respond_with(ok_env(json!({ "roaming": false, "call_state": "idle" })))
            .mount(&server).await;

        let tool = PhoneDayBriefTool::new(tool_url(&server), SECRET.to_string());
        let result = tool.execute(json!({})).await.unwrap();
        assert!(result.success);

        let out: Value = serde_json::from_str(&result.output).unwrap();
        let flags = out["risk_flags"].as_array().unwrap();
        assert!(
            flags.iter().any(|f| f == "multiple_financial_signals_review"),
            "3+ financial signals should trigger review flag: {flags:?}"
        );
    }
}
