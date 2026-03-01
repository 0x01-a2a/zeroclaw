//! Phone bridge tools — call Android phone APIs via the local HTTP bridge
//! server running on the mobile device (default 127.0.0.1:9092).
//!
//! Every request carries `X-Bridge-Token: <secret>` so only ZeroClaw
//! (which received the secret via the TOML config written by NodeService)
//! can reach the bridge endpoints.

use crate::tools::traits::{Tool, ToolResult};
use async_trait::async_trait;
use serde_json::Value;
use std::time::Duration;

// ── shared helper ─────────────────────────────────────────────────────────────

fn client(timeout_secs: u64) -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(timeout_secs))
        .build()
        .expect("reqwest client build failed")
}

fn ok_result(text: String) -> anyhow::Result<ToolResult> {
    Ok(ToolResult { success: true, output: text, error: None })
}

fn err_result(msg: impl Into<String>) -> anyhow::Result<ToolResult> {
    let m = msg.into();
    Ok(ToolResult { success: false, output: String::new(), error: Some(m) })
}

// Macro to reduce per-struct boilerplate.
macro_rules! phone_tool {
    (
        $struct:ident,
        name = $name:literal,
        desc = $desc:literal,
        schema = $schema:expr,
        exec = |$self:ident, $args:ident| $body:expr
    ) => {
        pub struct $struct {
            bridge_url:   String,
            secret:       String,
            timeout_secs: u64,
        }

        impl $struct {
            pub fn new(bridge_url: String, secret: String, timeout_secs: u64) -> Self {
                Self { bridge_url, secret, timeout_secs }
            }

            fn get(&$self, path: &str) -> reqwest::RequestBuilder {
                client($self.timeout_secs)
                    .get(format!("{}{}", $self.bridge_url, path))
                    .header("X-Bridge-Token", &$self.secret)
            }

            fn post(&$self, path: &str) -> reqwest::RequestBuilder {
                client($self.timeout_secs)
                    .post(format!("{}{}", $self.bridge_url, path))
                    .header("X-Bridge-Token", &$self.secret)
            }
        }

        #[async_trait]
        impl Tool for $struct {
            fn name(&self) -> &str { $name }
            fn description(&self) -> &str { $desc }
            fn parameters_schema(&self) -> Value { $schema }
            async fn execute(&$self, $args: Value) -> anyhow::Result<ToolResult> { $body }
        }
    };
}

// ── PhoneContactsRead ─────────────────────────────────────────────────────────

phone_tool!(
    PhoneContactsRead,
    name = "phone_contacts_read",
    desc = "Read contacts from the device address book. Pass an optional query string to search by name.",
    schema = serde_json::json!({
        "type": "object",
        "properties": {
            "query": { "type": "string", "description": "Optional name search filter" }
        }
    }),
    exec = |self, args| {
        let query = args["query"].as_str().unwrap_or("");
        let path  = format!("/phone/contacts?query={}", urlencoding::encode(query));
        match self.get(&path).send().await {
            Ok(r)  => ok_result(r.text().await.unwrap_or_default()),
            Err(e) => err_result(format!("bridge request failed: {e}")),
        }
    }
);

// ── PhoneContactsWrite ────────────────────────────────────────────────────────

phone_tool!(
    PhoneContactsWrite,
    name = "phone_contacts_write",
    desc = "Add a new contact to the device address book.",
    schema = serde_json::json!({
        "type": "object",
        "required": ["name", "phone"],
        "properties": {
            "name":  { "type": "string", "description": "Contact display name" },
            "phone": { "type": "string", "description": "Phone number" }
        }
    }),
    exec = |self, args| {
        match self.post("/phone/contacts").json(&args).send().await {
            Ok(r)  => ok_result(r.text().await.unwrap_or_default()),
            Err(e) => err_result(format!("bridge request failed: {e}")),
        }
    }
);

// ── PhoneSmsRead ──────────────────────────────────────────────────────────────

