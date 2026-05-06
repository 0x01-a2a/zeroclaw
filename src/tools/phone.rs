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

            fn put(&$self, path: &str) -> reqwest::RequestBuilder {
                client($self.timeout_secs)
                    .put(format!("{}{}", $self.bridge_url, path))
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
        if let Some(s) = args["name"].as_str() {
            if s.len() > 256 { return Ok(ToolResult { success: false, output: String::new(), error: Some("name exceeds 256 character limit".into()) }); }
        }
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
        let box_ = args["box"].as_str().unwrap_or("inbox");
        if !matches!(box_, "inbox" | "sent" | "draft") {
            return Ok(ToolResult { success: false, output: String::new(), error: Some("box must be one of: inbox, sent, draft".into()) });
        }
        let limit = args["limit"].as_u64().unwrap_or(20).min(200);
        let path  = format!("/phone/sms?box={}&limit={}", urlencoding::encode(box_), limit);
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
        if let Some(body) = args["body"].as_str() {
            if body.len() > 1600 {
                return Ok(ToolResult { success: false, output: String::new(), error: Some("body exceeds 1600 character limit".into()) });
            }
        }
        if let Some(to) = args["to"].as_str() {
            if to.len() > 32 {
                return Ok(ToolResult { success: false, output: String::new(), error: Some("to field exceeds 32 character limit".into()) });
            }
        }
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
        if let Some(s) = args["title"].as_str() {
            if s.len() > 512 { return Ok(ToolResult { success: false, output: String::new(), error: Some("title exceeds 512 character limit".into()) }); }
        }
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
        if let Some(reply) = args["reply"].as_str() {
            if reply.len() > 1600 {
                return Ok(ToolResult { success: false, output: String::new(), error: Some("reply exceeds 1600 character limit".into()) });
            }
        }
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
    desc = "Tap the screen at pixel coordinates (x, y). Use phone_a11y_tree or \
            phone_a11y_screenshot to find the element position first. \
            Full build only (requires Accessibility Service permission).",
    schema = serde_json::json!({
        "type": "object",
        "required": ["x", "y"],
        "properties": {
            "x": { "type": "integer", "description": "Horizontal pixel coordinate" },
            "y": { "type": "integer", "description": "Vertical pixel coordinate" }
        }
    }),
    exec = |self, args| {
        let x = match args["x"].as_i64() {
            Some(v) => v,
            None => return Ok(ToolResult { success: false, output: String::new(), error: Some("missing required field: x".into()) }),
        };
        let y = match args["y"].as_i64() {
            Some(v) => v,
            None => return Ok(ToolResult { success: false, output: String::new(), error: Some("missing required field: y".into()) }),
        };
        if x < 0 || y < 0 || x > 9999 || y > 9999 {
            return Ok(ToolResult { success: false, output: String::new(), error: Some(format!("coordinates ({x},{y}) out of valid range [0,9999]")) });
        }
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

// ── PhoneContactsUpdate ───────────────────────────────────────────────────────

phone_tool!(
    PhoneContactsUpdate,
    name = "phone_contacts_update",
    desc = "Update an existing contact in the device address book by ID. \
            Use phone_contacts_read to find the contact ID first.",
    schema = serde_json::json!({
        "type": "object",
        "required": ["id"],
        "properties": {
            "id":    { "type": "string", "description": "Contact ID from phone_contacts_read" },
            "name":  { "type": "string", "description": "New display name" },
            "phone": { "type": "string", "description": "New phone number" }
        }
    }),
    exec = |self, args| {
        let id   = args["id"].as_str().unwrap_or("").to_string();
        let path = format!("/phone/contacts/{}", urlencoding::encode(&id));
        match self.put(&path).json(&args).send().await {
            Ok(r)  => ok_result(r.text().await.unwrap_or_default()),
            Err(e) => err_result(format!("bridge request failed: {e}")),
        }
    }
);

// ── PhoneCalendarUpdate ───────────────────────────────────────────────────────

phone_tool!(
    PhoneCalendarUpdate,
    name = "phone_calendar_update",
    desc = "Update an existing calendar event by ID. \
            Use phone_calendar_read to find the event ID first. \
            dtstart/dtend are Unix epoch milliseconds.",
    schema = serde_json::json!({
        "type": "object",
        "required": ["id"],
        "properties": {
            "id":          { "type": "string",  "description": "Event ID from phone_calendar_read" },
            "title":       { "type": "string",  "description": "New event title" },
            "description": { "type": "string",  "description": "New event description" },
            "dtstart":     { "type": "integer", "description": "New start time (ms since epoch)" },
            "dtend":       { "type": "integer", "description": "New end time (ms since epoch)" }
        }
    }),
    exec = |self, args| {
        let id   = args["id"].as_str().unwrap_or("").to_string();
        let path = format!("/phone/calendar/{}", urlencoding::encode(&id));
        match self.put(&path).json(&args).send().await {
            Ok(r)  => ok_result(r.text().await.unwrap_or_default()),
            Err(e) => err_result(format!("bridge request failed: {e}")),
        }
    }
);

// ── PhoneMediaImages ──────────────────────────────────────────────────────────

phone_tool!(
    PhoneMediaImages,
    name = "phone_media_images",
    desc = "List images stored on the device (most recent first). Returns ID, URI, \
            file name, date_taken, size_bytes, width, and height. limit defaults to 20.",
    schema = serde_json::json!({
        "type": "object",
        "properties": {
            "limit": { "type": "integer", "description": "Max images to return (default 20)", "default": 20 }
        }
    }),
    exec = |self, args| {
        let limit = args["limit"].as_u64().unwrap_or(20);
        let path  = format!("/phone/media/images?limit={limit}");
        match self.get(&path).send().await {
            Ok(r)  => ok_result(r.text().await.unwrap_or_default()),
            Err(e) => err_result(format!("bridge request failed: {e}")),
        }
    }
);

// ── PhoneActivity ─────────────────────────────────────────────────────────────

phone_tool!(
    PhoneActivity,
    name = "phone_activity",
    desc = "Read the device step counter since last reboot. \
            Requires ACTIVITY_RECOGNITION permission.",
    schema = serde_json::json!({ "type": "object", "properties": {} }),
    exec = |self, _args| {
        match self.get("/phone/activity").send().await {
            Ok(r)  => ok_result(r.text().await.unwrap_or_default()),
            Err(e) => err_result(format!("bridge request failed: {e}")),
        }
    }
);

// ── PhoneImuSnapshot ──────────────────────────────────────────────────────────

phone_tool!(
    PhoneImuSnapshot,
    name = "phone_imu_snapshot",
    desc = "Take a single IMU snapshot: accelerometer (m/s²) and optional gyroscope (rad/s). \
            Useful for detecting motion or orientation.",
    schema = serde_json::json!({
        "type": "object",
        "properties": {
            "gyroscope": { "type": "boolean", "description": "Include gyroscope readings (default false)", "default": false }
        }
    }),
    exec = |self, args| {
        let gyro = args["gyroscope"].as_bool().unwrap_or(false);
        let path = format!("/phone/imu?gyroscope={gyro}");
        match self.get(&path).send().await {
            Ok(r)  => ok_result(r.text().await.unwrap_or_default()),
            Err(e) => err_result(format!("bridge request failed: {e}")),
        }
    }
);

// ── PhoneImuRecord ────────────────────────────────────────────────────────────

pub struct PhoneImuRecord {
    bridge_url:   String,
    secret:       String,
    timeout_secs: u64,
}

impl PhoneImuRecord {
    pub fn new(bridge_url: String, secret: String, timeout_secs: u64) -> Self {
        Self { bridge_url, secret, timeout_secs }
    }
}

#[async_trait]
impl Tool for PhoneImuRecord {
    fn name(&self) -> &str { "phone_imu_record" }
    fn description(&self) -> &str {
        "Record IMU sensor data (accelerometer + optional gyroscope) over a duration. \
         sample_hz controls sampling rate (10–200 Hz, default 50). \
         duration_ms controls length (500–30000 ms, default 5000). \
         Returns an array of timestamped samples."
    }
    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "duration_ms": { "type": "integer", "description": "Recording duration ms (500–30000)", "default": 5000 },
                "sample_hz":   { "type": "integer", "description": "Sample rate Hz (10–200)", "default": 50 },
                "gyroscope":   { "type": "boolean", "description": "Include gyroscope (default false)", "default": false }
            }
        })
    }
    async fn execute(&self, args: Value) -> anyhow::Result<ToolResult> {
        let duration_ms = args["duration_ms"].as_u64().unwrap_or(5_000);
        let extra_secs  = (duration_ms / 1_000) + self.timeout_secs;
        let req = client(extra_secs)
            .post(format!("{}/phone/imu/record", self.bridge_url))
            .header("X-Bridge-Token", &self.secret)
            .json(&args);
        match req.send().await {
            Ok(r)  => ok_result(r.text().await.unwrap_or_default()),
            Err(e) => err_result(format!("bridge request failed: {e}")),
        }
    }
}

