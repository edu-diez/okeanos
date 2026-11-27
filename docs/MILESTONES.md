# MILESTONES.md — Development Roadmap

**Status:** Phase 5 — roadmap; Milestone 0 complete & tagged (v0.1.0-milestone-0), Milestone 0.5 complete & tagged (v0.1.0-milestone-0.5), Milestone 1 complete & tagged (v0.1.0-milestone-1), Milestone 4 complete & tagged (v0.1.0-milestone-4), Milestone 5 complete & tagged (v0.1.0-milestone-5), Milestone 6 complete & tagged (v0.1.0-milestone-6), Milestone 7 complete & tagged (v0.1.0-milestone-7), Milestone 8 complete & tagged (v0.1.0-milestone-8), Milestone 9 complete & tagged (v0.1.0-milestone-9), Milestone 10 complete & tagged (v0.1.0-milestone-10), Milestone 11 complete & tagged (v0.1.0-milestone-11), Milestone 12 complete & tagged (v0.1.0-milestone-12), Milestone 12.1 complete & tagged (v0.1.0-milestone-12.1), Milestone 12.5 complete & tagged (v0.1.0-milestone-12.5), Milestone 13 complete & tagged (v0.1.0-milestone-13), Milestone 14 complete & tagged (v0.1.0-milestone-14), Milestone 15 complete & tagged (v0.1.0-milestone-15), Milestone 16 complete & tagged (v0.1.0-milestone-16), Milestone 17 complete & tagged (v0.1.0-milestone-17), Milestone 18 complete & tagged (v0.2.0), Milestone 19 complete & tagged (v0.3.0), Milestone 20 complete & tagged (v0.4.0), Milestone 21 complete & tagged (v0.5.0), Milestone 22 complete & tagged (v0.6.0), Milestone 23 complete & tagged (v0.7.0), Milestone 24 complete & tagged (v0.8.0), Milestone 26 complete & tagged (v0.9.0), Milestone 27 complete & tagged (v0.11.0).
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

**Status:** complete — tagged v0.1.0-milestone-17

**Scope:**
- Move `toml` crate to CLI-only dependency (optional feature or separate crate).
- Split config loading from validation.
- Add `Config::validate()` for CI gating.
- Ensure secrets never logged during validation.

**Out of scope:** Runtime config changes, hot-reload.

**Acceptance criteria:**
- `toml` not in harness `lib` dependencies (only in `cli`/`config` layer)
- `Config::load_raw()` returns raw config; `Config::validate()` separate call
- CI runs `okeanos config validate` as a gate
- No secrets in validation logs/output

**Testing requirements:** Unit tests for validation separation, secret redaction in validation output. Verify `cargo tree` shows `toml` only in CLI path.

**Definition of Done:** CI green; dependency layering compliant with ARCHITECTURE.md; milestone tagged. ✅
- `cargo fmt --check` passes
- `cargo clippy -- -D warnings` passes
- `cargo test` passes (313 unit tests)
- `cargo build --release` produces a single binary
- `cargo build --lib --no-default-features` compiles without `toml` in dependency tree
- `Config::load_raw()` loads config without validation; `Config::validate()` validates separately
- `okeanos config validate` works as CI gate
- Secrets redacted in validation output (`***REDACTED***`)
- Tagged v0.1.0-milestone-17

---

## Post-MVP Milestones (Phase 6+)

These are genuinely deferred features from PRODUCT_SPEC.md. Each is a small, independent milestone.

### Milestone 18 — Context compaction / summarization

**Status:** complete — tagged v0.2.0

**Scope:** Implement context compaction to work around hard cap for long sessions.
- Summarization strategy: LLM-based summary of older turns
- Configurable trigger (e.g., at 75% context cap)
- Preserve recent N turns + system prompt + skills + todos
- **Out of scope:** Compaction-aware tool calls, multi-agent summary delegation.

**Acceptance criteria:**
- Automatic compaction triggers at configured threshold (default 75% of context cap)
- Manual compaction via `/compact` slash command
- Two strategies: Basic (deterministic, Cline-inspired) and Agentic (LLM-based, OpenCode/Codex-inspired)
- Tier-aware default: Small models → Basic, Mid/Frontier → Agentic
- Preserves recent N tokens (default 15k) + system prompt + skills + todos verbatim
- Dropped-work summaries include tool call signatures, file read/modified lists with line ranges
- Native compaction delegation interface (`Model::compact_native`) for OpenAI Responses, Anthropic
- Compaction events emitted to event stream (TUI/headless/RPC parity)
- Observer pattern for future M23/M27 integration
- Configuration validation for all compaction settings

**Testing requirements:** Unit tests for CompactionBudget, strategy resolution, file ops extraction, Basic and Agentic compaction logic. Integration tests for automatic compaction trigger in ReactLoop, manual `/compact` command via command palette, event emission.

