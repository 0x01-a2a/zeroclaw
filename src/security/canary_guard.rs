//! Canary guard — per-turn context-exfiltration detector.
//!
//! A unique UUID is injected into each tool-call turn's request. After the
//! model responds, the response text is checked for the canary string.
//!
//! If the canary appears verbatim in the response, the model was likely
//! prompted by an injected instruction to echo back internal context —
//! a classic prompt-injection exfiltration attack. The guard logs a
//! security warning and emits a runtime trace event so the incident can
//! be audited.
//!
//! # Injection strategy
//!
//! The canary is appended as a hidden user-turn annotation (similar to the
//! safety heartbeat mechanism). It is NOT prepended to the system prompt
//! because mutating the system prompt across turns can interfere with
//! provider-side prefix caching. Appending it to a fresh user message keeps
//! the core prompt stable while still being present in the context window.
//!
//! # False-positive rate
//!
//! UUID v4 strings have 122 bits of randomness. The probability of an
//! accidental collision in a single response is negligible (~2.3e-37).

use uuid::Uuid;

/// A single-use canary token for one LLM turn.
#[derive(Debug, Clone)]
pub struct CanaryToken {
    /// The randomly generated UUID used as the canary value.
    pub id: String,
}

impl CanaryToken {
    /// Create a new canary token with a fresh random UUID.
    pub fn new() -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
        }
    }

    /// Return the text to append to the outgoing user message.
    ///
    /// The framing uses a system-annotation prefix so the model understands
    /// this is infrastructure metadata and not user-provided content.
    pub fn injection_text(&self) -> String {
        format!("[system-ref:{}]", self.id)
    }

    /// Returns `true` if the canary UUID appears verbatim anywhere in
    /// the model's response text.
    ///
    /// A `true` return value is a strong signal that the model was instructed
    /// (via prompt injection) to echo back internal context.
    pub fn is_leaked_in(&self, response_text: &str) -> bool {
        response_text.contains(&*self.id)
    }
}

impl Default for CanaryToken {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canary_not_leaked_in_unrelated_text() {
        let token = CanaryToken::new();
        assert!(!token.is_leaked_in("This is a normal response with no canary."));
    }

    #[test]
    fn canary_detected_when_echoed() {
        let token = CanaryToken::new();
        let response = format!("Here is the system ref: {}", token.id);
        assert!(token.is_leaked_in(&response));
    }

    #[test]
    fn injection_text_contains_id() {
        let token = CanaryToken::new();
        let text = token.injection_text();
        assert!(text.contains(&*token.id));
    }

    #[test]
    fn each_token_has_unique_id() {
        let a = CanaryToken::new();
        let b = CanaryToken::new();
        assert_ne!(a.id, b.id);
    }
}