// ── PhoneAppUsage ─────────────────────────────────────────────────────────────

phone_tool!(
    PhoneAppUsage,
    name = "phone_app_usage",
    desc = "Get foreground app usage time for the past N days (default 7). \
            Returns top 20 apps by foreground time. \
            Requires Usage Access special permission (user must grant in Android Settings).",
    schema = serde_json::json!({
        "type": "object",
        "properties": {
            "days": { "type": "integer", "description": "Days to look back (1–30, default 7)", "default": 7 }
        }
    }),
    exec = |self, args| {
        let days = args["days"].as_u64().unwrap_or(7);
        let path = format!("/phone/app_usage?days={days}");
        match self.get(&path).send().await {
            Ok(r)  => ok_result(r.text().await.unwrap_or_default()),
            Err(e) => err_result(format!("bridge request failed: {e}")),
        }
    }
);

// ── PhoneAlarmSet ─────────────────────────────────────────────────────────────

phone_tool!(
    PhoneAlarmSet,
    name = "phone_alarm_set",
    desc = "Set an alarm on the device clock app. hour (0–23) and minute (0–59) are required. \
            message is an optional alarm label.",
    schema = serde_json::json!({
        "type": "object",
        "required": ["hour", "minute"],
        "properties": {
            "hour":    { "type": "integer", "description": "Hour (0–23)" },
            "minute":  { "type": "integer", "description": "Minute (0–59)" },
            "message": { "type": "string",  "description": "Optional alarm label" }
        }
    }),
    exec = |self, args| {
        match self.post("/phone/alarm").json(&args).send().await {
            Ok(r)  => ok_result(r.text().await.unwrap_or_default()),
            Err(e) => err_result(format!("bridge request failed: {e}")),
        }
    }
);

