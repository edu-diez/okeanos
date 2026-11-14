# MILESTONES.md — Development Roadmap

**Status:** Phase 5 — roadmap; Milestone 0 complete & tagged (v0.1.0-milestone-0), Milestone 0.5 complete & tagged (v0.1.0-milestone-0.5), Milestone 1 complete & tagged (v0.1.0-milestone-1), Milestone 4 complete & tagged (v0.1.0-milestone-4), Milestone 5 complete & tagged (v0.1.0-milestone-5), Milestone 6 complete & tagged (v0.1.0-milestone-6), Milestone 7 complete & tagged (v0.1.0-milestone-7), Milestone 8 complete & tagged (v0.1.0-milestone-8), Milestone 9 complete & tagged (v0.1.0-milestone-9), Milestone 10 complete & tagged (v0.1.0-milestone-10)
**Owner:** Workflow manager
**Related:** PRODUCT_SPEC.md, ARCHITECTURE.md, DESIGN.md

Strategy: **small iterative milestones**. Each milestone is 1–2 sessions, introduces at most 2–3 subsystems, and produces a single testable deliverable. **No milestone starts until the previous one is tagged and CI-green.** Every feature from PRODUCT_SPEC.md is included — none deferred to post-MVP.

---

## Milestone 0 — Scaffolding & CLI

**Status:** complete — tagged v0.1.0-milestone-0  

**Scope:**
- Rust workspace, `Cargo.toml` with Phase 2 stack (tokio `current_thread`, reqwest+rustls, clap derive, thiserror/anyhow, serde_json, tracing JSON).
- CLI entry: `okeanos <prompt>`, `--interactive`, `--headless` flags; `Mode` enum.
- Error types: `thiserror` for lib, `anyhow` for bin/ReAct loop with `.context()`.
- CI: `cargo fmt --check`, `cargo clippy -- -D warnings`, `cargo test`, musl static build (`--target x86_64-unknown-linux-musl`).

**Out of scope:** Config file loading, model providers, tools, ReAct loop, TUI, git, RPC.

**Acceptance criteria:**
- `cargo build --release` produces a single static binary.
- `okeanos --help` and `okeanos --version` work.
- CI green on fmt, clippy, test, musl build.

**Testing requirements:** None beyond CI checks (no harness logic yet).

**Definition of Done:** CI green; binary builds statically; milestone tagged.

---

## Milestone 0.5 — CLI UX Essentials

**Status:** complete — tagged v0.1.0-milestone-0.5

**Scope:**
- Shell completions: `--generate-completions bash|zsh|fish|power-shell|elvish`.
- Verbose/quiet flags: `-v` (info), `-vv` (debug), `-q` (warn only) controlling `tracing` filter.
- Dry-run mode: `--dry-run` shows execution plan (tools that would run, files that would change) without executing.
- Model override: `--model <name>` and `--provider <base_url>` flags overriding config.
- Generation parameter overrides: `--temperature <f32>`, `--max-tokens <usize>` CLI flags.
- Project initialization: `okeanos init` creates `okeanos.toml` with sensible defaults (model, tier, permissions) at git root or CWD.
- Config validation: `okeanos config validate` checks TOML syntax, secret references, required fields.
- Health check: `okeanos doctor` verifies API key validity (if configured), config load, dependency versions.
- Session listing: `okeanos sessions` lists resumable sessions with ID, date, project, event count.

**Out of scope:** TUI, ReAct loop, tools, git snapshots, RPC.

**Acceptance criteria:**
- `okeanos completions bash` outputs valid completion script.
- `-v`/`-vv`/`-q` adjust log output visibly.
- `--dry-run` prints planned tool calls and exits 0.
- `--model`/`--temperature` override config values in headless run.
- `okeanos init` creates valid `okeanos.toml` that loads without error.
- `okeanos config validate` catches invalid TOML and missing secrets.
- `okeanos doctor` reports ✓/✗ for API key, config, deps.
- `okeanos sessions` shows table with ID, started-at, project, events.

