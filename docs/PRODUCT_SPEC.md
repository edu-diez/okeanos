# PRODUCT_SPEC.md — Rust CLI TUI Agent Harness

**Status:** Phase 3 — product definition (docs only, no implementation code)  
**Owner:** Workflow manager / product  
**Related:** DESIGN.md (visual strategy), Phase 2 tech-selection research

---

## Vision

A single static Rust binary that turns a language model into an autonomous coding agent. It drives model and tool calls, manages conversation state and context, applies approval policies, and keeps the agent progressing through multistep tasks — with a first-class terminal TUI and an identical headless/RPC mode, safe by default.

## User stories

1. **Developer starts a coding task.** I run `okeanos "refactor the auth module"` and the agent plans with the todo tool, calls tools, asks for approval before writes, and reports progress in the TUI until the task is done or blocked.
2. **Developer reviews risky actions.** Before any file write, delete, or network call, the agent pauses with a centered approval modal (TUI) or emits an `approval_request` JSON event (headless/RPC). I can allow, deny, or allow-always for the session.
3. **Developer works headless / via RPC.** I run the binary with no TUI (CI, SSH, automation) and consume the same JSON event stream over stdio or JSON-RPC. Screen readers and scripts use the canonical JSON UI.
4. **Developer recovers from mistakes.** `/undo` rolls the last git snapshot back; `/redo` re-applies it. I can inspect the JSONL session log at any time.
5. **Developer operates offline-ish / constrained.** No context compaction at MVP — the agent hits a hard context cap and errors clearly instead of silently truncating.
6. **Developer extends the agent.** New tools are added by implementing the `Tool` trait; new model providers come behind feature flags; lazy MCP-style server handling is deferred.

## Requirements

### Constraints

- Single static Rust binary (musl-friendly, minimal libc, no FFI where avoidable).
- TUI + headless/RPC from the same binary; one code path for state, two renderers.
- Model-agnostic provider abstraction; OpenAI-compatible via OpenRouter first; other providers behind feature flags.
- JSON-schema function calling through a `Tool` trait.
- No compaction at MVP: hard context cap with a clear error.
- Safe by default: allow-list + confirm-before-write/delete/network; local process execution is the permission boundary.
- Session persistence: JSONL append-only session log + git-based snapshots with `/undo`/`/redo`.
- `tracing` with JSON structured logging.
- ReAct loop as the core control flow.
- `todo` tool (add/complete/list) as the first planning primitive.
- Unit tests for harness logic with a mock provider.

### Functional requirements