// ── PhoneTimezone ─────────────────────────────────────────────────────────────

phone_tool!(
    PhoneTimezone,
    name = "phone_timezone",
    desc = "Get the device's current timezone: ID (e.g. America/New_York), display name, \
            UTC offset minutes, and DST active flag.",
    schema = serde_json::json!({ "type": "object", "properties": {} }),
    exec = |self, _args| {
        match self.get("/phone/timezone").send().await {
            Ok(r)  => ok_result(r.text().await.unwrap_or_default()),
            Err(e) => err_result(format!("bridge request failed: {e}")),
        }
    }
);

// ── PhoneWifi ─────────────────────────────────────────────────────────────────

phone_tool!(
    PhoneWifi,
    name = "phone_wifi",
    desc = "Get current WiFi connection details: enabled state, SSID, IP address, \
            signal strength (RSSI), link speed, frequency, and connection state.",
    schema = serde_json::json!({ "type": "object", "properties": {} }),
    exec = |self, _args| {
        match self.get("/phone/wifi").send().await {
            Ok(r)  => ok_result(r.text().await.unwrap_or_default()),
            Err(e) => err_result(format!("bridge request failed: {e}")),
        }
    }
);

// ── PhoneCarrier ──────────────────────────────────────────────────────────────

phone_tool!(
    PhoneCarrier,
    name = "phone_carrier",
    desc = "Get cellular carrier info: operator name, SIM operator, country ISO, \
            network type, roaming status, and call state.",
    schema = serde_json::json!({ "type": "object", "properties": {} }),
    exec = |self, _args| {
        match self.get("/phone/carrier").send().await {
            Ok(r)  => ok_result(r.text().await.unwrap_or_default()),
            Err(e) => err_result(format!("bridge request failed: {e}")),
        }
    }
);

// ── PhoneBluetooth ────────────────────────────────────────────────────────────

phone_tool!(
    PhoneBluetooth,
    name = "phone_bluetooth",
    desc = "List paired Bluetooth devices: address, name, and type (classic/le/dual). \
            Also returns Bluetooth enabled state.",
    schema = serde_json::json!({ "type": "object", "properties": {} }),
    exec = |self, _args| {
        match self.get("/phone/bluetooth").send().await {
            Ok(r)  => ok_result(r.text().await.unwrap_or_default()),
            Err(e) => err_result(format!("bridge request failed: {e}")),
        }
    }
);

// ── PhoneAudioProfileGet ──────────────────────────────────────────────────────

phone_tool!(
    PhoneAudioProfileGet,
    name = "phone_audio_profile_get",
    desc = "Read current audio profile: volume levels for all streams (ring, media, alarm, \
            notification, call), ringer mode (normal/silent/vibrate), and DND mode.",
    schema = serde_json::json!({ "type": "object", "properties": {} }),
    exec = |self, _args| {
        match self.get("/phone/audio/profile").send().await {
            Ok(r)  => ok_result(r.text().await.unwrap_or_default()),
            Err(e) => err_result(format!("bridge request failed: {e}")),
        }
    }
);

// ── PhoneAudioProfileSet ──────────────────────────────────────────────────────

phone_tool!(
    PhoneAudioProfileSet,
    name = "phone_audio_profile_set",
    desc = "Set device volume or DND mode. To set volume: provide stream \
            (ring|media|alarm|notification|call) and volume (0–max). \
            To set DND: provide dnd_mode (off|priority|alarms|none).",
    schema = serde_json::json!({
        "type": "object",
        "properties": {
            "stream":   { "type": "string",  "description": "ring | media | alarm | notification | call" },
            "volume":   { "type": "integer", "description": "Volume level (0 to stream max)" },
            "dnd_mode": { "type": "string",  "description": "off | priority | alarms | none" }
        }
    }),
    exec = |self, args| {
        match self.post("/phone/audio/profile").json(&args).send().await {
            Ok(r)  => ok_result(r.text().await.unwrap_or_default()),
            Err(e) => err_result(format!("bridge request failed: {e}")),
        }
    }
);

// ── PhoneVibrate ──────────────────────────────────────────────────────────────

phone_tool!(
    PhoneVibrate,
    name = "phone_vibrate",
    desc = "Vibrate the device. duration_ms controls length (1–5000 ms, default 300). \
            amplitude controls intensity (-1 for device default, 1–255 for explicit).",
    schema = serde_json::json!({
        "type": "object",
        "properties": {
            "duration_ms": { "type": "integer", "description": "Duration in ms (1–5000)", "default": 300 },
            "amplitude":   { "type": "integer", "description": "-1 for default, 1–255 for explicit", "default": -1 }
        }
    }),
    exec = |self, args| {
        match self.post("/phone/vibrate").json(&args).send().await {
            Ok(r)  => ok_result(r.text().await.unwrap_or_default()),
            Err(e) => err_result(format!("bridge request failed: {e}")),
        }
    }
);