**Testing requirements:** Unit tests for CLI arg parsing, dry-run plan generation, config validation logic. Integration test for `init` + `config validate` round-trip.

**Definition of Done:** CI green; all new CLI flags functional; milestone tagged. ✓
- `cargo fmt --check` passes
- `cargo clippy -- -D warnings` passes
- `cargo test` passes (184 unit tests)
- `cargo build --release` produces a single binary
- Shell completions generate valid scripts for bash, zsh, fish, power-shell, elvish
- `-v`/`-vv`/`-q` adjust tracing filter (warn/info/debug)
- `--dry-run` prints execution plan and exits 0
- `--model`/`--provider`/`--temperature`/`--max-tokens` override config values in headless run
- `okeanos init` creates valid `okeanos.toml` that loads without error
- `okeanos config validate` catches invalid TOML and missing secrets
- `okeanos doctor` reports ✓/✗ for API key, config, deps
- `okeanos sessions` shows table with ID, started-at, project, events
- Tagged v0.1.0-milestone-0.5

---

## Milestone 1 — Config & secrets

**Status:** complete — tagged v0.1.0-milestone-1

**Scope:**
- XDG global config (`~/.config/okeanos/config.toml`) + optional project-local config (`okeanos.toml` walked up from git root).
- TOML parsing via `toml` crate (CLI/config layer only, never in harness `lib`).
- Secret substitution: `{env:VAR}` and `{file:path}` in config values.
- Config load fails closed if a required secret is missing.
- Config validation: invalid TOML fails CI.

**Out of scope:** Model providers, tools, ReAct loop, TUI, git, RPC, session log.

**Acceptance criteria:**
- Config loads from global layer; project layer overrides.
- `{env:VAR}` / `{file:path}` substitution works; missing secret produces a clear error.
- Invalid TOML is rejected with a clear message.

**Testing requirements:** Unit tests for config layering, secret substitution, missing-secret failure, invalid-TOML rejection. No network.

**Definition of Done:** CI green including config validation; milestone tagged. ✓
- `cargo fmt --check` passes
- `cargo clippy -- -D warnings` passes
- `cargo test` passes (13 config unit tests)
- `cargo build --release` produces a single binary
- Config loads from global + project layers with project override
- `{env:VAR}` / `{file:path}` substitution verified; missing secret fails closed with clear error
- Invalid TOML rejected with clear message
- Tagged v0.1.0-milestone-1

---

## Milestone 2 — Core harness loop

**Scope:**
- `Model` trait (text-in/text-out primitive) + `MockModel` (test-only, no network). Native JSON tool calling is optional (`supports_tool_calling` flag), not required.
- `Conversation` context: static system prompt + `skills/` directory concatenation.
- Approximate token counter + per-model cost estimate (clearly labeled as estimates).
- Hard context cap with clear, non-panicking error (no silent truncation).
- ReAct loop skeleton: think → tool → observe → stop on completion / max iterations / max wall-clock time / SIGINT.
- Working directory: CWD default, `--root` flag, git root auto-detection.
- Graceful shutdown basics: SIGINT stops the ReAct loop cleanly.

**Out of scope:** Permission layer, built-in tools beyond a stub, session log, git snapshots, TUI, RPC.

**Acceptance criteria:**
- Mock-provider ReAct loop executes a `todo` sequence end-to-end in-process.
- Hard context cap raises a clear error instead of silently truncating.
- `skills/` contents appear in context; agent references them.
- SIGINT stops the loop without panic or data corruption.

**Testing requirements:** Unit tests for ReAct loop transitions, context cap enforcement, `skills/` concatenation, token counter, mock provider edge cases (rate limit, parse error, context cap). No network.

**Definition of Done:** CI green; mock-provider ReAct loop verified end-to-end; milestone tagged.

---

## Milestone 3 — Model capabilities & tiering