| ID | Requirement |
|----|-------------|
| FR-01 | CLI: `okeanos <prompt>`, `okeanos --interactive`, `okeanos --headless`, `okeanos --rpc`, plus `/undo`, `/redo`, `/help`, `/todos`, `/logs` commands. |
| FR-02 | ReAct loop: think → call tool → observe → repeat until task complete, blocked, or context cap reached. |
| FR-03 | Model abstraction: `Model` trait with chat-completion + streaming; OpenAI-compatible/OpenRouter provider at launch; additional providers feature-gated. |
| FR-04 | Tool system: `Tool` trait with JSON-schema input/output; built-in `todo`, `read`, `write`, `delete`, `run` (local process), `grep`; user-defined tools via the trait. |
| FR-05 | Approval policy: safe-by-default allow-list; confirm before write/delete/network; allow/deny/allow-always per session. |
| FR-06 | Context management: hard token/message cap; clear error when exceeded (no silent compaction/truncation). |
| FR-07 | Resilience: error-in-context with bounded retries (2–3) on tool errors; exponential backoff optional. |
| FR-08 | Session log: append-only JSONL of every message, tool call, result, approval, and snapshot event. |
| FR-09 | Git snapshots: snapshot before destructive actions and on `/undo`/`/redo` boundaries; gix-based, no libgit2 FFI. |
| FR-10 | TUI: conversation pane, streaming token render, tool-call panel, approval modal, todo panel, status bar, command palette, log tail; responsive at 80×24 minimum / 120×40 comfortable. |
| FR-11 | Headless/RPC: identical JSON event stream to TUI (per DESIGN.md §7) over stdout or JSON-RPC (stdio/HTTP, feature-gated). |
| FR-12 | Logging: `tracing` JSON structured logs to file/stdout; levels filterable via `RUST_LOG`. |
| FR-13 | Extensibility: `Tool` trait implementations register new tools; lazy MCP server handling deferred behind a feature flag. |
| FR-14 | Static system prompt + optional `skills/` directory concatenated into context. |
| FR-15 | Configuration: XDG global config (`~/.config/okeanos/config.toml`) + optional project-local config (`okeanos.toml` walked up from git root). Env var and file substitution for secrets (`{env:VAR}`, `{file:path}`). Secrets never committed to config. |
| FR-16 | Token counting & cost estimation: conservative approximate token count enforced against hard context cap; per-model cost estimate shown in status bar; counts are estimates, not provider-authoritative. |
| FR-17 | Session resume: `--resume <session-id>` continues a prior session; sessions stored as JSONL per project under the okeanos home dir; header line records cwd + format version for migration. |
| FR-18 | Tool output truncation: max bytes per tool result; head/tail truncation with overflow indicator; truncated output counts against the context cap. |
| FR-19 | Working directory & project scope: CWD by default; `--root` flag overrides; git root auto-detected; `okeanos.toml` project config; snapshots scoped to root. |
| FR-20 | Loop termination: max iterations, max wall-clock time, hard context cap, and user interrupt (SIGINT) all stop the ReAct loop with a clear, non-panicking error. |
| FR-21 | Rate limiting: bounded retries (2–3) with exponential backoff on provider 429/503; provider errors distinguished from permanent tool errors; retry events logged. |
| FR-22 | Graceful shutdown: SIGINT/SIGTERM flush the JSONL session log, teardown the TUI alternate screen, wait for in-flight work to drain, then escalate SIGTERM→SIGKILL after a timeout. |
| FR-23 | Model capability negotiation: each `Model` instance carries `ModelCapabilities` (tool_calling, structured_output, thinking, max_context, max_output, temperature_supported). Capabilities come from provider discovery where possible, with explicit TOML override for local/self-hosted models. Fail closed on missing required capability. |
| FR-24 | Text-based tool calling: the `Model` trait primitive is text-in/text-out. Models without native function calling get a text-based tool schema injected into the system prompt (XML/key-value format); the harness parses raw output into tool invocations. Frontier models with native JSON tool calling use the fast path. |
| FR-25 | Tiered system prompt & tool set by model size: small local models (4k–8k context, no native FC) get a short system prompt (<1k tokens) and a minimal tool set (read, edit, run); frontier models get the full prompt and full tool set. |
| FR-26 | Selective context inclusion: only files relevant to the current task are included (symbol scanning via ripgrep, pure Rust — no C dependencies); history turns capped per model tier; repo-map-lite deferred; summarization deferred (no compaction at MVP). |
| FR-27 | Tiered generation parameters: temperature, max_tokens, stop_sequences defaults set by model-size tier (small: temp 0.2–0.3, low max_tokens; frontier: temp 0.7, higher max_tokens), overridable per-model in config. |
| FR-28 | Lenient parse + validate before execution: tool calls are parsed leniently (tolerate trailing text, fix common quote/bracket errors) and validated against the tool schema before execution. Invalid calls return a structured error to the model; never execute unvalidated calls. |
| FR-29 | Fallback strategies: on malformed/failed tool call, retry once with a simpler prompt and reduced tool set; if still failing, escalate to the user or skip the turn. Hard retry cap per turn. |
| FR-30 | Thinking channel: optional separate channel for reasoning/thinking tokens (supports_thinking flag); thinking captured but not replayed into context for small-context models; replayed only for large-context frontier models. |
| FR-31 | File edit format: default diff/patch for frontier models; whole-file / str_replace fallback for small local models; per-model edit-format hint in config; preflight-validate patches before applying; never silently truncate or partially apply. |
| FR-32 | Long-running / interruptible tool execution: stream `run` output incrementally; configurable per-invocation timeout; on cancel SIGTERM → short grace → SIGKILL process group; on timeout straight SIGKILL; drain output after exit; return exit_code, timed_out, cancelled in tool result. |
| FR-33 | Cost budget: opt-in hard session cost cap (USD) + per-turn soft warning threshold; when cap reached, refuse new tool calls that would exceed it, let in-flight work fail; fail closed ($0 + warning) on unknown pricing. |
| FR-34 | Context window warnings before cap: per-turn current/total tokens in status bar; clear warning when remaining drops below threshold (10–15%); error (not crash) at hard cap; no silent truncation. |
| FR-35 | Tool result deduplication: none at MVP; repeated calls always re-execute. Revisit post-MVP with TTL + exclusion of mutating tools. |
| FR-36 | Sub-agent delegation: `todo` tool extended with `delegate` action to spawn isolated sub-agents; each sub-agent runs a full ReAct loop with its own context, tool subset (per-profile allowlist/denylist), token budget, and delegation depth limit. Parent aggregates results; approval requests bubble up to parent for decision. |
| FR-37 | Agent profiles: declarative TOML profiles in `.okeanos/agents/*.toml` (project) and `~/.config/okeanos/agents/*.toml` (user), with project overriding user. Profile defines: description, model (inherit or override), tool allowlist/denylist, max delegation depth, token budget, workspace mode (inherit | branch | share). |
| FR-38 | Sub-agent execution: async background execution with periodic progress events (`SubAgentProgress`) emitted on parent's event stream; parent continues working. On completion, `SubAgentComplete` event delivers structured result; parent's todo item is marked complete. |
| FR-39 | Approval bubbling: sub-agent `PolicyDecision::RequireApproval` emits `ApprovalRequest` on parent's event channel; parent UI/RPC handles approval; decision routed back to sub-agent's approval channel to resume execution. |
| FR-40 | Delegation limits: configurable max delegation depth (default 1, enforced at spawn); global spawn budget per session (default 64, like OpenCode/Pi); per-sub-agent token budget enforced via `CostTracker` child budget. |
| FR-41 | Workspace isolation modes: `inherit` (shared workspace, default M27), `branch` (isolated git worktree via gix, optional), `share` (shared directory storage). `branch` mode creates a temporary worktree for the sub-agent's lifetime. |
| FR-42 | FleetView TUI: dedicated panel showing active sub-agents (id, profile, status, tokens used, depth), with steer/stop controls. Background sub-agents emit progress updates; parent can open a sub-agent's transcript. |
| FR-43 | Model override per agent: profile `model` field accepts `"inherit"` (parent's model) or explicit `provider/model#variant`; enables cost optimization (Haiku for exploration, Opus for reasoning). |
| FR-44 | Project instruction files: at startup, load `AGENTS.md` (fallback `CLAUDE.md`, per directory) from the user config dir (`~/.config/okeanos/`) and from each directory between the project root and CWD (root first, CWD last = highest priority). Appended after the tier prompt and skills in the system prompt, each labeled with its source path. Total size capped (default 16 KiB; 4 KiB for the small tier); truncation is explicit and names the file. Re-sent every request as part of the system prompt, so it survives compaction untouched. Configurable/disableable via `[memory]`. |
| FR-45 | Durable memory store: plain-markdown topic files plus a `MEMORY.md` index in `<project_root>/.okeanos/memory/` (project scope) and `~/.config/okeanos/memory/` (user scope). The index (capped at 200 lines / 8 KiB) is loaded into the system prompt at startup; topic files are read on demand with `read`. Loaded once per session so the prompt prefix stays byte-stable (provider prompt caching). Not loaded for the small tier. |
| FR-46 | `remember` tool (mid/frontier tiers): writes a topic file and updates the `MEMORY.md` index for `project` or `user` scope. Write-category: passes through `Policy::check` and requires approval. Names are slugged (`[a-z0-9_-]`, ≤64 chars) so writes can never escape the memory directory. A memory written this session takes effect from the next session. Sub-agent inheritance of instructions/memory is deferred. |
### Non-functional requirements

| ID | Requirement |
|----|-------------|
| NF-01 | Single static binary; builds with `cargo build --release` (glibc) and `--target x86_64-unknown-linux-musl` (musl). |
| NF-02 | No C dependencies in the primary dependency tree (rules out git2/openssl; uses gix + rustls). |
| NF-03 | TUI starts and renders the first frame in < 200 ms on a modern terminal. |
| NF-04 | Memory bounded by the hard context cap; no unbounded growth. |
| NF-05 | Deterministic unit-testable harness logic with a mock provider (no network in unit tests). |
| NF-06 | All user-facing strings externalized for review; copy is fixed after design sign-off (see AGENTS.md). |
| NF-07 | Visual design per DESIGN.md is integral, not a post-launch enhancement; no visual rework after Phase 2 implementation. |
| NF-08 | Secrets never stored in plaintext in config files committed to version control; env var / file reference substitution required. |
| NF-09 | Session log and TUI state must survive SIGINT/SIGTERM via graceful flush; no data loss on clean shutdown. |
| NF-10 | Token counts and cost estimates are clearly labeled as estimates; provider-authoritative billing is the user's responsibility. |
| NF-11 | Model-agnostic by design: the `Model` trait never assumes native function calling; a text-based fallback path exists for models without structured tool output. |
| NF-12 | Small-model friendly: minimal prompt (<1k tokens) and minimal tool set (read, edit, run) work with 4k–8k context models without native function calling. |
| NF-13 | No C dependencies for context management: selective file inclusion and symbol scanning use pure Rust (ripgrep), not tree-sitter or other C libraries. |
| NF-14 | Tool results are never cached at MVP; every call re-executes, guaranteeing fresh output and avoiding staleness/invalidation complexity. |
| NF-15 | Long-running tool execution is interruptible: cancellation drains output and returns a clear result (cancelled/timed_out/exit_code) rather than hanging or dropping state. |
| NF-16 | Cost and context warnings are surfaced proactively (status bar, JSON events), not just at the hard cap. |

## Success criteria

- Agent completes a multistep coding task (plan → tool calls → edits → verify) end-to-end with approvals, in TUI and headless modes.
- `/undo` and `/redo` correctly restore prior git snapshots.
- Hard context cap triggers a clear, actionable error instead of silent truncation.
- Unit tests cover ReAct loop, approval policy, context cap, and git snapshot logic with a mock provider — no network.
- `cargo build --release` produces a single static binary; `cargo test` passes.

## MVP definition

**In scope:**
- ReAct loop with `Model` trait + OpenAI-compatible/OpenRouter provider.
- Model capability negotiation (`ModelCapabilities`) + explicit TOML override for local models.
- Text-based tool calling fallback for models without native function calling.
- Built-in tools: `todo` (add/complete/list), `read`, `write`, `delete`, `run` (bounded local process), `grep`.
- Approval policy with allow-list + confirm-before-write/delete/network.
- Hard context cap with clear error.
- Tiered system prompt + minimal tool set for small models; full set for frontier models.
- Selective context inclusion (pure Rust, no C deps); capped history turns per tier.
- Tiered generation parameters by model size.
- Lenient parse + validate-before-execution for tool calls; fallback strategies on malformed calls.
- Optional thinking channel.
- Diff/patch edit format with preflight validation; whole-file fallback for small models.
- Streamed, interruptible `run` tool execution with configurable timeout.
- Context window warnings before the hard cap.
- Opt-in hard session cost cap (USD).
- JSONL append-only session log.
- Git snapshots + `/undo`/`/redo` via gix.
- TUI (per DESIGN.md) and headless/RPC JSON parity.
- `tracing` JSON logging.
- Static system prompt + `skills/` concatenation.
- Unit tests with mock provider.

**Out of scope (MVP):**
- Context compaction/summarization.
- Lazy MCP client (feature-gated, deferred).
- Other model providers beyond OpenAI-compatible (feature-gated).
- WebSocket RPC (feature-gated).
- Compaction-aware long-context handling.
- Compressed/rotated session logs (raw JSONL only at MVP).
- Tree-sitter or other C-based symbol scanners for context inclusion.
- Tool result caching / deduplication at MVP (revisit post-MVP).
- Per-turn user confirmation for expensive tool calls (post-MVP).
- Role-based crew orchestration (oh-my-opencode-slim style) — deferred until sub-agent delegation proves the primitive.

## Deferred features

- Context compaction (post-MVP; hard cap is the intentional MVP behavior).
- Role-based crew orchestration (oh-my-opencode-slim style) — deferred until sub-agent delegation proves the primitive. Crews are composition (prompts + workflows using sub-agents), not infrastructure.
- MCP client — lazy MCP server handling behind a feature flag, post-MVP.
- Additional providers (Anthropic, Azure, Vertex, etc.) — behind `Model` trait feature flags.
- WebSocket transport for RPC — stdio JSON-RPC first.
- Rich text / markdown rendering in TUI — plain mono-data at MVP.
- Autonomous agent swarms / cross-project delegation — explicitly out of scope for M27.
