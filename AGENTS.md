# AGENTS.md

Rules for AI-assisted work on okeanos. Read [docs/architecture.md](docs/architecture.md) first; requirements are in [docs/design/product-spec.md](docs/design/product-spec.md) and the visual spec in [docs/design/tui-design.md](docs/design/tui-design.md).

## Workflow

- **Spec first.** Behaviour changes update the relevant doc (product-spec, architecture or tui-design) in the same change.
- **Done means green.** `cargo fmt --check`, `cargo clippy --all-targets --all-features -- -D warnings` and `cargo test` must pass.
- **Small changes.** One milestone or concern at a time; don't mix refactors with behaviour changes.
- **Docs hygiene.** Don't duplicate spec content across files. Update [CHANGELOG.md](CHANGELOG.md) for user-visible changes and [docs/roadmap.md](docs/roadmap.md) when a milestone ships. Versioning rules are in the roadmap.

## Code standards

- **Rust 2024 edition.**
- **Dependencies:** the harness `lib` never depends on `ui`, `headless` or `cli`. No C dependencies in the primary tree (no `git2`, OpenSSL, tree-sitter); `reqwest` uses rustls and git access uses `gix`. Keep the tree pure Rust so musl static builds work. Update the dependency rules in architecture.md when adding a dependency.
- **Feature flags:** `tui` and `config-file` (default); `rpc`, `mcp`, `otel`, `markdown`, `simd-json`; `mock-provider` is test-only.
- **Errors:** `thiserror` for library error types; `anyhow` with `.context()` in the binary and ReAct loop. Errors must be clear and actionable; never truncate silently, especially at the context cap.
- **Types over strings:** `Model`, `Tool`, `Policy` and `ApprovalDecision` are traits or enums. Use the mock provider in tests; unit tests never touch the network.
- **Secrets:** never put API keys in committed config. Use `{env:VAR}` or `{file:path}`. Config loading fails closed when a required secret is missing.

## Model handling

- **Model-agnostic.** The `Model` trait is text-in/text-out. Native JSON tool calling is an optional capability, never assumed.
- **Capabilities.** `ModelCapabilities` (tool calling, structured output, thinking, max context, max output, temperature, vision, tier) must be declared per model, via provider discovery or TOML override. Don't guess for self-hosted models; fail closed on a missing required capability.
- **Tool calls.** Parse model output leniently, then validate against the tool schema **before** executing. Invalid calls return a structured error; never execute unvalidated calls.
- **Tiers.** Small models (4k–8k context) get a short system prompt (<1k tokens) and a minimal tool set (read, edit, run, plus read-only grep/glob); frontier models get the full prompt and tool set. `tool::builtin_registry` is the only place that decides tier tool sets, and prompts never hard-code tool lists.
- **Context.** Selective file inclusion uses pure-Rust scanning. Warn at ~10–15% remaining context and raise a clear error (not a crash) at the hard cap.
- **Thinking.** Captured separately from visible output. Replayed into context only for large-context frontier models.
- **Fallbacks.** On a malformed or failed tool call: validate, retry once with a simpler prompt and reduced tool set, then escalate to the user or skip the turn. Retries are capped per turn.
- **Edits.** Diff/patch by default; whole-file or `str_replace` for small models; per-model edit-format hint in config. Preflight-validate patches; never truncate or apply partially.
- **Cost.** An opt-in hard session cap (USD) plus a per-turn soft warning. Refuse tool calls that would exceed the cap. Unknown pricing fails closed ($0 plus a warning). Token counts and costs are estimates and must be labelled as such.

## Tools and execution

- **Permission boundary.** Every write, delete, network or process call goes through `Policy::check`; tools never bypass it. Approval pauses the ReAct loop until an `Approval` event arrives.
- **Interruptible execution.** Long-running tools stream output incrementally. Cancellation escalates SIGTERM → grace period → SIGKILL, and output is drained after exit. Results include `exit_code`, `timed_out` and `cancelled`.
- **Output limits.** Every tool result has a max byte size; head/tail truncation with an overflow indicator, and truncated output counts against the context cap.
- **Caching.** Only a TTL cache for read-only tools (`src/tool/cache.rs`); writes invalidate it. All other calls re-execute.
- **Memory.** `AGENTS.md`/`CLAUDE.md` and the `MEMORY.md` index are loaded once at startup into the system prompt (byte-stable for prompt caching) with a size cap and an explicit truncation notice. Memory writes go only through the approval-gated `remember` tool, confined to the memory directories. No embeddings or vector stores.

## Architecture

- **Config layers:** global (XDG) config loads first, project config overrides it, found by walking up from the CWD to the git root. Don't add a third source without updating architecture.md.
- **Single source of truth:** the ReAct loop owns task progress. The TUI and headless/RPC modes are renderers of the same event stream (tui-design.md §7); never build a parallel state machine in the UI.
- **Shared vs private state:** public event structs (`Message`, `ToolCall`, `Approval`, `Todo`, `Status`) are shared; retry counters, scroll offsets and auth tokens stay private to their subsystem.
- **Sessions:** the JSONL session log is the source of truth for `--resume` (conversation, todos, plan state). Tolerate corrupted trailing lines.
- **Shutdown:** SIGINT/SIGTERM must flush the session log, restore the terminal and drain in-flight work. No subsystem may block shutdown indefinitely.
- **Visuals:** TUI colors, layout, tokens and the event map in tui-design.md are the visual spec. Changes need design review; don't add placeholder UI.

## Testing

- **Unit:** ReAct loop, approval policy, context cap, tool execution and interruption, snapshot undo/redo, capability negotiation, text tool parsing, tier selection, edit validation. Mock provider, no network.
- **Integration:** CLI modes, headless/TUI event parity, session log append, snapshot round-trip, small-model text tool calling, cost cap.
- **TUI:** golden-frame snapshots at 80×24 and 120×40, resize reflow, contrast audit (WCAG AA/AAA), reduced motion, event parity (every TUI state has a matching JSON event).
- **CI:** fmt, clippy, tests across operating systems, musl static build, and config validation.
- Every functional requirement (FR-*) in product-spec.md should map to at least one test.