**Scope:**
- `ModelCapabilities` per model: tool_calling, structured_output, thinking, max_context, max_output, temperature_supported, tier (small/frontier).
- Explicit TOML override required for local/self-hosted models; fail closed on missing required capability.
- Tiered prompts: small models get short system prompt (<1k tokens); frontier models get full prompt.
- Tiered generation parameters: small models low temperature/low max_tokens; frontier models higher temperature/higher max_tokens.
- Selective file inclusion: only task-relevant files included (ripgrep symbol scan, pure Rust — no C deps).
- Capped history turns per model tier.
- Context window warnings before cap: per-turn current/total tokens; warn at ~10–15% remaining.
- Thinking channel: optional, separate from visible message stream; not replayed for small-context models.

**Out of scope:** Tool calling execution, permission layer, TUI, RPC.

**Acceptance criteria:**
- `ModelCapabilities` declared per model via TOML; missing required capability fails closed.
- Small-model tier uses short prompt and minimal generation params; frontier tier uses full prompt and higher params.
- Selective file inclusion excludes irrelevant files; history turns capped per tier.
- Context warning fires before hard cap; hard cap still raises clear error.

**Testing requirements:** Unit tests for capability negotiation, tier selection, selective context inclusion, history-turn capping, context warnings. No network.

**Definition of Done:** CI green; milestone tagged.

---

## Milestone 4 — Tool system + text-based tool calling

**Status:** complete — tagged v0.1.0-milestone-4  

**Scope:**
- `Tool` trait (JSON-schema input/output) + `ToolRegistry`.
- Built-in `todo` tool (add/complete/list).
- Text-based tool calling: for models without native function calling, inject text-based tool schema into system prompt (XML/key-value format); parse raw model output into tool invocations (`TextToolParser`).
- Lenient parse + validate-before-execute: parse tool calls leniently (tolerate trailing text, fix common quote/bracket errors), validate against schema before execution, return structured error on failure. Never execute unvalidated calls.

**Out of scope:** Built-in `read`/`write`/`delete`/`run`/`grep`, permission layer, session log, TUI, RPC.

**Acceptance criteria:**
- `todo` add/complete/list executes and returns structured results.
- Text-based tool calls parse leniently and validate against schema before execution.
- Invalid tool calls return structured errors; never execute unvalidated calls.

**Testing requirements:** Unit tests for `Tool` trait, `ToolRegistry`, `TextToolParser`, lenient parsing, schema validation before execution. No network.

**Definition of Done:** CI green; no unvalidated tool call executes; milestone tagged. ✓
- `cargo fmt --check` passes
- `cargo clippy -- -D warnings` passes
- `cargo test` passes
- `cargo build --release` produces a single binary
- `todo` add/complete/list executes and returns structured results
- Text-based tool calls parse leniently and validate against schema before execution
- Invalid tool calls return structured errors; never execute unvalidated calls
- Tagged v0.1.0-milestone-4

---

## Milestone 5 — Permission layer & approval policy

**Scope:**
- Permission layer: allow-list, confirm-before-write/delete/network, `allow-always` session grants.
- Approval policy wiring: ReAct loop pauses on approval request, resumes only on `Approval` event.

**Out of scope:** Built-in `read`/`write`/`delete`/`run`/`grep`, session log, TUI, RPC.

**Acceptance criteria:**
- `todo` add/complete/list executes without approval.
- A stubbed `write` tool is blocked until approval is granted (allow / deny / allow-always).
- Loop pauses on approval request and resumes on `Approval`.

**Testing requirements:** Unit tests for policy decisions (allow/deny/allow-always), approval pause/resume. No network.

**Definition of Done:** CI green; no tool executes a guarded operation without policy approval; milestone tagged. ✓
- `cargo fmt --check` passes
- `cargo clippy -- -D warnings` passes
- `cargo test` passes
- `cargo build --release` produces a single binary
- `todo` add/complete/list executes without approval (allow-list + Read category)
- Stubbed `write` tool is blocked until approval is granted (allow / deny / allow-always)
- Loop pauses on `ApprovalRequest` and resumes on `Approval` decision
- Policy decisions (allow/deny/allow-always) unit tested; approval pause/resume unit tested
- Tagged v0.1.0-milestone-5

---

## Milestone 6 — Built-in tools + interruptible execution

**Status:** complete — tagged v0.1.0-milestone-6  

