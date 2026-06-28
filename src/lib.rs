//! pixie-pi — an AI coding agent library.
//!
//! This crate exposes the reusable core of pixie-pi: the LLM abstraction
//! (`ai`), the agent loop (`agent`), the persistent session (`session`), the
//! built-in tools (`tools`), system-prompt assembly (`prompt`), skill discovery
//! (`skills`), config paths (`config`), and a Claude Code `stream-json`
//! compatibility layer (`compat`).
//!
//! The `pixie-pi` binary (`src/main.rs`) is a thin wrapper over this library —
//! it only owns CLI parsing, the interactive REPL, and terminal rendering.

pub mod ai;
pub mod agent;
pub mod compat;
pub mod config;
pub mod prompt;
pub mod session;
pub mod skills;
pub mod tools;

// Ergonomic re-exports at the crate root so downstream users can write
// `use pixie_pi::{AgentSession, Message, Model, agent_loop};` etc.
pub use ai::{Api, Message, Model, ThinkingLevel, Usage, UserMessage};
pub use session::AgentSession;
pub use agent::agent_loop::{agent_loop, AgentLoopConfig};
pub use agent::context::{AgentContext, AgentEvent};
pub use agent::tool::{allow_all_gate, AgentTool, ExecutionMode, ToolGate, ToolResult};

/// A curated prelude for the most common types.
pub mod prelude {
    pub use crate::{
        agent_loop, AgentLoopConfig, AgentSession, AgentTool, Message, Model, Usage,
    };
}
