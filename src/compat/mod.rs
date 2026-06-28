//! Claude Code `stream-json` compatibility layer.
//!
//! Maps pixie-pi's [`AgentEvent`](crate::agent::context::AgentEvent) stream to
//! the NDJSON wire format Claude Code emits under `--output-format stream-json`,
//! so callers that spawn the `claude` CLI (e.g. [Pixie]) can drive pixie-pi as a
//! drop-in replacement.
//!
//! The mapper is intentionally **pure** (no I/O): [`map_events`] turns a single
//! event into zero or more [`ClaudeLine`]s, and [`to_ndjson`] serializes one.
//! SDK users and unit tests reuse it directly. The binary's
//! `modes::stream_json` driver is what wires stdin/stdout around it.
//!
//! [Pixie]: https://github.com/white1or1black/pixie

pub mod claude;

pub use claude::{map_events, system_init, to_ndjson, ClaudeLine, MapCtx};
