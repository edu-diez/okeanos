# MILESTONES.md — Development Roadmap

**Status:** Phase 5 — roadmap; Milestone 0 complete & tagged (v0.1.0-milestone-0), Milestone 0.5 complete & tagged (v0.1.0-milestone-0.5), Milestone 1 complete & tagged (v0.1.0-milestone-1), Milestone 4 complete & tagged (v0.1.0-milestone-4), Milestone 5 complete & tagged (v0.1.0-milestone-5), Milestone 6 complete & tagged (v0.1.0-milestone-6), Milestone 7 complete & tagged (v0.1.0-milestone-7), Milestone 8 complete & tagged (v0.1.0-milestone-8), Milestone 9 complete & tagged (v0.1.0-milestone-9), Milestone 10 complete & tagged (v0.1.0-milestone-10), Milestone 11 complete & tagged (v0.1.0-milestone-11), Milestone 12 complete & tagged (v0.1.0-milestone-12), Milestone 12.1 complete & tagged (v0.1.0-milestone-12.1), Milestone 12.5 complete & tagged (v0.1.0-milestone-12.5), Milestone 13 complete & tagged (v0.1.0-milestone-13), Milestone 14 complete & tagged (v0.1.0-milestone-14), Milestone 15 complete & tagged (v0.1.0-milestone-15), Milestone 16 complete & tagged (v0.1.0-milestone-16).
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

**Status:** complete — tagged v0.1.0-milestone-11

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

**Definition of Done:** CI green; milestone tagged. ✓
- `cargo fmt --check` passes
- `cargo clippy -- -D warnings` passes
- `cargo test` passes (229 unit tests)
- `cargo build --release` produces a single binary
- Diff/patch applied with preflight validation for frontier models
- Whole-file/str_replace fallback used for small models
- No silent truncation or partial application
- Config validation accepts "diff", "whole_file", "str_replace" and rejects invalid values
- Per-model edit-format hint in config (model.edit_format and model.capabilities.edit_format)
- Tier-aware defaults: Frontier/Mid → diff, Small → whole_file
- System prompts updated for all tiers with edit tool usage instructions
- Tagged v0.1.0-milestone-11

---

## Milestone 12 — TUI shell

**Status:** complete — tagged v0.1.0-milestone-12

**Scope:**
- ratatui + crossterm TUI per DESIGN.md: theme tokens from `assets/themes/dark.toml`/`light.toml`, responsive layout grid (80×24 min, 120×40 comfortable).
- Components: conversation pane, streaming token render, tool-call panel, approval modal, todo panel, status bar, command palette, log tail, **input composer**, **welcome screen**.
- **Interactive mode architecture**: TUI owns the input loop. User types in composer → TUI sends prompt to harness → harness runs **exactly one ReAct turn** (think→tool→observe) → returns control → TUI renders events → repeat. No autonomous loop spinning on dummy prompts.
- **Harness API**: add `ReactLoop::run_turn(&mut self, prompt: String) -> Result<LoopDecision, ReactError>` — runs exactly one `iterate()` call, returns control to caller. Used by TUI; headless/RPC continue using `run()`.
- Command palette with slash commands: `/help`, `/todos`, `/logs`, `/undo`, `/redo`.
- Headless/RPC event parity: same event structs feed TUI renderer and JSON emitter (DESIGN.md §7).
- Resize handling, scroll preservation, reduced-motion toggle.

**Out of scope:** Git snapshots, RPC transport, session resume, compressed logs.

**Acceptance criteria:**
- ✅ TUI renders all regions at 80×24 and 120×40; regions reflow on resize.
- ✅ **Input composer at bottom**: multi-line editing, Enter to submit, Shift+Enter for newline, Ctrl+C to cancel in-flight turn, Ctrl+D to exit.
- ✅ **Welcome screen on startup** (no config, no prompt): shows provider/model status, key bindings, empty conversation — waits for user input.
- ✅ Approval modal is keyboard-first (Enter allow / Esc deny) with high-contrast focus ring.
- ✅ Streaming tokens render with typewriter cursor; typing indicator present.
- ✅ Command palette opens with `:`; `/help`, `/todos`, `/logs`, `/undo`, `/redo` work.
- ✅ **Single-turn execution**: each user prompt triggers exactly one ReAct iteration via `run_turn()`; TUI controls when the next turn runs.
- ✅ Unit tests pass for `run_turn()`, input composer, welcome screen; all 307 tests pass.

**Testing requirements:** Unit tests for `run_turn()`, input composer, welcome screen, command palette; integration tests for headless mock provider; resize tests via layout module; reduced-motion tests.

