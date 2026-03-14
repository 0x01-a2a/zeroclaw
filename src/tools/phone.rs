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
use tokio::join;

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

// ── PhoneNotificationsGet ─────────────────────────────────────────────────────

phone_tool!(
    PhoneNotificationsGet,
    name = "phone_notifications_get",
    desc = "List active (currently visible) notifications on the device. Requires the Notification \
            Listener permission granted in Android Settings. Returns app, title, text, and a \
            key that can be used to reply or dismiss.",
    schema = serde_json::json!({ "type": "object", "properties": {} }),
    exec = |self, _args| {
        match self.get("/phone/notifications").send().await {
            Ok(r)  => ok_result(r.text().await.unwrap_or_default()),
            Err(e) => err_result(format!("bridge request failed: {e}")),
        }
    }
);

// ── PhoneNotificationsReply ───────────────────────────────────────────────────

phone_tool!(
    PhoneNotificationsReply,
    name = "phone_notifications_reply",
    desc = "Send a reply to a notification (e.g. reply to a messaging app notification). \
            Use the `key` returned by phone_notifications_get. Full and dappstore builds only.",
    schema = serde_json::json!({
        "type": "object",
        "required": ["key", "reply"],
        "properties": {
            "key":   { "type": "string", "description": "Notification key from phone_notifications_get" },
            "reply": { "type": "string", "description": "Reply text to send" }
        }
    }),
    exec = |self, args| {
        match self.post("/phone/notifications/reply").json(&args).send().await {
            Ok(r)  => ok_result(r.text().await.unwrap_or_default()),
            Err(e) => err_result(format!("bridge request failed: {e}")),
        }
    }
);

// ── PhoneNotificationsDismiss ─────────────────────────────────────────────────

phone_tool!(
    PhoneNotificationsDismiss,
    name = "phone_notifications_dismiss",
    desc = "Dismiss (clear) a specific notification by its key. \
            Use the `key` returned by phone_notifications_get.",
    schema = serde_json::json!({
        "type": "object",
        "required": ["key"],
        "properties": {
            "key": { "type": "string", "description": "Notification key from phone_notifications_get" }
        }
    }),
    exec = |self, args| {
        match self.post("/phone/notifications/dismiss").json(&args).send().await {
            Ok(r)  => ok_result(r.text().await.unwrap_or_default()),
            Err(e) => err_result(format!("bridge request failed: {e}")),
        }
    }
);

// ── PhoneCallsPending ─────────────────────────────────────────────────────────

phone_tool!(
    PhoneCallsPending,
    name = "phone_calls_pending",
    desc = "Check for pending (ringing or in-progress) incoming calls. \
            Returns caller info. Full build only (requires Call Screening permission).",
    schema = serde_json::json!({ "type": "object", "properties": {} }),
    exec = |self, _args| {
        match self.get("/phone/calls/pending").send().await {
            Ok(r)  => ok_result(r.text().await.unwrap_or_default()),
            Err(e) => err_result(format!("bridge request failed: {e}")),
        }
    }
);

// ── PhoneCallsRespond ─────────────────────────────────────────────────────────

phone_tool!(
    PhoneCallsRespond,
    name = "phone_calls_respond",
    desc = "Respond to a pending incoming call: accept, reject, or silence it. \
            Full build only (requires Call Screening permission).",
    schema = serde_json::json!({
        "type": "object",
        "required": ["action"],
        "properties": {
            "action": {
                "type": "string",
                "description": "accept | reject | silence",
                "enum": ["accept", "reject", "silence"]
            }
        }
    }),
    exec = |self, args| {
        match self.post("/phone/calls/respond").json(&args).send().await {
            Ok(r)  => ok_result(r.text().await.unwrap_or_default()),
            Err(e) => err_result(format!("bridge request failed: {e}")),
        }
    }
);

// ── PhoneA11yScreenshot ───────────────────────────────────────────────────────

