# AGENTS.md — Agent Instructions

Concise rules for AI-assisted work on this repo. Docs-only project at this phase; implementation follows PRODUCT_SPEC.md / ARCHITECTURE.md / MILESTONES.md / DESIGN.md.

## Coding standards

- **Docs first, code second.** No implementation code without a corresponding spec section updated first (PRODUCT_SPEC.md, ARCHITECTURE.md, or MILESTONES.md).
- **Rust 2024 edition.** `cargo fmt --check` and `cargo clippy -- -D warnings` must pass before any change is considered done.
- **Dependency rules (ARCHITECTURE.md):** harness `lib` never depends on `ui`, `headless`, or `cli`. No C dependencies in the primary tree (no git2, openssl, rapidjson). reqwest uses rustls. gix for git.
- **Feature flags:** `rpc` (jsonrpsee), `mcp` (deferred), `mock-provider` (test-only), `simd-json` (deferred opt). Do not add dependencies without updating ARCHITECTURE.md dependency rules.
- **Error handling:** `thiserror` for library error types; `anyhow` in binary/main and ReAct loop with `.context()`. Every user-facing error must be clear and actionable — no silent truncation, especially at the context cap.
- **Types over strings:** `Model`, `Tool`, `Policy`, `ApprovalDecision` are traits/enums. Mock provider in tests only; no network in unit tests.
- **Model-agnostic by design:** The `Model` trait is text-in/text-out as the primitive; native JSON tool calling is an optional capability (`supports_tool_calling`), not a requirement. Never assume a model can emit structured tool calls.
- **Model capabilities:** `ModelCapabilities` (tool_calling, structured_output, thinking, max_context, max_output, temperature_supported, vision, tier) must be explicitly declared per model — via provider discovery or explicit TOML override for local/self-hosted models. Never guess capabilities for self-hosted models; fail closed on missing required capability.
- **Text tool parsing:** Raw model output is parsed into tool invocations leniently, then validated against the tool schema **before** execution. Invalid calls return structured errors; never execute unvalidated calls.
- **Tiered prompts & tools:** Small local models (4k–8k context) get a short system prompt (<1k tokens) and minimal tool set (read, edit, run, plus read-only grep/glob); tool lists are never hard-coded in prompts and `tool::builtin_registry` is the only place that decides tier tool sets; frontier models get the full prompt and full tool set. Tier is declared in `ModelCapabilities`.
- **Selective context (no C deps):** File inclusion and symbol scanning use pure Rust (ripgrep) only. No tree-sitter or other C-based scanners.
- **Thinking channel:** Optional; captured separately from visible output. Not replayed into context for small-context models; replayed only for large-context frontier models.
- **Fallback strategies:** On malformed/failed tool call: validate before execution, retry once with simpler prompt + reduced tool set, then escalate to user or skip turn. Hard retry cap per turn.
- **Edit format:** Diff/patch default for frontier models; whole-file/str_replace fallback for small local models; per-model edit-format hint in config; preflight-validate patches before applying; never silently truncate or partially apply.
- **Interruptible tool execution:** Long-running tools stream output incrementally; cancellation (SIGINT/timeout) uses SIGTERM→grace→SIGKILL escalation; output is drained after exit; head/tail truncation counts against the context cap; return exit_code/timed_out/cancelled in tool result.
- **Cost budget:** Opt-in hard session cost cap (USD) + per-turn soft warning; refuse new tool calls exceeding cap; fail closed ($0 + warning) on unknown pricing; cap disabled by default at MVP.
- **Context warnings:** Per-turn current/total tokens in status bar; warn at ~10–15% remaining; clear error (not crash) at hard cap; no silent truncation.
- **Tool result caching (M23):** Only a TTL cache for read-only tools (`src/tool/cache.rs`); writes invalidate it. All other calls re-execute.
- **Secrets:** Never store API keys or secrets in config files committed to version control. Use `{env:VAR}` or `{file:path}` substitution. Config load must fail closed if a required secret is missing.
- **Shutdown:** Signal handlers (SIGINT/SIGTERM) must flush the JSONL session log, teardown the TUI alternate screen, and drain in-flight work before exit. No subsystem may block shutdown indefinitely.
- **Token counting:** Token counts and cost estimates are estimates, not provider-authoritative. Label them clearly in the UI and logs.
- **Session resume:** The JSONL session log is the source of truth for resume. `--resume <id>` restores conversation, todos, and plan state. Corrupted trailing lines must be tolerated.
- **Durable memory (M28):** `AGENTS.md`/`CLAUDE.md` and the `MEMORY.md` index are loaded once at startup into the system prompt (byte-stable for prompt caching), always size-capped with an explicit truncation notice. Memory writes go only through the `remember` tool (approval-gated, slugged names, confined to the memory dirs). No embeddings/vector stores.
- **No visual deviation:** TUI colors, layout regions, tokens, and the §7 event map (DESIGN.md) are the visual spec. Changes require design review; do not implement placeholder UI.