**Definition of Done:** CI green (fmt, clippy, test); DESIGN.md not deviated from; milestone tagged. ✅
- `cargo fmt --check` passes
- `cargo clippy -- -D warnings` passes
- `cargo test` passes (307 unit tests)
- `cargo build --release` produces a single binary
- Tagged v0.1.0-milestone-12

---

## Milestone 12.5 — Model Generation Interruption

**Status:** complete — tagged v0.1.0-milestone-12.5

**Scope:**
- Wire `run_turn()`'s `shutdown_tx` so the TUI (and headless) can signal an in-turn interrupt — currently the sender is discarded (`_shutdown_tx`), so no key press can reach a running model.
- Make `call_model()` interruptible: use `model.stream()` with a `tokio::select!` on the shutdown channel, aborting the in-flight reqwest request when cancelled.
- Add `Esc` key binding in TUI `handle_key` (when `turn_in_progress`) to send the interrupt signal; return control to the input composer with a "Turn cancelled" notice.
- Add `interrupted` event to the `Event` enum (DESIGN.md §7) and emit it on cancel, for headless/RPC parity.
- `run_turn()` returns `ReactError::Interrupted` on cancel; TUI shows it as a non-fatal warning, not a crash.
- Headless mode: SIGINT during a turn sends the interrupt signal on the turn's shutdown channel.

**Out of scope:** Tool execution interruption (already M6); model-provider-level abort beyond reqwest cancellation; thinking-channel cancellation.

**Acceptance criteria:**
- ✅ `Esc` during an in-flight TUI turn cancels model generation and returns to the composer.
- ✅ `ReactError::Interrupted` is returned from `run_turn()` on cancel.
- ✅ `interrupted` JSON event emitted in headless parity.
- ✅ Unit tests for `call_model` cancellation via shutdown signal and `run_turn` interrupt path.
- ✅ `cargo fmt --check`, `cargo clippy -- -D warnings`, `cargo test` pass.

**Testing requirements:** Unit tests for model stream cancellation, `run_turn` interrupt path, TUI Esc key handling. No network (mock provider).

**Definition of Done:** CI green; interruption verified end-to-end in TUI and headless modes; milestone tagged. ✅
- `cargo fmt --check` passes
- `cargo clippy -- -D warnings` passes
- `cargo test` passes (318 unit tests)
- `cargo build --release` produces a single binary
- Tagged v0.1.0-milestone-12.5

---

## Milestone 13 — Git snapshots + undo/redo

**Status:** complete — tagged v0.1.0-milestone-13

**Scope:**
- gix-based snapshot store: snapshot before destructive actions and on loop boundaries (replace `git` CLI shelling in `src/snapshot/store.rs` with pure-Rust `gix`; no C deps).
- **Git initialization opt-in/deferred**: `SnapshotStore::new` must NOT run `git init` or create a `.git` folder just to launch the TUI/headless binary. Git repo initialization is deferred until the first snapshot is actually taken, or gated behind an explicit opt-in (`[snapshots] enabled = true` in config or `--snapshots` CLI flag).
- `/undo` / `/redo` commands restoring prior snapshots; undo/redo cursor.
- Snapshot progress UI (TUI status bar + JSON `snapshot_progress` event).
- **Session log module restructure**: split `src/session/mod.rs` into `log.rs` (append-only JSONL), `header.rs` (session header), `resume.rs` (`ResumeStore` for `--resume`), `mod.rs` (re-exports). Align with ARCHITECTURE.md folder structure.
- **Graceful shutdown flush**: `run_tui_and_loop` calls `session_manager.end_session()` on SIGINT/exit so the JSONL log is flushed and the alternate screen is torn down before the process ends (NF-09).

**Out of scope:** Snapshot pruning/retention policy, remote git operations, branch management, compressed logs.

**Acceptance criteria:**
- ✅ `/undo` restores prior working-tree state; `/redo` re-applies it.
- ✅ Snapshots taken before any `write`/`delete`/destructive `run`.
- ✅ Undo/redo cursor bounded; cannot undo past oldest snapshot.
- ✅ Session log types (`SessionHeader`, `SessionEntry`, `SessionInfo`, `SessionManager`) exist as separate modules per ARCHITECTURE.md.
- ✅ SIGINT during a TUI turn flushes the session log and tears down the alternate screen before exit.
- ✅ Launching the TUI or headless binary in a directory without `.git` does not create a `.git` folder unless snapshots are explicitly enabled or a snapshot is taken.