**Definition of Done:** CI green; automatic and manual compaction verified end-to-end in headless and TUI modes; milestone tagged.
- `cargo fmt --check` passes
- `cargo clippy -- -D warnings` passes
- `cargo test` passes (316 unit tests)
- `cargo build --release` produces a single binary
- Automatic compaction triggers at threshold and reduces token usage
- Manual `/compact` command works in TUI and headless modes
- CompactionEvent emitted with correct metadata
- Native compaction interface available on Model trait
- Configuration validation rejects invalid values
- Tagged v0.1.0-milestone-18

### Milestone 19 — Additional model providers (Anthropic, OpenAI, Google)

**Status:** complete — tagged v0.3.0

**Scope:** Add multiple model providers behind feature flags.
- **Anthropic/Claude**: Implement `Model` trait for Anthropic Messages API
- **OpenAI**: Implement `Model` trait for native OpenAI API (distinct from OpenRouter)
- **Google/Gemini**: Implement `Model` trait for Google Generative AI API
- Map `ModelCapabilities` (tool_calling, thinking, structured_output, etc.) for each
- Config-driven selection: `provider.kind = "anthropic" | "openai" | "google"`
- Provider-specific auth/config: base_url, api_key, model-specific parameters
- **Out of scope:** Azure, Vertex, Bedrock, etc. — separate milestones.

**Acceptance criteria:**
- ✅ `AnthropicModel` implements `Model` trait with Messages API, native tool calling, SSE streaming, thinking channel
- ✅ `OpenAIModel` implements `Model` trait with Chat Completions API, native function calling, SSE streaming, o1 reasoning support
- ✅ `GoogleModel` implements `Model` trait with Generative AI API, native function calling, SSE streaming
- ✅ All three providers map `ModelCapabilities` correctly
- ✅ Config validation accepts `provider.kind = "anthropic" | "openai" | "google" | "openai-compatible"`
- ✅ Provider-specific base URLs default correctly
- ✅ Secret substitution works with `{env:VAR}` and `{file:path}` for all providers
- ✅ Unit tests for all three providers (construction, error mapping, request/response conversion, cost estimation, streaming)

**Portability follow-up (post-M19):**
- Adapters carry provider-native reasoning (Anthropic signed thinking, OpenAI Responses encrypted reasoning, Gemini thought signatures) as `MessageExtras.reasoning`, replayed only to the originating provider; images ride the same extras (`ImagePart`), and tool results carry `is_error`.
- `GenerationParams` gained `reasoning` (`model.reasoning = "off|low|medium|high"`) and `tool_choice`; `Usage` reports cache read/write and reasoning tokens.
- New `OpenAIResponsesModel` (`provider.wire_api = "responses"`, openai kind only).
- `model::registry` replaces duplicated provider construction; extra providers via `[providers.<name>]` addressed as `model.name = "<name>/<model>"`.
- `assets/models.json` (bundled snapshot generated from models.dev, first-party providers only) feeds capabilities, pricing and tier; a user cache (`okeanos models update`, or a best-effort background refresh when older than 24h) overrides it per id. Opt out with `model.catalog_refresh = false` or `OKEANOS_OFFLINE=1`. Tier comes from capabilities (no tools/<32k/<$1 per M output = Small; >=128k and >=$2 = Frontier; else Mid); name heuristics remain only for models absent from the catalog. Regenerate the snapshot with `okeanos models update --out assets/models.json`. Layering is tier defaults < catalog < explicit config, so explicit `max_context`/`max_output`/`edit_format` always win. Unknown low-confidence models never assume a thinking channel.
- ACP advertises `promptCapabilities.image = true`; resumed sessions keep reasoning blocks and images.
- Known gaps: no vision-capability gating (`ModelCapabilities` has no vision flag); adapters verified by fixtures only, not live APIs.
**Testing requirements:** Unit tests for each provider's request building, response parsing, error mapping, streaming, tool calling, cost estimation. No network (mock HTTP not needed as tests use static responses).

**Definition of Done:** CI green; all three providers verified via unit tests; milestone tagged.
- `cargo fmt --check` passes
- `cargo clippy -- -D warnings` passes
- `cargo test` passes (354 unit tests)
- `cargo build --release` produces a single binary
- Tagged v0.3.0

### Milestone 20 — WebSocket RPC transport

**Status:** complete — tagged v0.4.0

**Scope:** Add WebSocket transport for JSON-RPC (feature-gated).
- `jsonrpsee` WebSocket server/client
- `--rpc-ws` flag for WebSocket mode
- Same event stream as stdio/TCP RPC
- **Out of scope:** Auth/TLS for WebSocket (deferred).

**Acceptance criteria:**
- ✅ `--rpc-ws` flag accepted and starts JSON-RPC WebSocket server
- ✅ WebSocket server listens on configurable address (default 127.0.0.1:0 for random port)
- ✅ Same RPC methods available: `approve`, `deny`, `get_status`, `list_sessions`, `resume`
- ✅ Same event stream as stdio/TCP RPC mode
- ✅ ReAct loop runs identically in WebSocket mode
- ✅ Session management works (resume, list_sessions)
- ✅ Approval handling via RPC `approve`/`deny` methods works
- ✅ `cargo fmt --check` passes
- ✅ `cargo clippy -- -D warnings` passes
- ✅ `cargo test` passes (354 unit tests)
- ✅ `cargo build --release` produces a single binary