## Documentation rules

- Every functional requirement (FR-*) and non-functional requirement (NF-*) in PRODUCT_SPEC.md maps to at least one test.
- User-facing strings: externalized, reviewed for copy; copy changes are mechanical only — do not alter visuals.
- When a milestone completes, update MILESTONES.md Definition of Done and tag the release.
- Keep docs concise: prefer one page per phase (already done). Do not duplicate spec content across files.
- **Versioning**: see MILESTONES.md "Versioning Strategy" — pre-MVP uses `v0.1.0-milestone-N` tags; MVP release = `0.1.0`/`v0.1.0`; post-MVP minor bump per feature milestone (`0.2.0`, `0.3.0`…); stay in `0.x` until stability commitment.

## Testing requirements

- Unit tests: ReAct loop, approval policy, context cap, tool execution, snapshot undo/redo, model capability negotiation, text tool parsing, tier selection, edit format validation, tool execution interrupt/cancel — mock provider, no network.
- Integration tests: CLI modes, headless JSON event parity with TUI events, session log append, snapshot round-trip, small-model text-based tool calling, interruptible tool execution, cost cap enforcement.
- TUI tests: golden-frame snapshots at 80×24 and 120×40, resize reflow, contrast audit (WCAG AA/AAA), reduced-motion, event-parity (every TUI state → JSON event).
- CI runs: `cargo test`, `cargo clippy -- -D warnings`, `cargo fmt --check`, musl static build (`--target x86_64-unknown-linux-musl`), config validation (invalid TOML fails CI).

## Scope-control rules

- **MVP is fixed:** every feature in PRODUCT_SPEC.md (FR-01 through FR-35) is in scope across the 14 milestones — none deferred to post-MVP. Genuinely post-MVP items: context compaction/summarization, multi-agent crew, lazy MCP client, other model providers, WebSocket RPC, compaction-aware long-context handling, compressed/rotated session logs, tree-sitter/C-based symbol scanners, tool result caching, per-turn confirmation for expensive calls.
- **Config layers:** Global (XDG) config loads first, project-local config overrides. Walk up from CWD to git root for project config. Do not add a third config source without updating ARCHITECTURE.md.
- **Tool output truncation:** Every tool result has a max byte limit; truncation uses head/tail with an overflow indicator and counts against the context cap. Do not silently drop large outputs.
- **One milestone at a time.** Do not start milestone N+1 scope in milestone N (e.g., no git snapshots in milestone 2).
- **Hard context cap is intentional at MVP.** Do not add compaction/summarization to work around it; raise the cap via config only after MVP.
- **Visuals are integral, not post-launch.** No temporary placeholder TUI; DESIGN.md is implemented from milestone 2.

## Architecture compliance rules

- **Public vs private state:** public event structs (`Message`, `ToolCall`, `Approval`, `Todo`, `Status`) are shared; internal retry counters, scroll offsets, and auth tokens stay private to their subsystem.
- **Single source of truth:** the ReAct loop owns task progress; TUI and headless are renderers of the same event stream (DESIGN.md §7). Never build a parallel state machine in the UI.
- **Permission boundary:** every write/delete/network/process call passes through `Policy::check`; tools never bypass it.
- **Approval pauses the loop:** the ReAct loop stops on an approval request and resumes only on an `Approval` event.
- **Static binary:** keep the dependency tree pure Rust for musl/static builds; verify with `cargo build --release --target x86_64-unknown-linux-musl` in CI.