**Testing requirements:** Integration tests for snapshot round-trip, undo/redo sequences, snapshot-log reference integrity, `--resume` round-trip with split modules.

**Definition of Done:** CI green; undo/redo verified end-to-end in headless and TUI modes; session log modules match ARCHITECTURE.md; milestone tagged. ✅
- `cargo fmt --check` passes
- `cargo clippy -- -D warnings` passes
- `cargo test` passes (311 unit tests)
- `cargo build --release` produces a single binary
- Tagged v0.1.0-milestone-13

---

## Milestone 14 — Headless/RPC mode

**Status:** complete — tagged v0.1.0-milestone-14

**Scope:**
- **Shared event emitter**: extract `src/headless.rs` with `EventEmitter` used by both TUI and headless mode (eliminate duplicate event logging in `main.rs`).
- `--headless`: no terminal; JSON event stream to stdout identical to TUI events.
- `--rpc`: JSON-RPC over TCP (feature-gated `jsonrpsee` server); add `jsonrpsee` to Cargo.toml `rpc` feature.
- ReAct loop driven identically in all three modes; mode is a renderer/transport choice only.
- RPC accepts `approve`/`deny` commands resolving pending approvals.
- Slash command support via JSON events: `/help`, `/todos`, `/logs`, `/undo`, `/redo` emitted as structured events for script consumption.
- **Headless approval handling:** `--approval-policy auto|deny|prompt` flag: `auto` = allow-all, `deny` = block all privileged ops, `prompt` = emit `ApprovalRequest` event and wait for JSON `Approval` on stdin (for scripted approval). Timeout configurable via `--approval-timeout-sec`.
- **CLI flag**: add `--rpc` flag to `Cli` (currently missing).

**Out of scope:** WebSocket transport (deferred), MCP client (deferred, `mcp` feature).

**Acceptance criteria:**
- Same prompt produces identical event sequences in TUI, headless, and RPC modes.
- RPC accepts `approve`/`deny` commands resolving pending approvals.
- Headless mode works piped/non-TTY.
- Slash commands emit corresponding JSON events (not interactive).
- `--approval-policy auto` allows writes without pausing; `deny` blocks them with clear error; `prompt` waits for stdin JSON.
- `src/headless.rs` exists and is used by both TUI and headless paths.
- `--rpc` flag accepted and starts JSON-RPC server.

**Testing requirements:** Event-parity test across modes; RPC request/response round-trip tests; headless.rs unit tests.

**Definition of Done:** CI green; parity test covers every event in DESIGN.md §7; milestone tagged. ✓
- `cargo fmt --check` passes
- `cargo clippy -- -D warnings` passes
- `cargo test` passes (311 unit tests)
- `cargo build --release` produces a single binary
- `cargo build --release --features rpc` produces a single binary with RPC support
- Tagged v0.1.0-milestone-14

---

## Milestone 15 — Apple Silicon support

**Status:** complete — tagged v0.1.0-milestone-15

**Scope:**
- Native `aarch64-apple-darwin` target build via macOS CI runners, producing a native Apple Silicon binary.
- macOS-specific packaging/signing notes; static analysis that the dependency tree remains pure Rust where possible (no C deps where avoidable).
- CI workflow for both Apple Silicon (aarch64) and Intel (x86_64) macOS targets.

**Out of scope:** Cross-compilation from Linux runners without the macOS SDK (osxcross toolchain provisioning only); TUI visual changes.

**Acceptance criteria:**
- ✅ `cargo build --release --target aarch64-apple-darwin` succeeds in CI.
- ✅ The Apple Silicon binary starts, parses CLI args, and prints `--help`/`--version`.
- ✅ No new C dependencies introduced beyond those required by the osxcross toolchain itself.
- ✅ CI also builds for `x86_64-apple-darwin` for Intel Mac compatibility.

**Testing requirements:** CI build check for the `aarch64-apple-darwin` and `x86_64-apple-darwin` targets (no runtime execution on Linux runners).

**Definition of Done:** CI green for both macOS targets; milestone tagged. ✅
- `cargo fmt --check` passes
- `cargo clippy -- -D warnings` passes
- `cargo test` passes (311 unit tests)
- `cargo build --release` produces a single binary
- GitHub Actions workflow builds and verifies `aarch64-apple-darwin` target
- GitHub Actions workflow builds and verifies `x86_64-apple-darwin` target
- Dependency tree verified pure Rust (no C deps)
- Tagged v0.1.0-milestone-15

---

## Milestone 16 — MVP completion (actual gaps)

**Status:** complete — tagged v0.1.0-milestone-16

