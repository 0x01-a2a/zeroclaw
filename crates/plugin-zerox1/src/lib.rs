//! ZeroX1 mesh network client for ZeroClaw.
//!
//! Provides [`Zerox1Client`] for talking to the zerox1-node REST API and
//! types for the inbound WebSocket envelope stream.  This crate contains no
//! ZeroClaw trait implementations; those live in the main `zeroclaw` crate
//! under `src/channels/zerox1.rs` and `src/tools/zerox1.rs`.

pub mod client;
pub mod types;

pub use client::Zerox1Client;
pub use types::{InboundEnvelope, SendEnvelopeRequest, SendEnvelopeResponse};