phone_tool!(
    PhoneSmsRead,
    name = "phone_sms_read",
    desc = "Read SMS messages from the device. box can be 'inbox', 'sent', or 'draft'. limit defaults to 20.",
    schema = serde_json::json!({
        "type": "object",
        "properties": {
            "box":   { "type": "string",  "description": "inbox | sent | draft", "default": "inbox" },
            "limit": { "type": "integer", "description": "Max messages to return", "default": 20 }
        }
    }),
    exec = |self, args| {
        let box_  = args["box"].as_str().unwrap_or("inbox");
        let limit = args["limit"].as_u64().unwrap_or(20);
        let path  = format!("/phone/sms?box={box_}&limit={limit}");
        match self.get(&path).send().await {
            Ok(r)  => ok_result(r.text().await.unwrap_or_default()),
            Err(e) => err_result(format!("bridge request failed: {e}")),
        }
    }
);

// ── PhoneSmsSend ──────────────────────────────────────────────────────────────

phone_tool!(
    PhoneSmsSend,
    name = "phone_sms_send",
    desc = "Send an SMS message from the device.",
    schema = serde_json::json!({
        "type": "object",
        "required": ["to", "body"],
        "properties": {
            "to":   { "type": "string", "description": "Recipient phone number" },
            "body": { "type": "string", "description": "Message text (max 1600 chars)" }
        }
    }),
    exec = |self, args| {
        match self.post("/phone/sms/send").json(&args).send().await {
            Ok(r)  => ok_result(r.text().await.unwrap_or_default()),
            Err(e) => err_result(format!("bridge request failed: {e}")),
        }
    }
);

// ── PhoneLocation ─────────────────────────────────────────────────────────────

phone_tool!(
    PhoneLocation,
    name = "phone_location",
    desc = "Get the device's current GPS location (latitude, longitude, accuracy, age_ms, stale flag).",
    schema = serde_json::json!({ "type": "object", "properties": {} }),
    exec = |self, _args| {
        match self.get("/phone/location").send().await {
            Ok(r)  => ok_result(r.text().await.unwrap_or_default()),
            Err(e) => err_result(format!("bridge request failed: {e}")),
        }
    }
);

// ── PhoneCalendarRead ─────────────────────────────────────────────────────────

phone_tool!(
    PhoneCalendarRead,
    name = "phone_calendar_read",
    desc = "Read upcoming calendar events. days defaults to 7.",
    schema = serde_json::json!({
        "type": "object",
        "properties": {
            "days": { "type": "integer", "description": "Number of days ahead to query", "default": 7 }
        }
    }),
    exec = |self, args| {
        let days = args["days"].as_u64().unwrap_or(7);
        let path = format!("/phone/calendar?days={days}");
        match self.get(&path).send().await {
            Ok(r)  => ok_result(r.text().await.unwrap_or_default()),
            Err(e) => err_result(format!("bridge request failed: {e}")),
        }
    }
);

// ── PhoneCalendarWrite ────────────────────────────────────────────────────────

phone_tool!(
    PhoneCalendarWrite,
    name = "phone_calendar_write",
    desc = "Create a new calendar event. dtstart and dtend are Unix epoch milliseconds.",
    schema = serde_json::json!({
        "type": "object",
        "required": ["title", "dtstart"],
        "properties": {
            "title":       { "type": "string",  "description": "Event title" },
            "description": { "type": "string",  "description": "Event description" },
            "dtstart":     { "type": "integer", "description": "Start time (ms since epoch)" },
            "dtend":       { "type": "integer", "description": "End time (ms since epoch)" }
        }
    }),
    exec = |self, args| {
        match self.post("/phone/calendar").json(&args).send().await {
            Ok(r)  => ok_result(r.text().await.unwrap_or_default()),
            Err(e) => err_result(format!("bridge request failed: {e}")),
        }
    }
);

// ── PhoneNotify ───────────────────────────────────────────────────────────────