**Testing requirements:** Event-parity test across modes; RPC request/response round-trip tests; WebSocket connection test.

**Definition of Done:** CI green; parity test covers every event in DESIGN.md §7; milestone tagged.

### Milestone 21 — MCP client (lazy) ✅
**Scope:** Lazy MCP server handling behind `mcp` feature flag.
- ✅ MCP client trait + stdio transport
- ✅ Tool discovery from MCP servers at runtime
- ✅ Register discovered tools in `ToolRegistry`
- **Out of scope:** MCP server implementation, SSE transport.

**Testing requirements:** Unit tests for McpClient (connect, discover, call); integration test with mock MCP server; config parsing for mcp section; tool name prefixing and description truncation.

**Definition of Done:** CI green; all tests pass with `mcp` feature; milestone tagged.

### Milestone 22 — Session log rotation / compression
**Status:** complete — tagged v0.6.0

**Scope:** Rotate and compress JSONL session logs.
- Configurable max file size / age before rotation
- zstd compression (with long-distance matching) for rotated segments; gzip fallback
- `--resume` works transparently across rotated/compressed segments
- Background worker for automatic compression (startup + periodic)
- Manual `/rotate` command in command palette
- **Out of scope:** Remote log shipping, structured query API.

**Acceptance criteria:**
- Rotation at compaction boundary (primary) and size threshold (safety net)
- Segment naming: `session-{id}.jsonl` (active) → `session-{id}.N.jsonl[.zst|.gz]` (rotated)
- Each segment has header with `segment_index`, `parent_segment`, `is_compressed`
- Background worker compresses eligible segments (≥1 MB, ≥30 min idle, not current)
- File locking prevents compressing active writers
- Auto-cleanup: keeps `keep_rotated_segments` (default 20) and removes segments older than `max_age_days` (default 30)
- `load_session()` walks segment chain, decompresses on-the-fly, returns events in order
- `/rotate` slash command triggers manual rotation

**Testing requirements:** Unit tests for rotation at compaction boundary, manual rotation, resume across compressed segments, compression worker eligibility, file locking. Integration test for long session → rotate → compress → resume.

**Definition of Done:** CI green; rotation at compaction boundary verified; manual rotation via `/rotate` verified; resume across compressed segments verified; background worker compresses eligible segments without corrupting active session; milestone tagged.
- `cargo fmt --check` passes
- `cargo clippy -- -D warnings` passes
- `cargo test` passes (357 unit tests)
- `cargo build --release` produces a single binary
- Rotation at compaction boundary creates new segment with updated headers
- Manual `/rotate` command works in TUI
- `load_session()` correctly replays events across multiple compressed segments
- Background worker respects file locks and only compresses idle segments
- Auto-cleanup removes old segments beyond retention limits
- Tagged v0.6.0

### Milestone 23 — Tool result caching (TTL)

**Status:** complete — tagged v0.7.0

**Scope:** Optional caching for idempotent tool calls.
- TTL-based cache for `read`, `grep` results
- Configurable per-tool TTL; disabled by default
- Invalidate on any `write`/`delete`/`run` in same path
- **Out of scope:** Distributed cache, cache warming.

**Acceptance criteria:**
- Cache enabled via `[tool_cache]` config section
- `read` and `grep` results cached with TTL (default 5 min, configurable per tool)
- Cache invalidated on `write`/`delete`/`run` affecting same path
- Cache disabled by default (opt-in via `tool_cache.enabled = true`)
- File modification detection via hash+mtime invalidates stale entries
- LRU eviction when max entries reached (default 1000)

**Testing requirements:** Unit tests for cache hit/miss, TTL expiry, file modification invalidation, path invalidation on write/delete/run, canonical key stability, non-cacheable tools never cached, max entries eviction, cache disabled by default.

**Definition of Done:** CI green; caching verified end-to-end in headless mode; milestone tagged.
- `cargo fmt --check` passes
- `cargo clippy -- -D warnings` passes
- `cargo test` passes (371 unit tests)
- `cargo build --release` produces a single binary
- Tool result cache module exists at `src/tool/cache.rs`
- Config section `tool_cache` with `enabled`, `default_ttl_secs`, `per_tool_ttl`, `max_entries`
- `read`/`grep` cache hits logged at Debug level
- Cache invalidated on write/delete/run operations
- Cache disabled by default
- Tagged v0.7.0

### Milestone 24 — Per-turn expensive call confirmation

**Status:** complete — tagged v0.8.0

**Scope:** User confirmation before tool calls exceeding cost threshold.
- Configurable per-turn cost warning threshold (USD)
- TUI modal + headless `ApprovalRequest` event
- Skip confirmation for `allow-always` grants
- **Out of scope:** Budget forecasting, predictive cost estimates.