phone_tool!(
    PhoneA11yScreenshot,
    name = "phone_a11y_screenshot",
    desc = "Take a screenshot of the current screen. Returns a base64-encoded JPEG. \
            Full build only (requires Accessibility Service permission).",
    schema = serde_json::json!({ "type": "object", "properties": {} }),
    exec = |self, _args| {
        match self.get("/phone/a11y/screenshot").send().await {
            Ok(r)  => ok_result(r.text().await.unwrap_or_default()),
            Err(e) => err_result(format!("bridge request failed: {e}")),
        }
    }
);

// ── PhoneA11yTree ─────────────────────────────────────────────────────────────

phone_tool!(
    PhoneA11yTree,
    name = "phone_a11y_tree",
    desc = "Get the current screen's accessibility UI tree (visible elements, text, resource IDs). \
            Use this to understand what's on screen before clicking. \
            Full build only (requires Accessibility Service permission).",
    schema = serde_json::json!({ "type": "object", "properties": {} }),
    exec = |self, _args| {
        match self.get("/phone/a11y/tree").send().await {
            Ok(r)  => ok_result(r.text().await.unwrap_or_default()),
            Err(e) => err_result(format!("bridge request failed: {e}")),
        }
    }
);

// ── PhoneA11yClick ────────────────────────────────────────────────────────────

phone_tool!(
    PhoneA11yClick,
    name = "phone_a11y_click",
    desc = "Tap a UI element by resource_id or text label. Use after phone_a11y_tree to find \
            the target element. Full build only (requires Accessibility Service permission).",
    schema = serde_json::json!({
        "type": "object",
        "properties": {
            "resource_id": { "type": "string", "description": "Android resource ID of the element (e.g. com.app:id/button)" },
            "text":        { "type": "string", "description": "Visible text of the element to tap" }
        }
    }),
    exec = |self, args| {
        match self.post("/phone/a11y/click").json(&args).send().await {
            Ok(r)  => ok_result(r.text().await.unwrap_or_default()),
            Err(e) => err_result(format!("bridge request failed: {e}")),
        }
    }
);

// ── PhoneA11yGlobal ───────────────────────────────────────────────────────────

phone_tool!(
    PhoneA11yGlobal,
    name = "phone_a11y_global",
    desc = "Perform a global accessibility action: back, home, recents, notifications, or \
            quick_settings. Full build only (requires Accessibility Service permission).",
    schema = serde_json::json!({
        "type": "object",
        "required": ["action"],
        "properties": {
            "action": {
                "type": "string",
                "description": "back | home | recents | notifications | quick_settings",
                "enum": ["back", "home", "recents", "notifications", "quick_settings"]
            }
        }
    }),
    exec = |self, args| {
        match self.post("/phone/a11y/global").json(&args).send().await {
            Ok(r)  => ok_result(r.text().await.unwrap_or_default()),
            Err(e) => err_result(format!("bridge request failed: {e}")),
        }
    }
);

// ── PhoneDeviceInfo ───────────────────────────────────────────────────────────

phone_tool!(
    PhoneDeviceInfo,
    name = "phone_device_info",
    desc = "Get device context: battery level/charging state, network type (WiFi/mobile/offline), \
            timezone, and device model. Useful for context-aware agent behaviour.",
    schema = serde_json::json!({ "type": "object", "properties": {} }),
    exec = |self, _args| {
        let f1 = self.get("/phone/battery").send();
        let f2 = self.get("/phone/device").send();
        let f3 = self.get("/phone/network").send();
        let (r1, r2, r3) = join!(f1, f2, f3);
        let mut parts = Vec::new();
        if let Ok(r) = r1 { parts.push(r.text().await.unwrap_or_default()); }
        if let Ok(r) = r2 { parts.push(r.text().await.unwrap_or_default()); }
        if let Ok(r) = r3 { parts.push(r.text().await.unwrap_or_default()); }
        ok_result(parts.join("\n"))
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