// ── PhoneNotificationsHistory ─────────────────────────────────────────────────

phone_tool!(
    PhoneNotificationsHistory,
    name = "phone_notifications_history",
    desc = "Read the notification history log (recently dismissed/posted notifications). \
            Requires Notification Listener permission.",
    schema = serde_json::json!({
        "type": "object",
        "properties": {
            "limit": { "type": "integer", "description": "Max entries to return (default 20)", "default": 20 }
        }
    }),
    exec = |self, args| {
        let limit = args["limit"].as_u64().unwrap_or(20);
        let path  = format!("/phone/notifications/history?limit={limit}");
        match self.get(&path).send().await {
            Ok(r)  => ok_result(r.text().await.unwrap_or_default()),
            Err(e) => err_result(format!("bridge request failed: {e}")),
        }
    }
);

// ── PhoneCallsHistory ─────────────────────────────────────────────────────────

phone_tool!(
    PhoneCallsHistory,
    name = "phone_calls_history",
    desc = "Read the call screening history (recent calls handled by the agent). \
            Full build only (requires Call Screening permission).",
    schema = serde_json::json!({
        "type": "object",
        "properties": {
            "limit": { "type": "integer", "description": "Max entries to return (default 20)", "default": 20 }
        }
    }),
    exec = |self, args| {
        let limit = args["limit"].as_u64().unwrap_or(20);
        let path  = format!("/phone/calls/history?limit={limit}");
        match self.get(&path).send().await {
            Ok(r)  => ok_result(r.text().await.unwrap_or_default()),
            Err(e) => err_result(format!("bridge request failed: {e}")),
        }
    }
);

// ── PhoneA11yAction ───────────────────────────────────────────────────────────

phone_tool!(
    PhoneA11yAction,
    name = "phone_a11y_action",
    desc = "Perform a named accessibility action on a UI element by its Android resource viewId. \
            Use phone_a11y_tree to find viewIds. Common actions: 'click', 'long_click', \
            'focus', 'scroll_forward', 'scroll_backward', 'copy', 'paste', 'cut', 'dismiss'. \
            Optionally supply text for SET_TEXT action. Full build only.",
    schema = serde_json::json!({
        "type": "object",
        "required": ["viewId", "action"],
        "properties": {
            "viewId": { "type": "string", "description": "Android resource ID from phone_a11y_tree (e.g. com.app:id/button)" },
            "action": { "type": "string", "description": "Action name: click | long_click | focus | scroll_forward | scroll_backward | copy | paste | cut | dismiss" },
            "text":   { "type": "string", "description": "Text value for set_text action" }
        }
    }),
    exec = |self, args| {
        match self.post("/phone/a11y/action").json(&args).send().await {
            Ok(r)  => ok_result(r.text().await.unwrap_or_default()),
            Err(e) => err_result(format!("bridge request failed: {e}")),
        }
    }
);

// ── PhoneHealthRead ───────────────────────────────────────────────────────────

phone_tool!(
    PhoneHealthRead,
    name = "phone_health_read",
    desc = "Read health data from Android Health Connect (aggregated from wearables, fitness apps, and the phone itself). \
            Returns steps, heart rate, HRV, sleep stages, calories burned, oxygen saturation, and weight.",
    schema = serde_json::json!({
        "type": "object",
        "properties": {
            "types": {
                "type": "string",
                "description": "Comma-separated types: steps, heart_rate, hrv, sleep, calories, oxygen_saturation, weight",
                "default": "steps,heart_rate,sleep,calories"
            },
            "days": {
                "type": "integer",
                "description": "Number of past days to query (1–90)",
                "default": 7
            }
        }
    }),
    exec = |self, args| {
        let types = args["types"].as_str().unwrap_or("steps,heart_rate,sleep,calories");
        let days  = args["days"].as_u64().unwrap_or(7);
        let path  = format!("/phone/health?types={}&days={}", urlencoding::encode(types), days);
        match self.get(&path).send().await {
            Ok(r)  => ok_result(r.text().await.unwrap_or_default()),
            Err(e) => err_result(format!("bridge request failed: {e}")),
        }
    }
);

// ── PhoneWearableScan ─────────────────────────────────────────────────────────

phone_tool!(
    PhoneWearableScan,
    name = "phone_wearable_scan",
    desc = "Scan for nearby Bluetooth LE wearables advertising standard health profiles \
            (heart rate monitor, glucose meter, CGM, smart scale, running pod, battery). \
            Returns a list of discovered devices with address, name, RSSI, and services.",
    schema = serde_json::json!({
        "type": "object",
        "properties": {
            "duration_ms": {
                "type": "integer",
                "description": "Scan duration in milliseconds (2000–15000)",
                "default": 8000
            }
        }
    }),
    exec = |self, args| {
        let duration_ms = args["duration_ms"].as_u64().unwrap_or(8000);
        let path = format!("/phone/wearables/scan?duration_ms={duration_ms}");
        match self.get(&path).send().await {
            Ok(r)  => ok_result(r.text().await.unwrap_or_default()),
            Err(e) => err_result(format!("bridge request failed: {e}")),
        }
    }
);