phone_tool!(
    PhoneNotify,
    name = "phone_notify",
    desc = "Send a local push notification to the device.",
    schema = serde_json::json!({
        "type": "object",
        "required": ["message"],
        "properties": {
            "title":   { "type": "string", "description": "Notification title" },
            "message": { "type": "string", "description": "Notification body text" }
        }
    }),
    exec = |self, args| {
        match self.post("/phone/notify").json(&args).send().await {
            Ok(r)  => ok_result(r.text().await.unwrap_or_default()),
            Err(e) => err_result(format!("bridge request failed: {e}")),
        }
    }
);

// ── PhoneCallLog ──────────────────────────────────────────────────────────────

phone_tool!(
    PhoneCallLog,
    name = "phone_call_log",
    desc = "Read recent call log entries (incoming, outgoing, missed). limit defaults to 20.",
    schema = serde_json::json!({
        "type": "object",
        "properties": {
            "limit": { "type": "integer", "description": "Max entries to return", "default": 20 }
        }
    }),
    exec = |self, args| {
        let limit = args["limit"].as_u64().unwrap_or(20);
        let path  = format!("/phone/call_log?limit={limit}");
        match self.get(&path).send().await {
            Ok(r)  => ok_result(r.text().await.unwrap_or_default()),
            Err(e) => err_result(format!("bridge request failed: {e}")),
        }
    }
);

// ── PhoneClipboardRead ────────────────────────────────────────────────────────

phone_tool!(
    PhoneClipboardRead,
    name = "phone_clipboard_read",
    desc = "Read the current clipboard text content from the device.",
    schema = serde_json::json!({ "type": "object", "properties": {} }),
    exec = |self, _args| {
        match self.get("/phone/clipboard").send().await {
            Ok(r)  => ok_result(r.text().await.unwrap_or_default()),
            Err(e) => err_result(format!("bridge request failed: {e}")),
        }
    }
);

// ── PhoneClipboardWrite ───────────────────────────────────────────────────────

phone_tool!(
    PhoneClipboardWrite,
    name = "phone_clipboard_write",
    desc = "Write text to the device clipboard.",
    schema = serde_json::json!({
        "type": "object",
        "required": ["text"],
        "properties": {
            "text": { "type": "string", "description": "Text to copy to clipboard" }
        }
    }),
    exec = |self, args| {
        match self.post("/phone/clipboard").json(&args).send().await {
            Ok(r)  => ok_result(r.text().await.unwrap_or_default()),
            Err(e) => err_result(format!("bridge request failed: {e}")),
        }
    }
);

// ── PhoneCameraCapture ────────────────────────────────────────────────────────

phone_tool!(
    PhoneCameraCapture,
    name = "phone_camera_capture",
    desc = "Capture a photo using the device camera. Returns camera_id and availability info.",
    schema = serde_json::json!({ "type": "object", "properties": {} }),
    exec = |self, _args| {
        match self.post("/phone/camera/capture").send().await {
            Ok(r)  => ok_result(r.text().await.unwrap_or_default()),
            Err(e) => err_result(format!("bridge request failed: {e}")),
        }
    }
);

// ── PhoneAudioRecord ──────────────────────────────────────────────────────────

phone_tool!(
    PhoneAudioRecord,
    name = "phone_audio_record",
    desc = "Record audio from the device microphone. duration_ms controls length (500–30000 ms). Returns base64-encoded AAC audio.",
    schema = serde_json::json!({
        "type": "object",
        "properties": {
            "duration_ms": {
                "type": "integer",
                "description": "Recording duration in milliseconds (500–30000)",
                "default": 3000
            }
        }
    }),
    exec = |self, args| {
        let duration_ms = args["duration_ms"].as_u64().unwrap_or(3_000);
        // Allow extra time beyond bridge timeout to cover the recording duration.
        let extra_secs  = (duration_ms / 1_000) + self.timeout_secs;
        let req = client(extra_secs)
            .post(format!("{}/phone/audio/record", self.bridge_url))
            .header("X-Bridge-Token", &self.secret)
            .json(&args);
        match req.send().await {
            Ok(r)  => ok_result(r.text().await.unwrap_or_default()),
            Err(e) => err_result(format!("bridge request failed: {e}")),
        }
    }
);
