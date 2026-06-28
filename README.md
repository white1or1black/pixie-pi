# pixie-pi

[![CI](https://github.com/white1or1black/pixie-pi/actions/workflows/ci.yml/badge.svg)](https://github.com/white1or1black/pixie-pi/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)

> An autonomous, tool-using AI coding agent that runs in your terminal — as a
> single static Rust binary with no runtime dependencies.

`pixie-pi` reads, writes, and edits code, runs shell commands, searches your
codebase, and follows reusable **skills** — all streamed from an
Anthropic-compatible endpoint over a hand-rolled SSE decoder (no SDK). It ships
two modes: a one-shot command-line mode and an interactive REPL.

The agent core (SSE streaming, the double agent loop, tool execution, fuzzy edit
matching, LLM-summarizing compaction, skills) is a from-scratch reimplementation
written in the spirit of [`pi`](https://github.com/earendil-works/pi) — see
[Acknowledgments](#acknowledgments).

## Highlights

- **Single static binary.** No `node_modules`, no runtime, no supply-chain
  footprint. ~7 MB, cold-starts in **~3 ms** (faster than an empty `node -e ""`).
- **No Anthropic SDK.** The Messages API, SSE streaming, fine-grained block
  events, thinking signatures, and tolerant streaming-JSON repair are all
  implemented directly.
- **Seven tools** — `read`, `write`, `edit`, `bash`, `grep`, `find`, `ls` — with
  fuzzy `edit` matching, unified diffs, process-group bash kills, and
  gitignore-aware search.
- **Claude Code–compatible skills.** Drop a `SKILL.md` in `.claude/skills/` and
  the agent discovers and invokes it on demand (progressive disclosure).
- **LLM-summarizing compaction with model tiering.** Long sessions are summarized
  to fit the context window — and summaries run on the **cheap tier** (haiku) by
  default, regardless of your main model.
- **Prompt caching, adaptive/budget thinking, JSONL session persistence** with
  `--continue` / `--resume`.

> The command is `pixie-pi`. Tip: `alias pi=pixie-pi` if you prefer a shorter
> invocation.

## Requirements

- **Rust 1.82+** (to build from source).
- **An Anthropic-compatible endpoint.** Set a key for the real API, or point at a
  gateway/proxy that speaks the Anthropic Messages API.

## Install

### Build from source

```bash
git clone https://github.com/white1or1black/pixie-pi.git
cd pixie-pi
cargo build --release            # binary at target/release/pixie-pi
cp target/release/pixie-pi ~/.local/bin/pixie-pi   # optional, add to PATH
```

### Configure credentials

`pixie-pi` reads the same environment variables Claude Code / the Anthropic SDK
use:

```bash
# Pick one auth method:
export ANTHROPIC_API_KEY=sk-ant-...        # sent as x-api-key
# export ANTHROPIC_AUTH_TOKEN=...          # sent as Authorization: Bearer (takes precedence)

# Optional: point at a gateway / proxy / Anthropic-compatible endpoint:
# export ANTHROPIC_BASE_URL=https://api.anthropic.com
```

## Quick start

```bash
# One-shot (command-line mode)
pixie-pi -p "List the .rs files under src/ and summarize the architecture"

# Piped stdin is non-interactive
echo "explain this stack trace" | pixie-pi

# NDJSON event stream (pixie's own event schema, for piping into other tools)
pixie-pi --mode json -p "what tools are available?"

# Claude Code stream-json (drop-in for the `claude` CLI — see below)
pixie-pi -p "say hi" --output-format stream-json --verbose

# Interactive REPL
pixie-pi
pixie-pi "read Cargo.toml first"        # start with an initial prompt
```

Inside the REPL (`Ctrl-C` interrupts the current run, `Ctrl-D` or `/exit` quits):

```
/help           list slash commands
/model <id>     switch model (sonnet | opus | haiku)
/thinking <lvl> off|minimal|low|medium|high|xhigh
/compact        summarize old messages to fit the context
/tools          list enabled tools
/context        show estimated token usage
/cost           show cumulative token usage + cost
/system         show the system prompt
/clear          clear the conversation
/exit           quit
```

Conversations persist per-project as JSONL under `$PIXIE_PI_AGENT_DIR` (default
`~/.config/pixie-pi/agent/sessions/<hash>/session.jsonl`) and resume with
`pixie-pi -c` (`--continue`) or `pixie-pi -r` (`--resume`).

## Tools

| Tool | Default | Description |
|------|:-------:|-------------|
| `read`  | on  | Read a file (text or image), with `offset`/`limit` paging |
| `write` | on  | Create or overwrite a file (creates parent dirs) |
| `edit`  | on  | Exact **and fuzzy** text replacement, multiple per call, with a unified diff |
| `bash`  | on  | Run a shell command (timeout, process-tree kill, tail-truncated output) |
| `grep`  | off | Content search (regex/literal, context lines, respects `.gitignore`) |
| `find`  | off | File glob search (respects `.gitignore`) |
| `ls`    | off | Directory listing |

`grep`/`find`/`ls` are off by default — the model uses `bash` for searching
unless you enable them:

```bash
pixie-pi --tools read,edit,bash,grep,find,ls -p "review the code in src/"   # read+search set
pixie-pi --exclude-tools bash -p "..."          # deny a tool
pixie-pi --no-tools -p "..."                    # no tools at all
```

The `edit` tool accepts slightly-imprecise `oldText`: differences in trailing
whitespace, smart quotes, dashes, and special spaces (NFKC-folded) still match,
so the model rarely fails an edit over cosmetic whitespace.

## Skills (Claude Code–compatible)

A skill is a directory `<root>/<name>/SKILL.md`, where `<root>` is
`.claude/skills` (project) or `~/.claude/skills` (user). Project skills shadow
user skills of the same name. The `SKILL.md` has a small YAML frontmatter
(`name`, `description`) followed by markdown instructions:

```markdown
---
name: deploy
description: Use when the user wants to deploy or ship the project
---
# Deploy
1. Read the release notes.
2. Run `cargo build --release`.
3. Report the artifact path.
```

Only the name + description are surfaced up front; the body is loaded into
context only when the model invokes the `skill` tool (progressive disclosure).
Supporting files referenced by a skill live under its directory and are read with
the `read` tool. All registered tools remain callable while a skill runs (no
allowlist enforcement).

## Context management

- **Token estimate & budgeting.** Each turn's usage is accumulated; the REPL
  shows context % and running cost after every turn.
- **Auto-compaction.** When the transcript nears the context limit (~80%),
  `pixie-pi` summarizes the oldest messages (cut at a user-message boundary, so
  tool chains stay intact) and keeps the recent tail. It falls back to a plain
  drop if the summarizer is unavailable, so it never hard-fails.
- **Model tiering.** Summaries run on the **cheapest tier** by default (`haiku`),
  independent of your main model — so an `opus` session still compresses cheaply.
  Override with `PIXIE_PI_COMPACT_MODEL=<sonnet|opus|haiku|…>`.

## Configuration

### Environment variables

| Variable | Purpose |
|----------|---------|
| `ANTHROPIC_API_KEY` | API key (sent as `x-api-key`) |
| `ANTHROPIC_AUTH_TOKEN` | Bearer token (takes precedence over the API key) |
| `ANTHROPIC_BASE_URL` | Anthropic-compatible endpoint (default `https://api.anthropic.com`) |
| `PIXIE_PI_COMPACT_MODEL` | Model used for compaction summaries (default: cheapest tier) |
| `PIXIE_PI_AGENT_DIR` | Session/config directory (default `~/.config/pixie-pi/agent`) |
| `NO_COLOR` | Disable ANSI colors |

### CLI reference

```
pixie-pi [OPTIONS] [MESSAGE]...

Options:
  -p, --print                 Non-interactive one-shot run
      --mode <text|json>      Output mode for print runs (default: text)
      --output-format <text|stream-json>  Claude Code stream-json NDJSON (drop-in for `claude`)
      --input-format <text|stream-json>   stream-json ⇒ persistent stdin multi-turn driver
      --session-id <id>       Claude-compatible session id
      --permission-mode <mode>  Accepted; resolves to allow-all (bypass)
      --dangerously-skip-permissions  Skip permission prompts (bypass)
      --model <id>            sonnet | opus | haiku (fuzzy-matched)
      --provider <name>       (default: anthropic)
      --api-key <key>         (default: $ANTHROPIC_API_KEY)
      --system-prompt <text>  Replace the system prompt entirely
      --append-system-prompt <text>   Append (repeatable)
      --thinking <level>      off|minimal|low|medium|high|xhigh
      --max-tokens <n>        Override the model output cap
      --no-cache              Disable Anthropic prompt caching
  -c, --continue              Continue the most recent session
  -r, --resume                Resume a session
      --session <path|id>     Use a specific session
      --no-session            Don't persist (ephemeral)
  -t, --tools <a,b,c>         Enable only these tools
      --exclude-tools <a,b>   Disable these tools
  -n, --no-tools              Disable all tools
      --no-builtin-tools      Disable built-in tools
      --verbose               Stream model thinking
  -h, --help
  -V, --version
```

`@file` arguments are inlined into the prompt: `pixie-pi @notes.md "summarize this"`.

## Claude Code `stream-json` compatibility

pixie-pi can be spawned in place of the `claude` CLI: it speaks Claude Code's
`stream-json` wire protocol, emitting `system`/`assistant`/`user`/`result`
NDJSON lines that callers (e.g. [Pixie](https://github.com/white1or1black/pixie))
read off stdout until a `result` line ends each turn.

```bash
# One shot: one turn of stream-json output
pixie-pi -p "say hi" --output-format stream-json --verbose

# Persistent: multi-turn over one process — read {"type":"user",...} JSONL
# turns from stdin, emit a result line per turn, exit on stdin EOF
pixie-pi --print --output-format stream-json --verbose \
         --input-format stream-json --permission-mode bypassPermissions \
         --session-id <id>
```

The mapper is a **pure** library function (`pixie_pi::compat::map_events`) with
no I/O, so SDK consumers can reuse it directly. Permission brokering is
**bypass-only** by design (matches the daily-driver path): every
`--permission-mode` and `--dangerously-skip-permissions` resolves to allow-all.
Unknown permission modes are accepted and treated as bypass rather than
rejected, so a caller's argv never errors. Interactive permission brokering is a
documented future extension — its hook point is already isolated at the
`ToolGate` in the agent loop.

## Architecture

| Layer | Modules |
|-------|---------|
| AI / provider | `src/ai/` — `types`, `stream` (SSE + tolerant streaming-JSON), `anthropic` |
| Agent loop | `src/agent/` — `agent_loop` (double loop), `context` (events), `tool` (trait + gate) |
| Tools | `src/tools/` — `read`/`write`/`edit`/`bash`/`grep`/`find`/`ls`/`skill` + `truncate`/`edit_diff`/`util` |
| Session / skills / prompt | `src/session.rs`, `src/skills.rs`, `src/prompt.rs`, `src/config.rs` |
| Claude `stream-json` compat | `src/compat/` — pure `AgentEvent` → Claude NDJSON mapper (`map_events`, `to_ndjson`) |
| Library crate | `src/lib.rs` — public core + re-exports + `prelude` (the bin is a thin wrapper) |
| Entry / modes | `src/main.rs`, `src/cli.rs`, `src/app.rs`, `src/modes/` (`print`, `interactive`, `stream_json`), `src/render.rs` |

The agent loop streams a response, executes tool calls (parallel by default,
sequential when a tool requests it), feeds results back, and repeats until the
model stops; abort propagates through a `CancellationToken`; prompt caching
attaches `cache_control: ephemeral` to the system prompt, last user message, and
final tool definition; adaptive-effort thinking for new models, budget-based for
older ones.

## Performance

Because the harness is a compiled binary with no runtime, the non-LLM paths are
essentially free — so `pixie-pi` is well-suited to scripting, piping, and
resource-constrained setups (measured on macOS, release build):

| | pixie-pi | Node `node -e ""` (floor) |
|---|---|---|
| Cold start (full app loaded) | **~2.8 ms** | ~8 ms (empty, no modules) |
| Diff: 1-line edit in a 2000-line file | ~80 µs | — |
| Binary size | ~7 MB | `node_modules` (hundreds of MB) |

A real conversation is dominated by LLM latency, so this matters most for
frequent CLI invocations, low-memory containers, and cold-start-sensitive
environments — not for interactive chat responsiveness.

## Development

```bash
cargo test                              # ~66 unit tests: SSE, streaming-JSON, edit/diff, truncation, skills, compaction
cargo clippy --all-targets -- -D warnings
cargo build --release
```

The codebase is heavily commented: every non-trivial function carries a rationale
note, and each correctness-sensitive path has a guarding test.

## License

MIT © white1or1black. See [LICENSE](LICENSE).

## Acknowledgments

The agent core's algorithms — fuzzy edit matching, compaction, streaming-JSON
repair, truncation, and the overall agent-loop design — are from-scratch
reimplementations of those in [`pi`](https://github.com/earendil-works/pi) by
Mario Zechner (earendil-works). This project is an independent reimplementation
written from scratch in a different language; it is not affiliated with, or
endorsed by, pi. pi's MIT license and copyright are reproduced in
[THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).