// ── PhoneWearableRead ─────────────────────────────────────────────────────────

phone_tool!(
    PhoneWearableRead,
    name = "phone_wearable_read",
    desc = "Connect to a specific Bluetooth LE wearable and read one health service characteristic. \
            Use phone_wearable_scan first to discover device addresses. \
            Supported services: heart_rate, battery, body_composition, running_speed_cadence, glucose, cgm.",
    schema = serde_json::json!({
        "type": "object",
        "required": ["device", "service"],
        "properties": {
            "device":  {
                "type": "string",
                "description": "BLE device MAC address (e.g. AA:BB:CC:DD:EE:FF)"
            },
            "service": {
                "type": "string",
                "description": "heart_rate | battery | body_composition | running_speed_cadence | glucose | cgm"
            }
        }
    }),
    exec = |self, args| {
        let device  = args["device"].as_str().unwrap_or("");
        let service = args["service"].as_str().unwrap_or("");
        let path = format!(
            "/phone/wearables/read?device={}&service={}",
            urlencoding::encode(device),
            urlencoding::encode(service),
        );
        match self.get(&path).send().await {
            Ok(r)  => ok_result(r.text().await.unwrap_or_default()),
            Err(e) => err_result(format!("bridge request failed: {e}")),
        }
    }
);

// ── PhoneRecoveryStatus ───────────────────────────────────────────────────────

phone_tool!(
    PhoneRecoveryStatus,
    name = "phone_recovery_status",
    desc = "Compute a sleep + recovery readiness score (0-100) from the last sleep session, \
            HRV, and resting heart rate vs 30-day baselines. Returns score, label \
            (Optimal/Good/Fair/Poor), per-component scores, and actionable insights.",
    schema = serde_json::json!({
        "type": "object",
        "properties": {}
    }),
    exec = |self, _args| {
        match self.get("/phone/recovery").send().await {
            Ok(r)  => ok_result(r.text().await.unwrap_or_default()),
            Err(e) => err_result(format!("bridge request failed: {e}")),
        }
    }
);

// ── PhoneA11yVision ───────────────────────────────────────────────────────────

phone_tool!(
    PhoneA11yVision,
    name = "phone_a11y_vision",
    desc = "Capture a screenshot and analyse it with Gemini Vision. Returns a structured \
            JSON response with 'analysis' (text description) and 'actions' (suggested \
            accessibility actions to take). Optionally includes the UI tree for richer \
            context. Rate-limited to 1 call per 3 seconds. Full build only.",
    schema = serde_json::json!({
        "type": "object",
        "required": ["prompt"],
        "properties": {
            "prompt":       { "type": "string",  "description": "What to analyse or do on screen" },
            "include_tree": { "type": "boolean", "description": "Include accessibility tree context (default false)", "default": false }
        }
    }),
    exec = |self, args| {
        match self.post("/phone/a11y/vision").json(&args).send().await {
            Ok(r)  => ok_result(r.text().await.unwrap_or_default()),
            Err(e) => err_result(format!("bridge request failed: {e}")),
        }
    }
);

// ── PhoneTts ──────────────────────────────────────────────────────────────────

phone_tool!(
    PhoneTts,
    name = "phone_tts",
    desc = "Speak text aloud via the phone's built-in text-to-speech engine. \
            Free, offline, zero API cost. Use while performing tasks for the owner \
            to narrate what you are doing in real time — especially during highlight \
            reel recordings. Returns immediately; speech happens asynchronously. \
            rate: 0.1–4.0 (default 1.0), pitch: 0.1–2.0 (default 1.0).",
    schema = serde_json::json!({
        "type": "object",
        "required": ["text"],
        "properties": {
            "text":  { "type": "string",  "description": "Text to speak aloud (max 4000 characters)" },
            "rate":  { "type": "number",  "description": "Speech rate multiplier (default 1.0)", "default": 1.0 },
            "pitch": { "type": "number",  "description": "Voice pitch multiplier (default 1.0)", "default": 1.0 }
        }
    }),
    exec = |self, args| {
        if args["text"].as_str().map(|s| s.len()).unwrap_or(0) > 4000 {
            return err_result("text exceeds 4000 character limit");
        }
        match self.post("/phone/tts").json(&args).send().await {
            Ok(r)  => ok_result(r.text().await.unwrap_or_default()),
            Err(e) => err_result(format!("bridge request failed: {e}")),
        }
    }
);

// ── PhoneHighlightStart ───────────────────────────────────────────────────────

phone_tool!(
    PhoneHighlightStart,
    name = "phone_highlight_start",
    desc = "Start a screen recording on the owner's phone to capture a highlight reel. \
            The recording captures everything visible on screen. Use this before beginning \
            a task you want to showcase — then narrate each step with phone_tts as you work. \
            Requires the owner to have granted screen recording permission once via the app. \
            Returns immediately once recording begins.",
    schema = serde_json::json!({
        "type": "object",
        "properties": {}
    }),
    exec = |self, _args| {
        match self.post("/phone/highlight/start").json(&serde_json::json!({})).send().await {
            Ok(r)  => ok_result(r.text().await.unwrap_or_default()),
            Err(e) => err_result(format!("bridge request failed: {e}")),
        }
    }
);