**Acceptance criteria:**
- Config `cost.expensive_call_confirmation_usd` with validation (>= 0)
- CostTracker `requires_expensive_call_confirmation()` checks threshold
- ReAct loop emits `ApprovalRequest` when tool call cost exceeds threshold
- TUI modal + headless event parity (reuses existing approval infrastructure)
- Allow-always grant skips confirmation
- Disabled by default (opt-in)

**Testing requirements:** Unit tests for threshold check, allow-always skip, disabled-by-default. Integration tests for prompt/allow/deny/allow-always flows.

**Definition of Done:** CI green; expensive call confirmation verified end-to-end in headless and TUI modes; milestone tagged.
- `cargo fmt --check` passes
- `cargo clippy -- -D warnings` passes
- `cargo test` passes (382 unit tests)
- `cargo build --release` produces a single binary
- Config section `cost.expensive_call_confirmation_usd` with validation
- CostTracker `requires_expensive_call_confirmation()` method
- ReAct loop integrates check before tool execution
- Allow-always grant bypasses confirmation
- Feature disabled by default (opt-in)
- Tagged v0.8.0

### Milestone 25 — Tree-sitter symbol scanning (optional)
**Scope:** C-based symbol scanner for richer context selection (opt-in).
- Feature-gated `tree-sitter` dependency
- Language-specific symbol extraction
- Fallback to ripgrep when unavailable
- **Out of scope:** Full AST-based refactoring, cross-file analysis.

### Milestone 26 — Rich text / markdown rendering in TUI

**Status:** complete — tagged v0.9.0

**Scope:** Render markdown in conversation pane.
- Feature-gated `pulldown-cmark` + `syntect` (pure Rust) markdown widget
- Code blocks with syntax highlighting
- Configurable: plain / markdown / auto
- **Out of scope:** Image rendering, interactive widgets.

**Acceptance criteria:**
- `cargo build --release --features markdown` compiles; `cargo build --release` (without feature) compiles with plain text fallback
- Assistant messages render with markdown formatting when enabled
- Code blocks show syntax highlighting (keywords, strings, comments colored)
- Config option `ui.markdown_render` works (plain/markdown/auto)
- Theme colors respected throughout
- No C dependencies added (syntect uses fancy-regex pure Rust backend)

**Testing requirements:** Unit tests for markdown widget (headings, code blocks, lists); integration test rendering markdown message in conversation pane; test feature-gated compilation both ways.

**Definition of Done:** CI green; markdown rendering verified end-to-end in TUI and headless modes; feature-gated compilation verified; milestone tagged.
- `cargo fmt --check` passes
- `cargo clippy -- -D warnings` passes
- `cargo test --features markdown` passes (393 tests)
- `cargo test` passes (382 tests without feature)
- `cargo build --release --features markdown` produces a single binary
- `cargo build --release` produces a single binary (plain text fallback)
- Tagged v0.9.0

### Milestone 27 — Multi-agent delegation
**Status:** complete — tagged v0.11.0

**Scope:** Delegate subtasks to sub-agents via `todo` tool extension.
- Sub-agent spawn with isolated context + tool subset
- Parent aggregates results; approval bubbles up
- Configurable delegation depth / token budget
- **Out of scope:** Autonomous agent swarms, cross-project delegation, role-based crew orchestration.

**Definition of Done:** ✅ All verified
- ✅ **Agent profiles**: `.okeanos/agents/*.toml` (project) + `~/.config/okeanos/agents/*.toml` (user) parsed, validated, registered. Profile fields: `description`, `model` (inherit | override), `tools` (allowlist), `disallowed_tools` (denylist), `max_depth`, `token_budget`, `workspace_mode` (inherit | branch | share).
- ✅ **Todo tool extension**: `Delegate { todo_id, agent, prompt }` action added; validates profile, depth, spawn budget; returns `SubAgentHandle`.
- ✅ **SubAgent core**: `SubAgent` struct spawns isolated `ReactLoop` with new `Conversation`, filtered `ToolRegistry`, child `CostTracker`, `depth+1`. Runs as background task.
- ✅ **Workspace modes**: `inherit` (default, shared CWD), `branch` (gix worktree created, cleaned up on completion), `share` (shared directory, path-scoped).
- ✅ **Approval bubbling**: Sub-agent `RequireApproval` → `ApprovalRequest` on parent's `event_tx` → parent UI/RPC handles → `Approval` routed to sub-agent's approval channel → sub-agent resumes.
- ✅ **Event streaming**: `SubAgentProgress` (periodic: tokens, status) and `SubAgentComplete` (final result) emitted on parent's event stream. Parent `Todo.complete(todo_id)` + `ToolResult` injected into parent conversation.
- ✅ **Delegation limits**: `max_depth` enforced at spawn (default 1); global `max_subagents_per_session` (default 64); per-sub-agent `token_budget` via child `CostTracker`.
- ✅ **Model override**: Profile `model = "inherit"` or explicit `provider/model#variant`; resolved at spawn.
- ✅ **FleetView TUI**: Panel showing active sub-agents (id, profile, status, tokens, depth) with steer/stop controls. Background updates via `SubAgentProgress`.
- ✅ **Session log**: Sub-agent events tagged with `subagent_id` in parent's JSONL.
- ✅ **Tests**: Unit tests for profile parsing, spawn validation, tool subset, budget, depth, approval routing. Integration test: delegate → progress → approval → complete → parent continues.
- ✅ **CI gates**: `cargo fmt --check`, `cargo clippy -- -D warnings`, `cargo test` (including new sub-agent tests), `cargo build --release` single binary.
- ✅ **Tagged**: `v0.11.0`
- `cargo fmt --check` passes
- `cargo clippy -- -D warnings` passes
- `cargo test` passes (361 unit tests including FleetView tests)
- `cargo build --release` produces a single binary