**Scope:**
- Built-in tools: `read`, `write`, `delete`, `run` (local process at permission boundary), `grep`.
- Tool output truncation: max bytes per result, head/tail with overflow indicator, counts against context cap.
- Interruptible tool execution: stream `run` output incrementally; configurable per-invocation timeout; on cancel SIGTERM → short grace → SIGKILL process group; on timeout straight SIGKILL; drain output after exit; return exit_code, timed_out, cancelled in tool result.

**Out of scope:** Rate limiting, fallback strategies, session log, TUI, RPC.

**Acceptance criteria:**
- All 5 built-in tools execute under policy.
- Oversized `read`/`grep` output is truncated with overflow indicator.
- `run` output streams incrementally; Ctrl-C cancels with SIGTERM→grace→SIGKILL; timeout triggers SIGKILL; output drained after exit.

**Testing requirements:** Unit tests for each built-in tool under policy, truncation behavior, tool execution interrupt/cancel. No network.

**Definition of Done:** CI green; policy-boundary verified; interruptible execution verified; milestone tagged. ✓
- `cargo fmt --check` passes
- `cargo clippy -- -D warnings` passes
- `cargo test` passes (163 unit tests)
- `cargo build --release` produces a single binary
- All 5 built-in tools (read, write, delete, run, grep) implement the `Tool` trait and execute under policy
- `read`/`grep` output truncated with head/tail overflow indicator at MAX_OUTPUT_BYTES
- `run` tool streams output incrementally, supports per-invocation `timeout_secs`, kills on timeout (SIGKILL), drains output after exit, returns exit_code/timed_out/cancelled
- `run` uses `kill_on_drop(true)` and process groups for cancellation cleanup
- Tool execution interruptible via ReAct loop `select!` on shutdown signal
- Unit tests for each built-in tool under policy, truncation behavior, tool execution interrupt/cancel
- Tagged v0.1.0-milestone-6

---

## Milestone 7 — Rate limits & fallback strategies

**Status:** complete — tagged v0.1.0-milestone-7  

**Scope:**
- Error-in-context with bounded retries (2–3) + exponential backoff on provider 429/503.
- Provider errors distinguished from permanent tool errors; retry events logged.
- Fallback strategies: on malformed/failed tool call, retry once with simpler prompt + reduced tool set; if still failing, escalate to user or skip turn. Hard retry cap per turn.

**Out of scope:** Session log, TUI, RPC, cost budget, edit format.

**Acceptance criteria:**
- Provider 429 triggers retry with exponential backoff (2–3 attempts).
- Permanent tool errors are not retried indefinitely.
- Malformed tool call triggers retry with simpler prompt; escalation or skip after hard cap.

**Testing requirements:** Unit tests for retry bounds, backoff timing, provider-vs-tool error distinction, fallback strategies. No network (mock provider returns 429/error responses).

**Definition of Done:** CI green; retry and fallback verified; milestone tagged. ✓
- `cargo fmt --check` passes
- `cargo clippy -- -D warnings` passes
- `cargo test` passes (176 unit tests)
- `cargo build --release` produces a single binary
- Provider 429/503 triggers retry with exponential backoff (2–3 attempts)
- Permanent errors (AuthFailed, ModelNotFound, InvalidRequest, ContextLengthExceeded, MissingCapability) are not retried
- Malformed tool call triggers fallback retry with simpler prompt + reduced tool set
- Fallback exhaustion emits error event and skips turn (no loop abort)
- Retry events logged via `Event::Log`
- Tagged v0.1.0-milestone-7

---

## Milestone 8 — Session log & resume