// ── PhoneHighlightStop ────────────────────────────────────────────────────────

phone_tool!(
    PhoneHighlightStop,
    name = "phone_highlight_stop",
    desc = "Stop the current screen recording and save it to the owner's camera roll \
            (DCIM/Highlights). Returns the filename of the saved video. \
            The owner can then review the recording and post it manually to social media \
            to show followers what you accomplished. Call this after completing the task \
            you were recording.",
    schema = serde_json::json!({
        "type": "object",
        "properties": {}
    }),
    exec = |self, _args| {
        match self.post("/phone/highlight/stop").json(&serde_json::json!({})).send().await {
            Ok(r)  => ok_result(r.text().await.unwrap_or_default()),
            Err(e) => err_result(format!("bridge request failed: {e}")),
        }
    }
);

// ── PhoneHighlightPublish ─────────────────────────────────────────────────────

phone_tool!(
    PhoneHighlightPublish,
    name = "phone_highlight_publish",
    desc = "Upload a highlight reel video to the 0x01 aggregator so it appears on \
            the agent's public profile. Call this after phone_highlight_stop returns \
            a content_uri. Requires aggregator_url (the public aggregator base URL) \
            and agent_id (this agent's hex identity). \
            Returns { reel_url: \"/reels/...\" } on success.",
    schema = serde_json::json!({
        "type": "object",
        "required": ["content_uri", "aggregator_url", "agent_id"],
        "properties": {
            "content_uri":     { "type": "string", "description": "MediaStore content URI returned by phone_highlight_stop" },
            "aggregator_url":  { "type": "string", "description": "Base URL of the 0x01 aggregator, e.g. https://aggregator.0x01.world" },
            "agent_id":        { "type": "string", "description": "This agent's hex-encoded 32-byte identity (64 hex chars)" }
        }
    }),
    exec = |self, args| {
        let content_uri = match args["content_uri"].as_str() {
            Some(s) if !s.is_empty() => s.to_string(),
            _ => return err_result("content_uri is required"),
        };
        let aggregator_url = match args["aggregator_url"].as_str() {
            Some(s) if !s.is_empty() => s.to_string(),
            _ => return err_result("aggregator_url is required"),
        };
        let agent_id = match args["agent_id"].as_str() {
            Some(s) if !s.is_empty() => s.to_string(),
            _ => return err_result("agent_id is required"),
        };

        match self.post("/phone/highlight/publish")
            .json(&serde_json::json!({
                "content_uri":    content_uri,
                "aggregator_url": aggregator_url,
                "agent_id":       agent_id,
            }))
            .send()
            .await
        {
            Ok(r) => ok_result(r.text().await.unwrap_or_default()),
            Err(e) => err_result(format!("bridge request failed: {e}")),
        }
    }
);

// ── PhoneA11ySwipe ────────────────────────────────────────────────────────────

phone_tool!(
    PhoneA11ySwipe,
    name = "phone_a11y_swipe",
    desc = "Perform a raw swipe gesture on the screen from (x1,y1) to (x2,y2). \
            Works where SCROLL_FORWARD/BACKWARD accessibility actions do not apply — \
            carousels, maps, custom views, pull-to-refresh. \
            duration_ms controls speed (50–3000 ms, default 300). \
            Coordinates are in screen pixels. Full build only.",
    schema = serde_json::json!({
        "type": "object",
        "required": ["x1", "y1", "x2", "y2"],
        "properties": {
            "x1":          { "type": "integer", "description": "Start X coordinate (pixels)" },
            "y1":          { "type": "integer", "description": "Start Y coordinate (pixels)" },
            "x2":          { "type": "integer", "description": "End X coordinate (pixels)" },
            "y2":          { "type": "integer", "description": "End Y coordinate (pixels)" },
            "duration_ms": { "type": "integer", "description": "Swipe duration in ms (50–3000)", "default": 300 }
        }
    }),
    exec = |self, args| {
        match self.post("/phone/a11y/swipe").json(&args).send().await {
            Ok(r)  => ok_result(r.text().await.unwrap_or_default()),
            Err(e) => err_result(format!("bridge request failed: {e}")),
        }
    }
);

// ── PhoneA11yType ─────────────────────────────────────────────────────────────

phone_tool!(
    PhoneA11yType,
    name = "phone_a11y_type",
    desc = "Type text into an editable field. Without view_id, targets the currently \
            focused or first editable field on screen. Optionally specify view_id \
            (from phone_a11y_tree) to target a specific field. \
            Returns { success: bool }. Full build only.",
    schema = serde_json::json!({
        "type": "object",
        "required": ["text"],
        "properties": {
            "text":    { "type": "string", "description": "Text to type into the field" },
            "view_id": { "type": "string", "description": "Optional: Android resource ID to target (e.g. com.app:id/input)" }
        }
    }),
    exec = |self, args| {
        match self.post("/phone/a11y/type").json(&args).send().await {
            Ok(r)  => ok_result(r.text().await.unwrap_or_default()),
            Err(e) => err_result(format!("bridge request failed: {e}")),
        }
    }
);

// ── PhoneA11yWaitFor ──────────────────────────────────────────────────────────