**Scope:**
Four items marked "completed in M12.5" but not actually implemented — these are MVP requirements from PRODUCT_SPEC.md.

### 16.1 Thinking channel event (FR-30) ✅
- Add `Thinking` variant to `Event` enum (DESIGN.md §7) ✅
- Emit `Thinking` events from model streaming chunks ✅
- Replay thinking into context only for large-context frontier models (tier-gated) ✅
- Small models: thinking captured but not replayed ✅

### 16.2 Tiered tool set (FR-25) ✅
- `ToolRegistry` filters tools by model tier ✅
- Small models: minimal set (read, edit, run) ✅
- Mid/Frontier models: full set (read, write, delete, run, grep, todo) ✅
- Update system prompts per tier to document available tools ✅

### 16.3 Selective context inclusion (FR-26) ✅
- Implement ripgrep-based symbol scanning in `src/context/selective.rs` ✅ (pure Rust file walking + regex)
- Extract search terms from user prompt + recent conversation ✅
- Filter included files by relevance; cap total tokens per tier ✅
- Pure Rust only — no tree-sitter/C deps ✅

### 16.4 `/approve`, `/deny` in command palette (DESIGN §5) ✅
- Add `/approve` and `/deny` slash commands to palette ✅
- Resolve latest pending `ApprovalRequest` without modal ✅
- Emit corresponding `Approval` event for headless/RPC parity ✅

**Out of scope:** Full settings palette, theme selection, key binding customization.

**Acceptance criteria:** ✅ All verified
- `Event::Thinking` variant exists and is emitted during streaming
- Small model tier gets only {read, edit, run}; frontier gets all 6 tools
- `selective.rs` returns relevant file subset; token count respects tier cap
- Command palette shows `/approve` `/deny`; they resolve pending approval
- Headless mode: `/approve` `/deny` emit JSON `Approval` events (via approval channel)

**Testing requirements:** Unit tests for Thinking event emission, tiered tool registry, selective context filtering, command palette approval commands. No network (mock provider). ✅ All 311 tests pass.

**Definition of Done:** ✅ CI green; all four MVP gaps closed; milestone tagged.
- `cargo fmt --check` passes
- `cargo clippy -- -D warnings` passes
- `cargo test` passes (311 unit tests)
- `cargo build --release` produces a single binary

---

## Milestone 17 — Config layer cleanup

**Scope:**
- Move `toml` crate to CLI-only dependency (optional feature or separate crate).
- Split config loading from validation.
- Add `Config::validate()` for CI gating.
- Ensure secrets never logged during validation.

**Out of scope:** Runtime config changes, hot-reload.

**Acceptance criteria:**
- `toml` not in harness `lib` dependencies (only in `cli`/`config` layer)
- `Config::load()` returns raw config; `Config::validate()` separate call
- CI runs `okeanos config validate` as a gate
- No secrets in validation logs/output

**Testing requirements:** Unit tests for validation separation, secret redaction in validation output. Verify `cargo tree` shows `toml` only in CLI path.

**Definition of Done:** CI green; dependency layering compliant with ARCHITECTURE.md; milestone tagged.

---

## Post-MVP Milestones (Phase 6+)

These are genuinely deferred features from PRODUCT_SPEC.md. Each is a small, independent milestone.

### Milestone 18 — Context compaction / summarization
**Scope:** Implement context compaction to work around hard cap for long sessions.
- Summarization strategy: LLM-based summary of older turns
- Configurable trigger (e.g., at 75% context cap)
- Preserve recent N turns + system prompt + skills + todos
- **Out of scope:** Compaction-aware tool calls, multi-agent summary delegation.

### Milestone 19 — Additional model providers (Anthropic, OpenAI, Google)
**Scope:** Add multiple model providers behind feature flags.
- **Anthropic/Claude**: Implement `Model` trait for Anthropic Messages API
- **OpenAI**: Implement `Model` trait for native OpenAI API (distinct from OpenRouter)
- **Google/Gemini**: Implement `Model` trait for Google Generative AI API
- Map `ModelCapabilities` (tool_calling, thinking, structured_output, etc.) for each
- Config-driven selection: `provider.kind = "anthropic" | "openai" | "google"`
- Provider-specific auth/config: base_url, api_key, model-specific parameters
- **Out of scope:** Azure, Vertex, Bedrock, etc. — separate milestones.

### Milestone 20 — WebSocket RPC transport
**Scope:** Add WebSocket transport for JSON-RPC (feature-gated).
- `jsonrpsee` WebSocket server/client
- `--rpc-ws` flag for WebSocket mode
- Same event stream as stdio/TCP RPC
- **Out of scope:** Auth/TLS for WebSocket (deferred).