### Milestone 28 — Durable & cross-session memory
**Status:** complete — tagged v0.12.0

**Scope:** Give the agent durable context that survives sessions and compaction (FR-44..FR-46).
- Hierarchical `AGENTS.md` / `CLAUDE.md` loading into the system prompt (size-capped, explicit truncation)
- File-based memory store: `MEMORY.md` index + topic files, project and user scope
- `remember` tool (Write category, approval-gated, slugged names)
- `[memory]` config section (all optional; enabled by default)
- **Out of scope:** sub-agent inheritance, background extraction/consolidation, session titles/search/`--continue`, todo-state log events, embeddings/vector search.

**Definition of Done:**
- ✅ Instruction loader: user dir → root → CWD order, `AGENTS.md` preferred over `CLAUDE.md` per dir, byte cap with notice, small tier cap 4 KiB
- ✅ Memory index loader: project + user, 200 line / 8 KiB cap, skipped for small tier
- ✅ `build_system_prompt_with_context` used by all four entry points (interactive, headless, rpc, rpc-ws); existing `build_system_prompt` unchanged
- ✅ `remember` tool registered for mid/frontier; unit tests for slugging, traversal rejection, index upsert, scope selection
- ✅ `[memory]` config (`enabled`, `instructions`, `max_instruction_bytes`, `max_index_lines`) + validation + `okeanos init` template
- ✅ Unit tests with tempdirs (no network)
- ✅ `cargo fmt --check`, `cargo clippy -- -D warnings`, `cargo test`, `cargo build --release` pass (CI green)
- ✅ `Cargo.toml` bumped to `0.12.0`
- ✅ Tagged `v0.12.0`

### Milestone 29 — Execution environment hardening
**Status:** complete — v0.13.0 (tag pending)

**Scope:** Fix correctness/safety gaps found in the execution-environment review (FR-47..FR-49).
- `run`: default 120 s timeout; merged ordered output; raw-byte capture (lossy UTF-8, no stalls on invalid UTF-8); bounded head/tail capture; exit-based completion with bounded drain (grandchild holding a pipe cannot hang the tool); partial output preserved on timeout (previously lost); process-tree kill on timeout/cancel (Unix process group, Windows Job Object — replaces leader-only kill); stdin closed; secret env vars scrubbed.
- Workspace confinement for `write`/`edit`/`delete` (`src/tool/workspace.rs`, `permissions.extra_write_roots`); `.git`/`.env*` protected.
- `Policy::check_call`: command-scoped `run` rules and session grants; risky commands never session-granted.
- Docs corrected: command/arg allow-listing and path scoping are now real; OS sandbox explicitly marked not implemented.
- **Out of scope:** OS sandbox, incremental output events, background processes (M30–M32).

**Definition of Done:**
- ✅ Unit tests: capture bounds, env scrubbing, timeout keeps output + kills tree, stdin closed, non-zero exit; Unix-only: invalid UTF-8, background grandchild; workspace confinement (outside root, `..`, `.git`, `.env`, symlink on Unix); policy rule scoping/risky commands/allow-list rules
- ✅ `cargo clippy --all-targets -- -D warnings` and `cargo test` (417 passed, Windows)
- ⚠️ Unix-only code paths (`killpg`, process group) and tests compile-checked by review only — verify in CI on Linux/macOS/musl
- ✅ `Cargo.toml` bumped to `0.13.0`; `nix` made Unix-only, `windows-sys` added Windows-only
- ☐ Tag `v0.13.0`

### Milestone 30 — Optional OS sandbox
**Status:** complete — v0.14.0 (tag pending)

**Scope:** FR-50. `permissions.sandbox` (`none` default | `read-only` | `workspace-write`) and `permissions.sandbox_network` (default false) behind a `Sandbox` trait (`src/tool/sandbox.rs`), applied to `run`. Linux: `bwrap`; macOS: `sandbox-exec`; Windows: unsupported (fails closed). Spawns external helpers; nothing linked.
- **Out of scope:** Landlock, Windows AppContainer/restricted token, network proxy/allowlist, sandboxing MCP servers.

**Definition of Done:**
- ✅ Config + validation (`sandbox` values), `okeanos init` template comment
- ✅ Unit tests: bwrap argv per mode/network, `.git`/`.env*` protection, Seatbelt profile + escaping, fail-closed when unsupported/missing
- ☐ Linux/macOS execution paths verified in CI (compile + argv tests only on Windows dev host)
- ✅ `Cargo.toml` bumped to `0.14.0`
- ☐ Tag `v0.14.0`