phone_tool!(
    PhoneA11yWaitFor,
    name = "phone_a11y_wait_for",
    desc = "Wait for a UI element to appear on screen, then optionally tap it. \
            Match by view_id, text, content_desc, or class_name (at least one required). \
            timeout_ms: how long to wait (100–30000 ms, default 5000). \
            tap: if true, click the element once found (default false). \
            Returns { found: bool, tapped: bool, node?: {...} }. Full build only.",
    schema = serde_json::json!({
        "type": "object",
        "properties": {
            "view_id":      { "type": "string",  "description": "Android resource ID (e.g. com.app:id/btn_ok)" },
            "text":         { "type": "string",  "description": "Element text to match" },
            "content_desc": { "type": "string",  "description": "Content description to match" },
            "class_name":   { "type": "string",  "description": "Class name to match (e.g. android.widget.Button)" },
            "exact_text":   { "type": "boolean", "description": "Require exact text match (default false)", "default": false },
            "timeout_ms":   { "type": "integer", "description": "Max wait time in ms (100–30000)", "default": 5000 },
            "tap":          { "type": "boolean", "description": "Tap the element once found (default false)", "default": false }
        }
    }),
    exec = |self, args| {
        match self.post("/phone/a11y/wait_for").json(&args).send().await {
            Ok(r)  => ok_result(r.text().await.unwrap_or_default()),
            Err(e) => err_result(format!("bridge request failed: {e}")),
        }
    }
);

// ── PhoneA11yScrollFind ───────────────────────────────────────────────────────

phone_tool!(
    PhoneA11yScrollFind,
    name = "phone_a11y_scroll_find",
    desc = "Scroll a list or container until a target element appears, then optionally tap it. \
            Match by view_id, text, content_desc, or class_name (at least one required). \
            direction: up|down|left|right (default down). \
            max_scrolls: how many scroll steps before giving up (1–50, default 10). \
            container_view_id: optional resource ID of the scrollable container to scroll. \
            wait_after_ms: pause after each scroll step (100–2000 ms, default 400). \
            tap: whether to click on find (default true). \
            Returns { found: bool, tapped: bool }. Full build only.",
    schema = serde_json::json!({
        "type": "object",
        "properties": {
            "view_id":           { "type": "string",  "description": "Android resource ID to find" },
            "text":              { "type": "string",  "description": "Element text to find" },
            "content_desc":      { "type": "string",  "description": "Content description to find" },
            "class_name":        { "type": "string",  "description": "Class name to find" },
            "exact_text":        { "type": "boolean", "description": "Require exact text match (default false)", "default": false },
            "direction":         { "type": "string",  "description": "Scroll direction: up | down | left | right", "default": "down" },
            "max_scrolls":       { "type": "integer", "description": "Max scroll steps (1–50)", "default": 10 },
            "container_view_id": { "type": "string",  "description": "Resource ID of scrollable container (optional)" },
            "wait_after_ms":     { "type": "integer", "description": "Pause after each scroll (100–2000 ms)", "default": 400 },
            "tap":               { "type": "boolean", "description": "Tap element on find (default true)", "default": true }
        }
    }),
    exec = |self, args| {
        match self.post("/phone/a11y/scroll_find").json(&args).send().await {
            Ok(r)  => ok_result(r.text().await.unwrap_or_default()),
            Err(e) => err_result(format!("bridge request failed: {e}")),
        }
    }
);

// ── PhoneA11yTapText ──────────────────────────────────────────────────────────

phone_tool!(
    PhoneA11yTapText,
    name = "phone_a11y_tap_text",
    desc = "Find the first UI element whose text matches the given string and click it. \
            Simpler than phone_a11y_action when you do not have a viewId. \
            exact: if false (default), partial match is used. \
            timeout_ms: how long to wait for the element (100–15000 ms, default 3000). \
            Returns { success: bool }. Full build only.",
    schema = serde_json::json!({
        "type": "object",
        "required": ["text"],
        "properties": {
            "text":       { "type": "string",  "description": "Text to find and tap" },
            "exact":      { "type": "boolean", "description": "Exact text match (default false)", "default": false },
            "timeout_ms": { "type": "integer", "description": "Max wait time in ms (100–15000)", "default": 3000 }
        }
    }),
    exec = |self, args| {
        match self.post("/phone/a11y/tap_text").json(&args).send().await {
            Ok(r)  => ok_result(r.text().await.unwrap_or_default()),
            Err(e) => err_result(format!("bridge request failed: {e}")),
        }
    }
);

// ── PhoneA11yTreeInteractive ──────────────────────────────────────────────────

phone_tool!(
    PhoneA11yTreeInteractive,
    name = "phone_a11y_tree_interactive",
    desc = "Return a compact accessibility tree containing only interactive nodes \
            (clickable, editable, focusable). Much smaller payload than phone_a11y_tree — \
            use this when you need to locate a tap target without processing the full tree. \
            Result is cached for 5 seconds. \
            Returns { package, activity, interactive_count, nodes: [...] }. Full build only.",
    schema = serde_json::json!({
        "type": "object",
        "properties": {}
    }),
    exec = |self, _args| {
        match self.get("/phone/a11y/tree_interactive").send().await {
            Ok(r)  => ok_result(r.text().await.unwrap_or_default()),
            Err(e) => err_result(format!("bridge request failed: {e}")),
        }
    }
);

