//! Converge core: what the proxy and the website agree on, with no IO, no
//! clock and no randomness, so every rule here is tested in isolation.
//!
//! Translations: Responses over chat (`responses`), chat over Anthropic's
//! Messages (`anthropic`); see AGENTS.md.

pub mod anthropic;
pub mod limits;
pub mod names;
pub mod openai;
pub mod persona;
pub mod responses;
pub mod secrets;
pub mod sse;