**Scope:**
- Append-only JSONL session log of messages, tool calls, results, approvals, snapshot events.
- Session header line: id, cwd, format version.
- `--resume <session-id>`: restores conversation + todos + plan state.
- Project-scoped session storage under okeanos home dir.
- Corrupted trailing line tolerance (skip/report, don't crash).
- **Graceful shutdown:** SIGINT/SIGTERM flushes the JSONL session log, tears down TUI alternate screen (when M12 ready), drains in-flight tool execution, escalates SIGTERM→SIGKILL after timeout.

**Out of scope:** Git snapshots, undo/redo, TUI, RPC.

**Acceptance criteria:**
- Session log is append-only; every ReAct event is appended.
- `--resume <id>` restores conversation, todos, plan state.
- Corrupted last line does not crash the loader; earlier lines are readable.
- SIGINT flushes session log to disk before exit.
- SIGTERM escalation works: grace period then SIGKILL on hung work.

**Testing requirements:** Integration tests for session log append, resume round-trip, corrupted-line tolerance, project-scoped storage, graceful shutdown flush. Headless mode only (no TUI).

**Definition of Done:** CI green; resume verified end-to-end in headless mode; graceful shutdown verified; milestone tagged.

---

## Milestone 9 — OpenRouter / OpenAI-compatible provider

**Scope:**
- `OpenRouterModel` implementing `Model` trait with chat completion + streaming.
- OpenAI-compatible API via reqwest + rustls (already in deps).
- Native function calling (OpenAI tool calling format) with `supports_tool_calling = true`.
- Streaming token deltas via SSE; emit `Token` events for TUI/headless parity.
- Provider discovery via OpenRouter `/models` endpoint with local cache (24h TTL).
- Model capability negotiation: auto-populate `ModelCapabilities` from discovered metadata.
- Config-driven model selection: `model.name`, `provider.base_url`, `provider.api_key` (with `{env:VAR}`/`{file:path}` secret substitution).
- Retry policy integration: 429/503 → exponential backoff (uses M7 retry policy).
- Error mapping: provider errors → `ModelError` variants (AuthFailed, RateLimited, ServiceUnavailable, ContextLengthExceeded, InvalidRequest, ModelNotFound, MissingCapability).
- Per-model pricing from OpenRouter metadata → cost estimation (feeds M10 cost budget).

**Out of scope:** Non-OpenAI-compatible providers (Anthropic, etc. — feature-gated post-MVP), WebSocket transport, MCP client.

**Acceptance criteria:**
- `okeanos --headless "prompt"` runs end-to-end against OpenRouter with a real model.
- Streaming tokens render in headless JSON events and TUI (when M12 ready).
- Native tool calls execute via `ToolRegistry`; text-based fallback works for models without FC.
- Provider discovery populates capabilities; explicit TOML override works for self-hosted.
- Rate limits trigger retry with backoff; permanent errors fail fast.
- Cost estimates match OpenRouter pricing (within estimate tolerance).

**Testing requirements:** Integration tests with real OpenRouter API (feature-gated, requires API key); unit tests with mock HTTP server for error mapping, streaming parsing, tool call format. CI runs unit tests only; integration tests run manually or in separate workflow.

**Definition of Done:** CI green (unit tests); integration verified manually; milestone tagged. ✓
- `cargo fmt --check` passes
- `cargo clippy -- -D warnings` passes
- `cargo test` passes (all unit tests)
- `cargo build --release` produces a single binary
- `cargo build --release --target x86_64-unknown-linux-musl` produces a static binary
- `OpenRouterModel` implements `Model` trait with chat completion + streaming
- Native function calling via OpenAI tool calling format works for models with `supports_tool_calling = true`
- Streaming token deltas via SSE; `Token` events emitted for TUI/headless parity
- Provider discovery via OpenRouter `/models` endpoint with local cache (24h TTL)
- Model capability negotiation auto-populates `ModelCapabilities` from discovered metadata
- Config-driven model selection: `model.name`, `provider.base_url`, `provider.api_key` with `{env:VAR}`/`{file:path}` secret substitution
- Retry policy integration: 429/503 → exponential backoff (uses M7 retry policy)
- Error mapping: provider errors → `ModelError` variants (AuthFailed, RateLimited, ServiceUnavailable, ContextLengthExceeded, InvalidRequest, ModelNotFound, MissingCapability)
- Per-model pricing from OpenRouter metadata → cost estimation
- Unit tests with mock HTTP server for error mapping, streaming parsing, tool call format
- Tagged v0.1.0-milestone-9

---

## Milestone 10 — Cost budget

**Status:** complete — tagged v0.1.0-milestone-10

**Scope:**
- Opt-in hard session cost cap (USD) + per-turn soft warning threshold.
- When cap reached, refuse new tool calls that would exceed it; let in-flight work finish.
- Fail closed ($0 + warning) on unknown pricing.
- Cost = tokens × declared per-model rate (from `ModelCapabilities`/model metadata).
- Cap disabled by default at MVP.

**Out of scope:** Per-turn confirmation for expensive calls (post-MVP).

**Acceptance criteria:**
- Cost cap enforcement: calls over the cap are refused; in-flight work finishes.
- Unknown pricing fails closed with clear warning.
- Per-turn soft warning fires before hard cap.

**Testing requirements:** Unit tests for cost cap enforcement, fail-closed on unknown pricing, per-turn warnings. No network.

**Definition of Done:** CI green; milestone tagged. ✓
- `cargo fmt --check` passes
- `cargo clippy -- -D warnings` passes
- `cargo test` passes (214 unit tests)
- `cargo build --release` produces a single binary
- Cost cap enforcement: calls over the cap are refused; in-flight work finishes
- Unknown pricing fails closed with clear warning
- Per-turn soft warning fires before hard cap (emits warning event, does not block)
- Tagged v0.1.0-milestone-10

---

## Milestone 11 — Edit format

**Scope:**
- Diff/patch default for frontier models; whole-file / str_replace fallback for small local models.
- Per-model edit-format hint in config.
- Preflight-validate patches before applying; never silently truncate or partially apply.
- On parse failure, fall back to whole-file or surface to user.

**Out of scope:** TUI, RPC, git snapshots.

**Acceptance criteria:**
- Diff/patch applied with preflight validation for frontier models.
- Whole-file fallback used for small models or malformed patches.
- No silent truncation or partial application.

**Testing requirements:** Unit tests for patch validation, fallback behavior, no silent truncation. No network.

**Definition of Done:** CI green; milestone tagged.

---

## Milestone 12 — TUI shell

**Scope:**
- ratatui + crossterm TUI per DESIGN.md: theme tokens from `assets/themes/dark.toml`/`light.toml`, responsive layout grid (80×24 min, 120×40 comfortable).
- Components: conversation pane, streaming token render, tool-call panel, approval modal, todo panel, status bar, command palette, log tail.
- Command palette with slash commands: `/help`, `/todos`, `/logs`, `/undo`, `/redo`.
- Headless/RPC event parity: same event structs feed TUI renderer and JSON emitter (DESIGN.md §7).
- Resize handling, scroll preservation, reduced-motion toggle.

**Out of scope:** Git snapshots, RPC transport, session resume, compressed logs.

**Acceptance criteria:**
- TUI renders all regions at 80×24 and 120×40; regions reflow on resize.
- Approval modal is keyboard-first (Enter allow / Esc deny) with high-contrast focus ring.
- Streaming tokens render with typewriter cursor; typing indicator present.
- Command palette opens with `:`; `/help`, `/todos`, `/logs`, `/undo`, `/redo` work.
- Snapshot tests pass for golden frames; contrast audit passes (WCAG AA/AAA); reduced-motion disables all animations.

**Testing requirements:** TUI snapshot tests, resize tests, contrast audit, reduced-motion test, event-parity test (TUI state → JSON event).

**Definition of Done:** Visual design review sign-off; CI green including snapshot/contrast tests; DESIGN.md not deviated from without design review; milestone tagged.

---

## Milestone 13 — Git snapshots + undo/redo

**Scope:**
- gix-based snapshot store: snapshot before destructive actions and on loop boundaries.
- `/undo` / `/redo` commands restoring prior snapshots; undo/redo cursor.
- Snapshot progress UI (TUI status bar + JSON `snapshot_progress` event).

**Out of scope:** Snapshot pruning/retention policy, remote git operations, branch management, compressed logs.

**Acceptance criteria:**
- `/undo` restores prior working-tree state; `/redo` re-applies it.
- Snapshots taken before any `write`/`delete`/destructive `run`.
- Undo/redo cursor bounded; cannot undo past oldest snapshot.

**Testing requirements:** Integration tests for snapshot round-trip, undo/redo sequences, snapshot-log reference integrity.

**Definition of Done:** CI green; undo/redo verified end-to-end in headless and TUI modes; milestone tagged.

---

## Milestone 14 — Headless/RPC mode

**Scope:**
- `--headless`: no terminal; JSON event stream to stdout identical to TUI events.
- `--rpc`: JSON-RPC over stdio (feature-gated `jsonrpsee` server); optional HTTP transport behind `rpc` feature.
- ReAct loop driven identically in all three modes; mode is a renderer/transport choice only.
- RPC accepts `approve`/`deny` commands resolving pending approvals.
- Slash command support via JSON events: `/help`, `/todos`, `/logs`, `/undo`, `/redo` emitted as structured events for script consumption.
- **Headless approval handling:** `--approval-policy auto|deny|prompt` flag: `auto` = allow-all, `deny` = block all privileged ops, `prompt` = emit `ApprovalRequest` event and wait for JSON `Approval` on stdin (for scripted approval). Timeout configurable via `--approval-timeout-sec`.

**Out of scope:** WebSocket transport (deferred), MCP client (deferred, `mcp` feature).

**Acceptance criteria:**
- Same prompt produces identical event sequences in TUI, headless, and RPC modes.
- RPC accepts `approve`/`deny` commands resolving pending approvals.
- Headless mode works piped/non-TTY.
- Slash commands emit corresponding JSON events (not interactive).
- `--approval-policy auto` allows writes without pausing; `deny` blocks them with clear error; `prompt` waits for stdin JSON.

**Testing requirements:** Event-parity test across modes; RPC request/response round-trip tests.

**Definition of Done:** CI green; parity test covers every event in DESIGN.md §7; milestone tagged.

---

## Milestone 15 — Apple Silicon support

**Scope:**
- Native `aarch64-apple-darwin` target build via osxcross (or macOS CI runners), producing a native Apple Silicon binary.
- macOS-specific packaging/signing notes; static analysis that the dependency tree remains pure Rust where possible (no C deps where avoidable).

**Out of scope:** Cross-compilation from Linux runners without the macOS SDK (osxcross toolchain provisioning only); TUI visual changes.

**Acceptance criteria:**
- `cargo build --release --target aarch64-apple-darwin` succeeds in CI.
- The Apple Silicon binary starts, parses CLI args, and prints `--help`/`--version`.
- No new C dependencies introduced beyond those required by the osxcross toolchain itself.

**Testing requirements:** CI build check for the `aarch64-apple-darwin` target only (no runtime execution on Linux runners).

**Definition of Done:** CI green for the Apple Silicon target; milestone tagged.

---

## Milestone summary table

| Milestone | Deliverable | Sessions (est.) | Builds on |
|-----------|-------------|-----------------|-----------|
| 0 | Scaffolding + CLI | 1 | — |
| 0.5 | CLI UX Essentials | 1 | M0 |
| 1 | Config & secrets | 1 | M0 |
| 2 | Core harness loop | 2 | M1 |
| 3 | Model capabilities & tiering | 2 | M2 |
| 4 | Tool system + text-based calling | 2 | M2 |
| 5 | Permission layer & approval policy | 1 | M2, M4 |
| 6 | Built-in tools + interruptible execution | 2 | M4, M5 |
| 7 | Rate limits & fallback strategies | 1 | M6 |
| 8 | Session log & resume | 1 | M2 |
| 9 | **OpenRouter / OpenAI-compatible provider** | 2 | M3, M4 |
| 10 | ✅ Cost budget | 1 | M2, M3, M9 |
| 11 | Edit format | 1 | M6, M9 |
| 12 | TUI shell | 2–3 | M2, M4, M5, M9, M11 |
| 13 | Git snapshots + undo/redo | 2 | M6 |
| 14 | Headless/RPC | 2 | M2, M8, M11, M12 |
| 15 | Apple Silicon support | 1–2 | M0 |