### Milestone 31 — Incremental tool output events
**Status:** complete — v0.15.0 (tag pending)

**Scope:** `Tool::execute_streaming(args, ToolOutputSink)` (default delegates to `execute`); `run` streams UTF-8-safe chunks as they arrive; the ReAct loop emits ephemeral `tool_output` events (`id`, `name`, `seq`, `chunk`, `ts`) before `tool_result`. TUI tool panel shows live output; headless prints `[TOOL OUTPUT]` to stderr and RPC/WS forward the JSON event. Not session-logged; capped per call (256 KiB, then one notice chunk). Final `tool_result` unchanged.
- **Out of scope:** background processes (M32), streaming for parallel read-only batches, MCP tool streaming.

**Definition of Done:**
- ✅ DESIGN.md §7, ARCHITECTURE.md updated first
- ✅ Unit tests: UTF-8 chunk splitting, `run` emits chunks in order and final result unchanged, ReAct loop emits `tool_output` before `tool_result`, session log skips `tool_output`
- ✅ `Cargo.toml` bumped to `0.15.0`
- ☐ Tag `v0.15.0`

### Milestone 32a — Tooling, skills and protocol extensibility
**Status:** complete - v0.16.0

**Scope:** stable (sorted) tool order; line-numbered `read` with offset/limit; `.gitignore`-aware `grep` and new `glob` (pure-Rust `ignore` crate); tier-based `builtin_registry`; built-in no-login MCP servers (context7, gh_grep); MCP overhaul (parallel connect, timeouts, reconnect, spec cache, `tool_search` deferral, `.mcp.json`, Streamable HTTP over rustls/ring, unannotated tools gated as `Network`); skills as progressive disclosure (`SKILL.md` index + `skill` tool); `[[hooks]]` PreToolUse/PostToolUse; `okeanos --acp` (ACP over stdio).
- **Out of scope:** MCP image/audio content (→ M34), OAuth for HTTP MCP servers (→ M35).

**Definition of Done:**
- ✅ ARCHITECTURE.md, PRODUCT_SPEC.md (FR-13/14), AGENTS.md updated
- ✅ Unit tests for skills, hooks, ACP translation, grep/read/glob, tool registry
- ✅ `Cargo.toml` bumped to `0.16.0`; tag `v0.16.0`

### Milestone 32 — Background processes
**Status:** complete — v0.22.0

**Scope:** FR-51. un accepts `background: true` and returns a job id immediately (same approval, guard, sandbox and env scrubbing as foreground un; no timeout unless `timeout_secs` is given). New tools `job_output` (`id`, optional `wait_secs` ≤ 60; returns only output since the last poll plus status; no `id` lists jobs; category Read) and `job_kill` (stops the process tree; category Network, approval-gated). Implemented in `src/tool/builtin/jobs.rs` on the existing process-group / Job Object guard; a supervisor task per job keeps a bounded 256 KiB output tail (dropped bytes are reported). Max 8 running jobs, last 16 finished jobs kept; jobs are killed when the session's runtime shuts down. `job_output`/`job_kill` are registered for Mid/Frontier tiers only (Small keeps read/edit/run).
- **Out of scope:** job persistence across sessions/resume, stdin to jobs, TUI job panel, per-job sandbox/env overrides.