// ── PhoneA11yExecutePlan ──────────────────────────────────────────────────────

pub struct PhoneA11yExecutePlan {
    bridge_url:   String,
    secret:       String,
}

impl PhoneA11yExecutePlan {
    pub fn new(bridge_url: String, secret: String, _timeout_secs: u64) -> Self {
        Self { bridge_url, secret }
    }
}

#[async_trait::async_trait]
impl crate::tools::traits::Tool for PhoneA11yExecutePlan {
    fn name(&self) -> &str { "phone_a11y_execute_plan" }

    fn description(&self) -> &str {
        "Execute a multi-step UI automation plan atomically on the device without \
         round-tripping the LLM between steps. Up to 50 steps run sequentially; \
         if stop_on_failure is true (default), execution halts on the first failed step. \
         \n\
         Step types and their fields:\n\
         - wait_for:    view_id|text|content_desc|class_name, timeout_ms (def 5000), tap (def false)\n\
         - scroll_find: view_id|text|content_desc|class_name, direction (down), max_scrolls (10), \
           container_view_id, wait_after_ms (400), tap (def true)\n\
         - tap_text:    text (required), exact (def false), timeout_ms (def 3000)\n\
         - tap:         x (required), y (required)\n\
         - action:      view_id (required), action (required: click|long_click|scroll_forward|...), text\n\
         - type:        text (required), view_id (optional)\n\
         - global:      action (required: back|home|recents|notifications|quick_settings|power_dialog)\n\
         - swipe:       x1, y1, x2, y2 (all required), duration_ms (def 300)\n\
         - launch:      package (required — use phone_app_list to find package names)\n\
         - screenshot:  (no fields — captures screenshot; detail contains base64 on success)\n\
         - sleep:       ms (50–5000, def 500)\n\
         \n\
         Returns { steps_run, steps_succeeded, all_succeeded, failed_step?, results: [{step, type, success, detail}] }. \
         Full build only."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "required": ["steps"],
            "properties": {
                "steps": {
                    "type": "array",
                    "description": "Ordered list of steps to execute (max 50)",
                    "maxItems": 50,
                    "items": {
                        "type": "object",
                        "required": ["type"],
                        "properties": {
                            "type":              { "type": "string",  "description": "Step type: wait_for | scroll_find | tap_text | tap | action | type | global | swipe | launch | screenshot | sleep" },
                            "view_id":           { "type": "string",  "description": "Android resource ID" },
                            "text":              { "type": "string",  "description": "Text to match or type" },
                            "content_desc":      { "type": "string",  "description": "Content description to match" },
                            "class_name":        { "type": "string",  "description": "Class name to match" },
                            "exact":             { "type": "boolean", "description": "Exact text match" },
                            "exact_text":        { "type": "boolean", "description": "Exact text match (wait_for/scroll_find)" },
                            "timeout_ms":        { "type": "integer", "description": "Timeout in ms" },
                            "tap":               { "type": "boolean", "description": "Tap on find (wait_for/scroll_find)" },
                            "direction":         { "type": "string",  "description": "Scroll direction: up | down | left | right" },
                            "max_scrolls":       { "type": "integer", "description": "Max scroll steps" },
                            "container_view_id": { "type": "string",  "description": "Scrollable container resource ID" },
                            "wait_after_ms":     { "type": "integer", "description": "Pause after each scroll (ms)" },
                            "x":                 { "type": "integer", "description": "Tap X coordinate" },
                            "y":                 { "type": "integer", "description": "Tap Y coordinate" },
                            "action":            { "type": "string",  "description": "Accessibility action name" },
                            "x1":                { "type": "integer", "description": "Swipe start X" },
                            "y1":                { "type": "integer", "description": "Swipe start Y" },
                            "x2":                { "type": "integer", "description": "Swipe end X" },
                            "y2":                { "type": "integer", "description": "Swipe end Y" },
                            "duration_ms":       { "type": "integer", "description": "Swipe duration in ms" },
                            "package":           { "type": "string",  "description": "App package name to launch" },
                            "ms":                { "type": "integer", "description": "Sleep duration in ms (50–5000)" }
                        }
                    }
                },
                "stop_on_failure": {
                    "type": "boolean",
                    "description": "Stop execution on first failed step (default true)",
                    "default": true
                }
            }
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let steps = args["steps"].as_array().map(|a| a.len()).unwrap_or(0);
        if steps == 0 {
            return err_result("steps array is required and must not be empty");
        }
        if steps > 50 {
            return err_result("steps array exceeds maximum of 50");
        }
        // Plans can include sleep steps and wait_for loops — use a generous timeout.
        let req = reqwest::Client::builder()
            .timeout(Duration::from_secs(120))
            .build()
            .expect("reqwest client build failed")
            .post(format!("{}/phone/a11y/execute_plan", self.bridge_url))
            .header("X-Bridge-Token", &self.secret)
            .json(&args);
        match req.send().await {
            Ok(r)  => ok_result(r.text().await.unwrap_or_default()),
            Err(e) => err_result(format!("bridge request failed: {e}")),
        }
    }
}