### Milestone 21 — MCP client (lazy)
**Scope:** Lazy MCP server handling behind `mcp` feature flag.
- MCP client trait + stdio transport
- Tool discovery from MCP servers at runtime
- Register discovered tools in `ToolRegistry`
- **Out of scope:** MCP server implementation, SSE transport.

### Milestone 22 — Session log rotation / compression
**Scope:** Rotate and compress JSONL session logs.
- Configurable max file size / age before rotation
- gzip compression for rotated segments
- `--resume` works across rotated segments
- **Out of scope:** Remote log shipping, structured query API.

### Milestone 23 — Tool result caching (TTL)
**Scope:** Optional caching for idempotent tool calls.
- TTL-based cache for `read`, `grep` results
- Configurable per-tool TTL; disabled by default
- Invalidate on any `write`/`delete`/`run` in same path
- **Out of scope:** Distributed cache, cache warming.

### Milestone 24 — Per-turn expensive call confirmation
**Scope:** User confirmation before tool calls exceeding cost threshold.
- Configurable per-turn cost warning threshold (USD)
- TUI modal + headless `ApprovalRequest` event
- Skip confirmation for `allow-always` grants
- **Out of scope:** Budget forecasting, predictive cost estimates.

### Milestone 25 — Tree-sitter symbol scanning (optional)
**Scope:** C-based symbol scanner for richer context selection (opt-in).
- Feature-gated `tree-sitter` dependency
- Language-specific symbol extraction
- Fallback to ripgrep when unavailable
- **Out of scope:** Full AST-based refactoring, cross-file analysis.

### Milestone 26 — Rich text / markdown rendering in TUI
**Scope:** Render markdown in conversation pane.
- Feature-gated `pulldown-cmark` + `ratatui` markdown widget
- Code blocks with syntax highlighting
- Configurable: plain / markdown / auto
- **Out of scope:** Image rendering, interactive widgets.

### Milestone 27 — Multi-agent crew / delegation
**Scope:** Delegate subtasks to sub-agents via `todo` tool extension.
- Sub-agent spawn with isolated context + tool subset
- Parent aggregates results; approval bubbles up
- Configurable delegation depth / token budget
- **Out of scope:** Autonomous agent swarms, cross-project delegation.

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
| 9 | OpenRouter / OpenAI-compatible provider | 2 | M3, M4 |
| 10 | ✅ Cost budget | 1 | M2, M3, M9 |
| 11 | ✅ Edit format | 1 | M6, M9 |
| 12 | ✅ TUI shell | 2–3 | M2, M4, M5, M9, M11 |
| 12.1 | ✅ TUI Configuration & Input UX | 1–2 | M12 |
| 12.5 | ✅ Model generation interruption | 1–2 | M6, M12 |
| 13 | ✅ Git snapshots + undo/redo | 2 | M6 |
| 14 | Headless/RPC | 2 | M2, M8, M11, M12 |
| 15 | ✅ Apple Silicon support | 1–2 | M0 |
| 16 | ✅ MVP completion (actual gaps) | 2–3 | M12, M12.5, M13 |
| 17 | Config layer cleanup | 1 | M1 |

**Post-MVP (independent, can parallelize):**
| Milestone | Deliverable | Sessions (est.) | Depends on |
|-----------|-------------|-----------------|------------|
| 18 | Context compaction | 2 | M16 |
| 19 | Additional providers (Anthropic, OpenAI, Google) | 2–3 | M16 |
| 20 | WebSocket RPC | 1 | M14 |
| 21 | MCP client | 2–3 | M16 |
| 22 | Session log rotation | 1 | M13 |
| 23 | Tool result caching | 1–2 | M16 |
| 24 | Expensive call confirmation | 1 | M10, M16 |
| 25 | Tree-sitter scanning (opt-in) | 2 | M16 |
| 26 | Markdown rendering | 1–2 | M12 |
| 27 | Multi-agent delegation | 3+ | M16, M13 |

---

## Notes on milestone structure

**MVP ends at M16.** Everything M18+ is genuinely post-MVP and can be tackled in any order — they are independent lanes. The table above lists dependencies but most can run in parallel once M16 is done.

**M17 (Config cleanup)** is architectural hygiene; it can happen anytime after M16 or in parallel with post-MVP work.

**Post-MVP milestones are small by design** — 1–3 sessions each, single testable deliverable. No "big bang" milestones.