**Definition of Done:**
- ✅ ARCHITECTURE.md, PRODUCT_SPEC.md (FR-51) updated first
- ✅ Unit tests: immediate return + incremental output, kill, explicit timeout, unknown id, running-job cap, dropped-byte notice, argument validation
- ✅ `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, `cargo test` (525 passed, Windows)
- ✅ Unix process-group paths verified in CI (Linux/macOS/musl)
- ✅ `Cargo.toml` bumped to `0.22.0`
- ✅ Tag `v0.22.0`

### Execution-environment roadmap (planned, from the M29 review)
Compared against Codex (Seatbelt/bwrap/Landlock/restricted tokens, approval × sandbox modes, exec-policy prefix rules), Claude Code (prefix rules, compound-command parsing, optional sandbox + network proxy, background tasks), OpenCode (per-command glob permissions) and Pi (no permissions, container assumed; tail truncation + spill).

| Milestone | Deliverable | Notes |
|-----------|-------------|-------|
| M30 | Optional OS sandbox: `sandbox = none \| read-only \| workspace-write`, network off by default, behind a `Sandbox` trait independent of approvals. Linux: bwrap/Landlock; macOS: `sandbox-exec`; Windows: restricted token/AppContainer. | Minor bump. Must keep musl/static rule (no C deps) — prefer spawning `bwrap`/`sandbox-exec` over linking. |
| M31 | Incremental output: `Tool` trait gains an output-event channel; `run` streams chunks as `ToolOutput` events (TUI + headless parity per DESIGN.md §7). | AGENTS.md already requires streaming; currently output is returned at completion. Needs DESIGN.md §7 update first. |
| M32 | Background processes: `run` `background: true` + poll/kill handle; Job Object/process group reuse. | Needed for dev servers/watchers. |
| M33 | Policy depth: file-based exec-policy rules (allow/ask/deny, prefix/glob), compound-command parsing for shell-style invocations, env allowlist mode, dedicated `Process` tool category (today `run` is categorised `Network`), persistent shell session (optional). | Config schema change → document in ARCHITECTURE config section. |
| M34 | Multimodal MCP results: map MCP image/audio content blocks to model image inputs (instead of flattening to text) for providers that support them; text placeholder otherwise. | Depends on Message content-part support in the Model trait; capability-gated per DESIGN of ModelCapabilities. |
| M35 | MCP OAuth for Streamable HTTP servers: rmcp auth flow, token store (0600), refresh, okeanos mcp login <server>. | Secrets stay out of config; must stay pure Rust (no C deps). |

### Maintainability milestone (planned, next up)

| Milestone | Deliverable | Notes |
|-----------|-------------|-------|
| M36 | Split the two monoliths with **no behaviour change**. `src/main.rs` (~3.8k lines) → thin entrypoint + `src/commands/` modules (`doctor`, `sessions`, `stats_export_replay`, `models`, `trust`, `init`) and run-mode modules (`interactive`, `headless`, `acp`, `rpc`, `rpc_ws`) plus shared wiring (`build_active_model`, `build_policy`, `build_hooks`, MCP/skill/memory registration). `src/harness/react.rs` (~3.2k lines) → `harness/react/` (`mod.rs` loop + config, `dispatch.rs` serial tool execution, `parallel.rs` parallel read batch, `authorize.rs` policy/hooks/approval, `budget.rs` cost and expensive-call checks, `compaction_glue.rs`). | Pure moves: public API (`okeanos::harness::ReactLoop`, etc.) re-exported unchanged; no new deps; `cargo fmt`, `clippy -D warnings` and the full test suite must pass identically before and after, with tests moved beside their code. Do it in small commits (one module per commit) so each is reviewable. Patch bump (`0.x.y`), since nothing user-facing changes. |
| M37 | **⏸ Deferred (revisit if Windows users ask for it): Windows sandbox spike → opt-in backend.** Rationale: green real-process tests only prove the attacks we thought of, a half-trusted native sandbox can give false confidence, and WSL2/containers already give Windows users the bwrap sandbox. Until then Windows `run` is guarded by approvals and path checks only, and `doctor`/the refusal message say so. Design kept below for later. Phase 1 (time-boxed spike): prototype a `Sandbox` impl for Windows behind the existing trait, using a restricted token (disabled SIDs, low integrity level) + the existing Job Object, and/or AppContainer with explicit ACL grants on the workspace roots. Phase 2 (only if the spike passes): ship it as an opt-in backend for `permissions.sandbox = "read-only" \| "workspace-write"`, with `doctor` reporting availability. | **Exit criteria for the spike** (real child processes on `windows-latest`, no mocks): writes outside the workspace roots fail; writes inside succeed; `.git`/`.env*` stay read-only; network is blocked unless `sandbox_network = true`; the process tree dies with the job. If any fails or ACL grants are flaky/leave residue, abandon: keep today's "refuse to run" behaviour and document WSL/container as the Windows path. Unsafe FFI confined to one module (`src/tool/sandbox/windows.rs`) with `// SAFETY:` comments and RAII handle wrappers. Minor bump if shipped; no bump if abandoned. |

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
| 17 | ✅ Config layer cleanup | 1 | M1 |

**Post-MVP (independent, can parallelize):**
| Milestone | Deliverable | Sessions (est.) | Depends on |
|-----------|-------------|-----------------|------------|
| 18 | ✅ Context compaction | 2 | M16 |
| 19 | ✅ Additional providers (Anthropic, OpenAI, Google) | 2–3 | M16 |
| 20 | ✅ WebSocket RPC | 1 | M14 |
| 21 | ✅ MCP client | 2–3 | M16 |
| 22 | ✅ Session log rotation | 1 | M13 |
| 23 | ✅ Tool result caching | 1–2 | M16 |
| 24 | ✅ Expensive call confirmation | 1 | M10, M16 |
| 25 | Tree-sitter scanning (opt-in) | 2 | M16 |
| 26 | Markdown rendering | 1–2 | M12 |
| 27 | ✅ Multi-agent delegation | 5–6 | M16, M13 |
| 28 | ✅ Durable & cross-session memory | 2 | M16, M18 |
| 29 | ✅ Execution environment hardening | 1–2 | M6, M5 |
| 30 | ✅ OS sandbox (optional) | 3 | M29 |
| 31 | ✅ Incremental tool output events | 1–2 | M29, M12 |
| 32 | ✅ Background processes | 1–2 | M29 |
| 33 | Policy depth (exec-policy rules, env allowlist) | 2 | M29 |
| 34 | Multimodal MCP results | 1–2 | M32a |
| 35 | MCP OAuth (Streamable HTTP) | 2 | M32a |
| 36 | Split `main.rs` / `react.rs` (no behaviour change) | 2–3 | — |
| 37 | ⏸ Deferred: Windows sandbox spike (opt-in backend if it passes) | 2–3 | M30 |

---

## Notes on milestone structure

**MVP ends at M16.** Everything M18+ is genuinely post-MVP and can be tackled in any order — they are independent lanes. The table above lists dependencies but most can run in parallel once M16 is done.

**M17 (Config cleanup)** is architectural hygiene; it can happen anytime after M16 or in parallel with post-MVP work.

**Post-MVP milestones are small by design** — 1–3 sessions each, single testable deliverable. No "big bang" milestones.

---

## Versioning Strategy

### MVP Release (Completed)
- **`Cargo.toml` version**: `0.1.0`
- **MVP features complete**: M16 (MVP completion), M17 (Config cleanup) ✅
- **MVP release tag**: `v0.1.0` — not yet tagged (pending final release prep)
- **Historical pre-MVP milestone tags**: `v0.1.0-milestone-N` — used for M0–M17

### Post-MVP: Semantic Versioning (Cargo Convention)
Since `0.y.z` means "any breaking change at any time" in Cargo semver, we stay in `0.x` until API/CLI/config are genuinely stable.

| Phase | Cargo Version | Git Tag | Trigger |
|-------|---------------|---------|---------|
| **MVP Release** | `0.1.0` | `v0.1.0` | Pending: final CI green, release artifacts |
| **Post-MVP feature milestones** | `0.2.0`, `0.3.0`, `0.4.0`… | `v0.2.0`, `v0.3.0`, `v0.4.0`… | Each independent milestone (M18, M19, M20…) |
| **Breaking changes** (CLI flags, config schema, public Rust API) | Minor bump (`0.x.0`) | `v0.x.0` | Any user-facing breaking change |
| **Bug fixes only** | Patch bump (`0.x.y`) | `v0.x.y` | Non-breaking fixes |
| **Stability commitment** | `1.0.0` | `v1.0.0` | When willing to guarantee semver stability |

### Milestone → Version Mapping (Post-MVP)
| Milestone | Version | Notes |
|-----------|---------|-------|
| M18 Context compaction | `0.2.0` | First post-MVP feature |
| M19 Additional providers | `0.3.0` | Can be parallel with M18 → combine if shipped together |
| M20 WebSocket RPC | `0.4.0` | ✅ Complete |
| M21 MCP client | `0.5.0` | ✅ Complete |
| M22 Session log rotation | `0.6.0` | ✅ Complete |
| M23 Tool result caching | `0.7.0` | |
| M24 Expensive call confirmation | `0.8.0` | ✅ Complete |
| M25 Tree-sitter scanning | `0.9.0` | |
| M26 Markdown rendering | `0.10.0` | |
| M27 Multi-agent delegation | `0.11.0` | ✅ Complete |
| M28 Durable & cross-session memory | `0.12.0` | ✅ Complete. Minor bump: new feature, additive only (new optional `[memory]` config, new `remember` tool, system prompt gains project instructions). No breaking CLI/config/API change, so not a major/patch bump. |
| … | `0.x.0` | One minor per independent feature milestone |
| M29 Execution environment hardening | `0.13.0` | ✅ Complete. Minor bump (pre-1.0 convention for user-visible behaviour change): `run` now defaults to a 120 s timeout, scrubs secret env vars and closes stdin; `write`/`edit`/`delete` refuse paths outside the project root/`.git`/`.env*`; `allow-always` on `run` is command-scoped. New optional `permissions.extra_write_roots`. Not a patch bump because behaviour changes; not 1.0 because still pre-stability. |
| M30 OS sandbox | `0.14.0` | ✅ Complete. Minor bump: additive, opt-in (`permissions.sandbox`, default `none`). |
| M31 Incremental tool output | `0.15.0` | ✅ Complete. Minor bump: additive `tool_output` event; consumers matching `Event` exhaustively must handle it. |
| M32–M35 | `0.16.0`+ | Planned, one minor each |
| M36 Monolith split | patch bump | Planned, next up. Refactor only; no user-visible change. |
| M37 Windows sandbox spike | minor bump if shipped | Deferred; spike with explicit abandon criteria if revived. |

**If multiple milestones land in one release cycle**, combine into a single minor bump (e.g., M18+M19 together → `0.2.0`).

### Release Process
1. Complete milestone → all tests pass → CI green
2. Update `Cargo.toml` version (minor bump for features, patch for fixes)
3. Update `CHANGELOG.md` (if one exists) or rely on milestone tags
4. `git tag vX.Y.Z`
5. Push tag → CI builds release artifacts (musl binary, macOS binaries)
6. Optional: `cargo publish` to crates.io when ready for public consumption

### Rationale
- **`0.x` series** = explicit "not yet stable" signal to users (Cargo convention)
- **Minor per milestone** = each post-MVP milestone adds meaningful user-facing functionality
- **No patch versions pre-1.0** unless users depend on specific behavior
- **Tags match versions** = `git tag v0.2.0` for `0.2.0` release
- **Milestone tags preserved** = `v0.1.0-milestone-N` remain as internal checkpoints