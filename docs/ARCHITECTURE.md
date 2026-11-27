# ARCHITECTURE.md — Rust CLI TUI Agent Harness

**Status:** Phase 4 — architecture (docs only, no implementation code)  
**Owner:** Workflow manager / architecture  
**Related:** PRODUCT_SPEC.md, DESIGN.md, Phase 2 tech-selection research

---

## Major subsystems

| Subsystem | Responsibility | Key traits/interfaces |
|-----------|----------------|-----------------------|
| **CLI entry** (`src/main.rs`) | Argument parsing (clap), mode selection (interactive/headless/RPC), binary startup/shutdown, signal handling (tokio `signal`), graceful shutdown orchestration. | `Cli` (clap derive), `Mode` enum, `ShutdownHandler` |
| **Config** (`src/config/`) | XDG global config + project-local config walk-up, TOML parsing, env var / file secret substitution, defaults, validation. | `Config`, `ConfigLayer`, `SecretRef` |
| **ReAct loop** (`src/harness/react.rs`) | Core control flow: think → call tool → observe → repeat; context-cap enforcement; bounded retries; error-in-context. | `Harness`, `ReactLoop`, `LoopDecision` |
| **Model abstraction** (`src/model/`) | Provider-agnostic chat completions + streaming; capability negotiation; OpenAI-compatible/OpenRouter at launch; mock provider for tests. | `Model` trait, `ModelCapabilities`, `OpenAICompatible` provider, `MockModel` |
| **Tool system** (`src/tool/`) | JSON-schema function calling; text-based tool calling for models without native FC; tool registry; execution with permission checks; lenient parse + validate-before-execute; edit format (diff/patch + whole-file fallback); interruptible tool execution. | `Tool` trait, `ToolSpec` (JSON schema), `ToolRegistry`, `TextToolParser`, `EditFormat`, `ToolExecutor` |
| **Permission layer** (`src/perm/`) | Safe-by-default boundary: allow-list, confirm-before-write/delete/network, argument-aware `run` rules and command-scoped session grants (M29). Optional OS sandbox for `run` (M30, see below). | `Policy`, `Permission`, `ApprovalDecision` |
| **Tool execution** (`src/tool/execute/`) | Interruptible local process execution; default timeout; merged ordered output with bounded head/tail capture; process-tree kill (Unix process group / Windows Job Object); exit-based completion with bounded drain; scrubbed environment; closed stdin; head/tail output truncation counted vs cap. Currently implemented in `src/tool/builtin/run.rs`. Incremental output (M31): `Tool::execute_streaming(args, ToolOutputSink)` (default delegates to `execute`); the ReAct loop forwards sink chunks as ephemeral `ToolOutput` events (not session-logged, capped per call). | `ToolExecutor`, `ProcessHandle`, `StreamBuffer` |
| **Workspace confinement** (`src/tool/workspace.rs`) | Process-wide writable roots (project root + `permissions.extra_write_roots`); canonicalizes through symlinks, rejects `..` escapes, protects `.git` and `.env*`; checked by `write`/`edit`/`delete` before any mutation (M29). | `set_roots`, `guard_write` |
| **File IO** (`src/tool/builtin/fsio.rs`) | `write`/`edit` write atomically (temp file + fsync + rename, symlinks written through), run blocking IO off the async executor, preserve BOM/uniform CRLF, refuse non-UTF-8 files and files over 20 MiB, and reject modifying a file that changed since the model last `read` it (content-hash stamp). | `atomic_write`, `check_unchanged` |
| **Provider HTTP** (`src/model/http.rs`) | One client builder with a 30 s connect timeout and a 120 s per-read idle timeout (`OKEANOS_HTTP_CONNECT_TIMEOUT_SECS` / `OKEANOS_HTTP_READ_TIMEOUT_SECS`); a stalled stream surfaces as a retryable `StreamInterrupted`. | `provider_client` |
| **Background jobs** (`src/tool/builtin/jobs.rs`) | M32. `run` with `background: true` spawns through the same guard/sandbox/env-scrubbing path and returns a job id; a supervisor task per job keeps a bounded output tail. `job_output` (Read) polls new output/status, `job_kill` (Process, approval-gated) stops the tree. Max 8 running jobs; jobs die with the runtime. Mid/Frontier tiers only. | `JobManager`, `JobOutputTool`, `JobKillTool` |
| **OS sandbox** (`src/tool/sandbox.rs`) | Optional (M30), independent of approvals. `permissions.sandbox = none \| read-only \| workspace-write` (default `none`), `permissions.sandbox_network` (default `false`). `Sandbox` trait rewrites the `run` command line to spawn through an external helper — Linux: `bwrap`; macOS: `sandbox-exec` (Seatbelt); Windows: unsupported, so an enabled sandbox fails closed. Nothing is linked (no C deps; musl-safe). | `Sandbox`, `SandboxMode`, `SandboxPolicy`, `set_policy` |
| **Edit format** (`src/tool/edit/`) | Diff/patch default for frontier; whole-file/str_replace fallback for small models; per-model edit-format hint; preflight-validate patches before applying; never silently truncate or partially apply. | `EditFormat`, `PatchValidator`, `PatchApplier` |
| **Cost budget** (`src/cost/`) | Opt-in hard session cost cap (USD); per-turn soft warning threshold; fail closed on unknown pricing; refuse new tool calls exceeding cap; let in-flight work finish. | `CostBudget`, `CostTracker` |
| **Conversation & context** (`src/context/`) | Message history, tiered system prompt + `skills/` concatenation, hard context cap, context-cap error, approximate token counting, cost estimation, selective file inclusion, history-turn capping per tier, context window warnings before cap. | `Conversation`, `ContextCap`, `TokenCounter`, `CostEstimate`, `Tier`, `SelectiveContext`, `ContextWarnings` |
| **Durable memory** (`src/context/memory.rs`, `src/tool/builtin/remember.rs`) | Cross-session context (M28): hierarchical `AGENTS.md`/`CLAUDE.md` instruction loading (user dir → project root → CWD), size-capped with explicit truncation notices; file-based memory store (`MEMORY.md` index + topic files, project and user scope) with the index injected into the system prompt; `remember` tool writes through the permission layer. All loading happens once at startup, synchronously, from a handful of small files; no embeddings, no index service. | `MemoryConfig`, `InstructionFile`, `MemoryIndex`, `ProjectContext`, `RememberTool` |
| **Session resume** (`src/session/`) | Append-only JSONL log, session header (id, cwd, version), resume by session ID, project-scoped session directory, corrupted-line tolerance. | `SessionLog`, `SessionHeader`, `ResumeStore` |
| **Session log** (`src/session/`) | Append-only JSONL log of messages, tool calls, results, approvals, snapshot events; session header with id/cwd/version; resume by session ID; project-scoped storage. | `SessionLog`, `SessionHeader`, `ResumeStore` |
| **Git snapshots** (`src/snapshot/`) | gix-based git snapshots before destructive actions and on `/undo`/`/redo` boundaries; snapshot store. | `SnapshotStore`, `UndoRedo` |
| **Sub-agent delegation** (`src/harness/subagent.rs`, `src/tool/agent.rs`) | Dedicated `agent` / `wait_agent` / `exit_plan` tools (schemas in `src/tool/agent.rs`, handled by `ReactLoop`); `todo.delegate` kept as a background-spawn alias. Built-in `explore`/`plan`/`general` profiles (overridable via user/project profiles). Per-sub-agent ReAct loop with isolated context, tool subset, real model resolution, turn/token limits, depth limit; concurrency cap (4); approval routing by `<subagent_id>/` id prefix; worktree cleanup (changed `branch` worktrees are committed to `okeanos/<id>` first); foreground results returned as the tool result, background results delivered as `<subagent-result>` user messages and awaited before a turn completes; child spend charged to the parent's `CostTracker`. | `SubAgent`, `AgentProfile`, `AgentRegistry`, `SubAgentHandle`, `SubAgentProgress`, `SubAgentComplete`, `WorkspaceHandle` |
| **TUI** (`src/ui/`) | Terminal rendering per DESIGN.md; ratatui + crossterm; themes from `assets/themes/`. | `TerminalApp`, `Theme`, components (conversation, tool panel, modal, status bar, FleetView, …) |
| **Headless/RPC** (`src/headless.rs`) | JSON event emitter shared with TUI; stdio or JSON-RPC transport; identical event stream to TUI (DESIGN.md §7). | `HeadlessDriver`, `JsonRpcServer` (feature-gated) |
| **Logging** | `tracing` spans/events; JSON subscriber via tracing-subscriber; `RUST_LOG` filtering. | — |

## Ownership boundaries

- **Harness owns:** ReAct loop state, context cap, retry policy, dispatch to tools/models, approval gating. It is the single source of truth for task progress.
- **Model owns:** Provider-specific request/response shaping, auth headers, SSE stream parsing, capability negotiation, text-based tool output parsing. The harness never touches provider internals directly.
- **Model capabilities own:** Declared per-model capabilities (tool_calling, structured_output, thinking, max_context, max_output, temperature_supported). Explicit TOML override for local/self-hosted models; fail closed on missing required capability.
- **Text tool parsing owns:** Parsing raw model output into tool invocations for models without native function calling; lenient repair of common quote/bracket errors.
- **Selective context owns:** Choosing which files to include based on task relevance (symbol scanning via ripgrep, pure Rust); capping history turns per model tier.
- **Edit format owns:** Diff/patch vs whole-file decision per model tier; preflight validation of patches before applying; never silently truncates or partially applies.
- **Tool execution owns:** Streaming output capture, timeout enforcement, cancellation signal handling (SIGTERM→grace→SIGKILL), drain-after-exit; no tool execution blocks the ReAct loop indefinitely.
- **Cost budget owns:** Session cost cap enforcement (USD), per-turn soft warnings, fail-closed on unknown pricing; refuses new tool calls exceeding cap, lets in-flight work finish.
- **Tool owns:** Its own JSON schema, argument validation, execution, and result shaping. The permission layer wraps tool execution; tools do not bypass it.
- **Permission layer owns:** The allow-list and approval decisions. No tool executes a write/delete/network operation without policy approval.
- **Session log owns:** Append-only durability. It is write-only after creation; snapshots reference log positions.
- **Snapshot store owns:** Git object writes via gix; undo/redo cursor. It never mutates the working tree outside policy-approved operations.
- **Sub-agent delegation owns:** Agent profile registry (project + user config), sub-agent spawning lifecycle, per-sub-agent `ReactLoop` instances, tool subset selection, token budget child accounting, delegation depth enforcement, workspace isolation (git worktree for `branch` mode via gix), approval routing between parent and sub-agent event channels. The parent harness owns the delegation decision; each sub-agent owns its own loop execution.
- **TUI owns:** Rendering and **input loop for interactive mode**. In interactive mode, the TUI owns the input composer; user input triggers a single ReAct turn via the harness, then control returns to the TUI. The TUI does not run an autonomous loop — it drives the harness turn-by-turn. In headless/RPC mode, the harness runs the full ReAct loop autonomously. FleetView panel renders sub-agent status and provides steer/stop controls.
- **Headless/RPC owns:** Transport serialization (JSON over stdout/stdio/HTTP). Same event structs as the TUI renderer.

## Public vs private state

**Public (cross-subsystem, serialized):**
- `Message` (role, content, tool_calls, tool_call_id, ts)
- `ToolCall` / `ToolResult` (id, name, args, output, ok, duration_ms, truncated)
- `ApprovalRequest` / `Approval` (id, tool, args, decision)
- `Todo` (id, title, done)
- `Status` (model, tokens_in/out, cost, elapsed, retries, mode)
- `Config` (model, provider, context_cap, max_iterations, max_elapsed_sec, allow_list)
- `ModelCapabilities` (tool_calling, structured_output, thinking, max_context, max_output, temperature_supported, tier)
- `Tier` (small / frontier) — selects prompt template, tool subset, generation params
- `EditFormatHint` (diff / whole_file / str_replace) — per-model edit format selection
- `CostBudget` (session_cap_usd, per_turn_warning_usd, spent_usd) — opt-in cost enforcement
- `ToolResult` (id, name, args, output, ok, duration_ms, truncated, exit_code, timed_out, cancelled)
- Snapshot refs and log positions
- Session header (`id`, `cwd`, `version`, `started_at`)
- `ThinkingEvent` (optional, separate from visible message stream)

**Private (encapsulated within subsystem):**
- Harness: retry counters, in-flight ReAct iteration, loop-control flags, iteration counter, elapsed timer
- Config: resolved config layers, secret substitution cache
- Model: HTTP client, auth token, SSE buffer, provider config, capabilities, thinking buffer
- Permission: allow-list entries, session allow-always grants
- Context: rolling token estimate, cap threshold, model metadata table, tier, selected files, history turn cap, warning threshold
- Text tool parser: lenient parse state, repair rules
- Tool execution: streaming buffer, process handle, timeout state, cancellation token
- Cost budget: session spent, cap threshold, warning threshold
- TUI: scroll offsets, cursor blink state, focus ring, animation frame
- Session log: file handle, buffer flush state, session id
- Snapshot store: git repo handle, undo/redo cursor index
- Shutdown: signal registration, drain state, flush timeout

## Data flow

### Interactive mode (TUI-driven)

```
User types in composer
        │
        ▼
TUI (input loop) ──► Harness.run_turn(prompt) ──► Single ReAct iteration
        │                    │
        │                    ├─► Context (tiered prompt + skills/ + history + selective files, cap check, warnings)
        │                    ├─► Model (chat completion / stream; capabilities; thinking channel)
        │                    ├─► Text tool parsing (for models without native FC)
        │                    │         └─► Validate parsed calls against schema before execution
        │                    ├─► Edit format (diff/patch or whole-file per tier; preflight validate)
        │                    ├─► Permission (allow-list / approve)
        │                    │         ├─► Tool (execute, JSON schema or text-parsed)
        │                    │         │         └─► Tool execution (streaming, timeout, cancel)
        │                    │         └─► run: local process at boundary
        │                    ├─► Cost budget (check before tool call; refuse if over cap)
        │                    ├─► Session log (append JSONL)
        │                    ├─► Snapshot store (git, before destructive)
        │                    └─► Event stream (emitted during turn)
        │
        ▼
TUI renders events ──► Wait for next user input
```

### Headless/RPC mode (harness-driven)

```
User prompt / RPC request
        │
        ▼
CLI entry ──► Config load (global + project) ──► Mode (headless | rpc)
        │
        ▼
Harness (ReAct loop) ──► Autonomous: think → tool → observe → repeat
        ├─► Context (tiered prompt + skills/ + history + selective files, cap check, warnings)
        ├─► Model (chat completion / stream; capabilities; thinking channel) ◄──┐
        │                                                                       │
        ├─► Text tool parsing (for models without native FC)                    │
        │         └─► Validate parsed calls against schema before execution     │
        │                                                                       │
        ├─► Edit format (diff/patch or whole-file per tier; preflight validate) │
        │                                                                       │
        ├─► Permission (allow-list / approve)       │
        │         ├─► Tool (execute, JSON schema or text-parsed)   │
        │         │         └─► Tool execution (streaming, timeout, cancel)   │
        │         └─► run: local process at boundary │
        │                                           │
        ├─► Cost budget (check before tool call; refuse if over cap)            │
        ├─► Session log (append JSONL)              │
        ├─► Snapshot store (git, before destructive)│
        └─► Event stream (headless JSON / RPC)      │
                                                        │
        ▼                                               │
Headless/RPC (JSON over stdout/stdio/HTTP)            │
```

### Key differences

| Aspect | Interactive (TUI) | Headless / RPC |
|--------|-------------------|----------------|
| Loop driver | TUI (turn-by-turn) | Harness (autonomous) |
| Input | Composer (user types) | CLI arg / RPC request |
| Turn control | One `run_turn()` per user prompt | Continuous until done/error/cap |
| Approval | Modal blocks composer | JSON event on stdout / RPC call |
| Exit | Ctrl+D / `/quit` | Task complete / error / SIGINT |

- In both modes, the harness emits the **same event structs** (DESIGN.md §7).
- The TUI is a **renderer + input driver** in interactive mode; a **passive renderer** in headless mode (not used).
- Approval requests pause the harness turn; the loop resumes on an `Approval` event (from TUI modal or RPC).
- Context cap errors propagate as a terminal `error` event; the turn stops cleanly.

### Sub-agent delegation data flow

```
Parent ReAct Loop (Harness)
    │
    ├─► todo.delegate(todo_id, agent_profile, prompt)
    │       │
    │       ▼
    │   AgentRegistry.resolve(agent_profile)
    │       │
    │       ├── Validate: profile exists, depth < max_depth, spawns < budget
    │       ├── Create: isolated Conversation, tool subset, TokenBudget child
    │       ├── WorkspaceHandle: inherit | branch (gix worktree) | share
    │       └── Spawn: SubAgent { ReactLoop, parent_tx, approval_rx, depth+1 }
    │
    ├─► SubAgent runs autonomous ReAct loop (background task)
    │       ├─► Context (isolated, tiered prompt + selective files)
    │       ├─► Model (per-profile model override or inherit)
    │       ├─► Tool subset (profile allowlist/denylist)
    │       ├─► Permission (sub-agent's own policy; RequireApproval → parent)
    │       │       └─► ApprovalRequest emitted on PARENT's event_tx
    │       │       └─► Parent UI/RPC handles → decision → sub-agent approval_rx
    │       ├─► Cost budget (child tracker, enforced against parent cap)
    │       ├─► Session log (appends to parent's log with subagent_id tag)
    │       ├─► Snapshot store (parent's store; snapshots tagged with subagent_id)
    │       └─► Event stream: SubAgentProgress (periodic), SubAgentComplete (final)
    │
    └─► Parent receives SubAgentComplete
            ├─► Todo.complete(todo_id)
            ├─► ToolResult injected into parent conversation
            └─► Parent continues (async, non-blocking)

Key events (extend DESIGN.md §7):
- SubAgentProgress { subagent_id, profile, tokens_used, status, depth }
- SubAgentComplete { subagent_id, todo_id, result: ToolResult, summary }
- SubAgentError { subagent_id, error }
- ApprovalRequest (bubbled from sub-agent) → handled by parent → Approval routed back
```

## Service boundaries

- **Model service boundary:** `Model` trait is the only seam. Providers implement it; the harness depends on the trait, not on `reqwest` types directly (adapters convert). `Model` trait is text-in/text-out as primitive; native JSON tool calling is an optional capability, not a requirement.
- **Model capability boundary:** `ModelCapabilities` is declared per-model (TOML override for local models); harness never guesses capabilities for self-hosted models.
- **Text tool parsing boundary:** Raw model output is parsed into tool invocations leniently, then validated against the tool schema before execution. Invalid calls return structured errors; never execute unvalidated calls.
- **Tool service boundary:** `Tool` trait + `ToolRegistry`. Built-in tools live in `src/tool/builtin/`; user tools register at runtime. Tool subset selected by model tier.
- **Permission boundary:** `Policy::check` / `Policy::check_call(tool, spec, args)` gate every write/delete/network/process call. `check_call` is the argument-aware entry point used by the loop: for `run` it matches command rules (`run:<program>[ <subcommand>]`) from the allow-list and session grants. Confinement of file mutations is enforced inside the tools by `tool::workspace`.
- **Config boundary:** Config is loaded once at startup (global + project layers), validated, then treated as read-only. Secrets are substituted at load time and never logged.
- **Shutdown boundary:** Signal handler owns SIGINT/SIGTERM; it flushes the session log, drains the ReAct loop, teardowns the TUI, then exits. No subsystem may block shutdown indefinitely.
- **Transport boundary:** Headless/RPC transports (stdio JSON-RPC, optional HTTP) sit behind `HeadlessDriver`; the TUI has no transport dependency.
- **Persistence boundary:** `SessionLog` and `SnapshotStore` are the only subsystems that touch the filesystem/git outside the permission layer.

## Folder structure

```
okeanos/
├── .github/               # CI workflows (build, test, clippy, fmt)
├── assets/
│   └── themes/
│       ├── dark.toml      # design tokens, dark canvas
│       └── light.toml     # design tokens, light canvas
├── docs/
│   ├── DESIGN.md          # Phase 1 visual design (art direction, tokens, parity map)
│   ├── PRODUCT_SPEC.md    # Phase 3 product definition
│   ├── ARCHITECTURE.md    # Phase 4 architecture (this file)
│   ├── MILESTONES.md      # Phase 5 roadmap
│   └── AGENTS.md          # Phase 6 agent instructions
├── src/
│   ├── main.rs            # CLI entry, mode selection, signals, shutdown orchestration
│   ├── lib.rs             # harness crate root (public API)
│   ├── cli.rs             # clap derive definitions
│   ├── config/
│   │   ├── mod.rs         # Config, ConfigLayer, load/walk-up
│   │   ├── secret.rs      # {env:VAR} / {file:path} substitution
│   │   └── agent_profile.rs  # AgentProfile, AgentRegistry (M27)
│   ├── harness/
│   │   ├── mod.rs
│   │   ├── react.rs       # ReAct loop, LoopDecision, iteration/elapsed limits
│   │   ├── policy.rs      # approval policy wiring
│   │   ├── error.rs       # HarnessError (thiserror)
│   │   └── subagent.rs    # SubAgent, SubAgentHandle, spawning, bubbling (M27)
│   ├── model/
│   │   ├── mod.rs
│   │   ├── trait.rs       # Model trait, ModelCapabilities, Tier
│   │   ├── openai.rs      # OpenAI-compatible/OpenRouter provider
│   │   ├── openai_native.rs     # OpenAI Chat Completions (native)
│   │   ├── openai_responses.rs  # OpenAI Responses API (stateless, encrypted reasoning replay)
│   │   ├── anthropic.rs / google.rs  # Messages / Generative AI adapters
│   │   ├── registry.rs    # ProviderKind/WireApi; the only place config becomes a model
│   │   ├── catalog.rs     # models.dev-derived catalog: bundled assets/models.json + 24h user cache (context, output, vision, reasoning, price; tier derived from capabilities)
│   │   ├── metadata.rs    # per-model context window + cost table + tier defaults
│   │   ├── text_tool.rs   # lenient text-based tool call parsing for non-native FC models
│   │   └── mock.rs        # MockModel (test-only, cfg(test) or test feature)
│   ├── tool/
│   │   ├── mod.rs
│   │   ├── trait.rs       # Tool trait, ToolSpec
│   │   ├── registry.rs    # ToolRegistry (tool subset by tier)
│   │   ├── edit/          # diff/patch + whole-file fallback, preflight validation
│   │   ├── execute/       # interruptible process execution, streaming, timeout
│   │   └── builtin/
│   │       ├── todo.rs    # extended with Delegate action (M27)
│   │       ├── read.rs
│   │       ├── write.rs
│   │       ├── delete.rs
│   │       ├── run.rs     # local process at permission boundary (M29 hardened)
│   │       ├── remember.rs # durable memory writer, Write category (M28)
│   │       └── grep.rs
│   │   └── workspace.rs   # writable-root confinement for write/edit/delete (M29)
│   ├── cost/
│   │   └── budget.rs      # session cost cap, per-turn warnings, fail-closed pricing
│   ├── perm/
│   │   ├── mod.rs
│   │   ├── policy.rs      # Policy, allow-list
│   │   └── approval.rs    # ApprovalDecision, session grants
│   ├── context/
│   │   ├── mod.rs
│   │   ├── conversation.rs
│   │   ├── cap.rs         # hard context cap, ContextCap error
│   │   ├── tokens.rs      # approximate token counter, CostEstimate
│   │   ├── tier.rs        # model tier → prompt template + tool subset + generation params
│   │   ├── selective.rs   # selective file inclusion (ripgrep symbol scan, pure Rust)
│   │   ├── memory.rs      # instruction-file hierarchy + MEMORY.md index loading (M28)
│   │   └── warnings.rs    # context window warnings before hard cap
│   ├── session/
│   │   ├── log.rs         # append-only JSONL (TODO: not yet split)
│   │   ├── header.rs      # session id, cwd, version (TODO: not yet split)
│   │   └── resume.rs      # --resume <id>, project-scoped storage (TODO: not yet split)
│   ├── snapshot/
│   │   ├── mod.rs
│   │   ├── store.rs       # gix-based snapshot store
│   │   └── undo_redo.rs
│   ├── ui/
│   │   ├── mod.rs
│   │   ├── theme.rs       # design tokens from assets/themes/
│   │   ├── layout.rs      # responsive grid per DESIGN.md §4
│   │   └── components/    # conversation, tool_panel, modal, status_bar, fleet_view (M27), …
│   ├── headless.rs        # JSON event emitter, stdio/JSON-RPC transports (TODO: not yet created)
│   └── events.rs          # shared event structs (Message, ToolCall, Approval, SubAgentProgress, …)
├── tests/                 # integration tests (mock provider, harness logic)
├── Cargo.toml
├── rust-toolchain.toml
├── .gitignore
└── LICENSE
```

### Current Implementation Gaps (as of M11 complete)

| Spec (this doc) | Reality | Tracking |
|-----------------|---------|----------|
| `src/headless.rs` exists | Missing; headless logic inline in `main.rs::run_headless()` | M14 |
| `src/session/log.rs`, `header.rs`, `resume.rs` | Single `src/session/mod.rs` | M13 |
| `toml` crate in harness deps | In workspace `[dependencies]` (violates CLI-only rule) | M16 |
| `--rpc` CLI flag | Not defined in `cli.rs` | M14 |
| `jsonrpsee` in `rpc` feature | Not in `Cargo.toml` | M14 |
| `ReactLoop::run_turn()` | Not implemented; only `run()` exists | M12 |
| TUI input composer | Not implemented | M12 |
| Welcome screen | Not implemented | M12 |
| Graceful shutdown flush on SIGINT | Only normal exit cleanup | M17 |
| `src/config/agent_profile.rs` | Not created | M27 |
| `src/harness/subagent.rs` | Not created | M27 |
| `TodoArgs::Delegate` variant | Not in `todo.rs` | M27 |
| `SubAgentProgress/Complete` events | Not in `events.rs` | M27 |
| FleetView TUI component | Not created | M27 |
| `branch` workspace mode (gix worktree) | Not implemented | M27 |

These gaps are tracked in MILESTONES.md Technical Debt section and will be resolved in the indicated milestones.

## Security model

1. **Safe by default:** No write, delete, or network operation executes without policy approval. Read-only operations (read, grep, todo list) run without confirmation.
2. **Allow-list:** Commands (for `run`, as `run:<program>[ <subcommand>]` rules), hosts (for model provider), and tool names are checked against an allow-list; defaults deny. File mutations are confined to the workspace roots (see 12).
3. **Confirm-before:** write/delete/network require explicit allow (TUI modal or RPC approval). `allow-always` grants are session-scoped and recorded in the session log.
4. **Permission boundary for processes:** The `run` tool executes a program directly (no shell) inside the permission layer; its category is `Process` (M33, gated like `Network`). `allow-always` on `run` is scoped to the command and refused for risky commands (shells, interpreters, sudo, rm, curl/ssh, `git push`…). The child gets a closed stdin and a scrubbed environment (secret-looking variables removed), or — with `permissions.env_allowlist` — only a baseline plus the listed names. When the model runs `sh|bash|zsh|dash|ash|ksh -c "<script>"`, the script is split into simple commands (`&&`, `||`, `;`, `|`, `&`, newline; quote-aware): `deny`/`ask` rules match any segment, `allow` rules/grants require every segment to be allowed, and scripts that cannot be analysed (command substitution, subshells, redirections, variable command words, keywords) are never auto-allowed. Rules also come from `exec-policy.toml` files (`src/harness/exec_policy.rs`): user `$XDG_CONFIG_HOME/okeanos/exec-policy.toml` and project `.okeanos/exec-policy.toml` (part of the trust fingerprint; untrusted → `deny`/`ask` only). By default there is **no OS-level sandbox** — an approved command runs with the user's full privileges; enable one with `permissions.sandbox` (item 14).
5. **No secrets in logs:** `tracing` events must not log API keys, auth headers, or tool args containing secrets; args are redacted by default in log output.
6. **Git sandbox:** Snapshots are taken only inside the session's working directory; gix never runs arbitrary git commands outside policy.
7. **Static binary reduces supply-chain surface:** No libgit2/openssl FFI (gix + rustls), minimizing native dependencies.
8. **Sub-agent isolation:** Sub-agents inherit parent's permission mode but receive a tool subset per profile (allowlist/denylist). They cannot escalate privileges beyond their profile. `branch` workspace mode uses gix worktrees for filesystem isolation; `share` mode uses a shared directory with path-scoped access.
9. **Approval bubbling:** Sub-agent approval requests route to parent; parent's decision is the only authority. Sub-agent cannot bypass approval by spawning children (depth limit enforced at spawn).
10. **Token budget accounting:** A sub-agent runs with its own `CostTracker` (no USD cap) bounded by the profile's turn/token limits; its real spend is added to the parent's tracker when it finishes, so the parent's session cap applies. Global spawn budget (default 64) and a concurrency cap (4) prevent resource exhaustion.
11. **Plan mode:** In plan mode (read-only) the model calls `exit_plan(plan)`; the plan is saved to `.okeanos/plans/` and shown for approval, and on approval the mode switches to accept-edits.
11. **Instruction files and memory are untrusted-ish input:** `AGENTS.md`/`CLAUDE.md` and memory files from a cloned repo enter the system prompt, like in other agents; they cannot widen permissions because every tool call still passes `Policy::check`. They are plain files, not a new config source: they carry prose for the model and never change okeanos settings. `remember` is Write-category (approval required), slugs file names, and refuses to write outside the memory directories. Memory is never loaded from outside `<project_root>/.okeanos/memory/` or the user config dir.
12. **Workspace confinement (M29):** `write`, `edit` and `delete` reject paths outside the project root and `permissions.extra_write_roots` (symlinks/`..` resolved first), and always protect `.git/**`, `.env`/`.env.*` and the roots themselves. Reads are unrestricted. This guards against model mistakes and path tricks; it is not a sandbox against an approved `run` command. The argument-aware `guard_run` check is best-effort only (shell redirects, interpreters and VCS commands can still write anywhere); only the OS sandbox (item 14) actually confines `run`, so enable it wherever a backend exists. Windows has no confinement backend yet (Job Objects only kill the process tree).
13. **Process containment (M29):** timeouts and cancellation kill the whole process tree (Unix process group; Windows Job Object via `windows-sys`, a pure-Rust bindings crate). Default timeout 120 s. Output capture is memory-bounded.
14. **OS sandbox (M30, opt-in):** `sandbox = "read-only"` makes the whole filesystem read-only for `run` children (private `/tmp`); `"workspace-write"` additionally allows writes under the workspace roots (project root + `extra_write_roots`), with `.git` and `.env*` kept read-only. Network is off unless `sandbox_network = true`. Approvals still apply first; the sandbox is a second, OS-enforced layer. If the mode is not `none` and the platform helper is missing or unsupported (Windows, no `bwrap`), `run` **refuses to execute** rather than silently running unsandboxed. Landlock/Windows AppContainer are future work.
### Durable memory data flow (M28)

1. **Startup:** `ProjectContext::load(project_root, cwd, tier, &MemoryConfig)` reads the instruction files (user dir, then root→CWD) and, for mid/frontier tiers, the two `MEMORY.md` indexes. Each source is size-capped; truncation appends a notice naming the file.
2. **Prompt assembly:** `build_system_prompt_with_context` appends, after tier prompt and skills: `## Project instructions` (one labeled block per file), then `## Memory` (index lines + how to `read` topics / `remember`). The result is fixed for the session, so the cached prefix never changes mid-session.
3. **Compaction:** instructions/memory live in the system prompt, outside the compactable history, so they are re-sent verbatim after every compaction and never summarised away.
4. **Write:** `remember` → `Policy::check` (approval) → write `<scope dir>/<slug>.md` + upsert the index line. Takes effect next session.
5. **Out of scope (deferred):** sub-agent inheritance of instructions/memory, background memory extraction/consolidation, session titles/search, `--continue`, dedicated todo-state log events.
## Observability, tracing and evals

- **Logs** go to stderr (never stdout); in TUI mode only to `OKEANOS_LOG_FILE`.
- **Span trace:** `--trace[=PATH]` or `OKEANOS_TRACE` writes JSONL spans (`run`, `iteration`, `model_call`, `tool_call`, `compaction`) to `.okeanos/traces/`.
- **Session log** records `RunMeta` (version, model, prompt/tool hashes) and a per-attempt `ModelCall` (TTFT, latency, usage, cost, errors). `Token`/`Thinking`/`ToolOutput` deltas are not persisted. Lines are scrubbed of secrets; set `OKEANOS_LOG_REDACT=0` to disable.
- **OTLP export (optional):** build with `--features otel` and set `OTEL_EXPORTER_OTLP_ENDPOINT`; the same spans are sent over OTLP/HTTP.
- **Commands:** `okeanos stats|export|replay <session>` and `okeanos eval <suite.json> [--baseline F] [--out F]`. Eval runs tasks on fixture copies, optionally replaying a recorded session via `OKEANOS_REPLAY_SESSION`.

## Dependency rules

- **Primary crates (Phase 2 recommendation):** ratatui + crossterm (TUI), tokio `current_thread` (runtime), reqwest with rustls (HTTP), clap derive (CLI), thiserror (lib errors) + anyhow (bin), serde + serde_json (JSON), toml (config), tracing + tracing-subscriber `json` (logging), gix (git), jsonrpsee feature-gated (RPC).
- **No C dependencies** in the primary tree: avoid git2, openssl, rapidjson. Build with `--target x86_64-unknown-linux-musl` for full static.
- **Platform process-control crates (M29):** `nix` (`signal`, Unix-only target dependency) for process-group signalling and `windows-sys` (Windows-only target dependency, features `Win32_Foundation`/`Security`/`System_IO`/`System_JobObjects`/`System_Threading`) for Job Objects. Both are raw-binding crates with no C code; neither is compiled on the other platform. tokio additionally enables `io-util` and `sync`.
- **Feature flags:** `rpc` (jsonrpsee HTTP/WebSocket), `mcp` (lazy MCP client, deferred), `mock-provider` (test-only MockModel), `simd-json` (performance opt, deferred).
- **Config crate rule:** `toml` is added only for config parsing; it sits in the CLI/config layer, never in the harness `lib`.
- **Layering rule:** `src/lib` (harness) depends only on model/tool/perm/context/session/snapshot/events — never on `ui`, `headless`, or `cli`. UI and headless depend on the harness, not vice versa. Sub-agent delegation (`src/harness/subagent.rs`) is part of the harness lib; it uses `ReactLoop`, `ToolRegistry`, `Policy`, `CostTracker`, `Conversation`, `SnapshotStore` — all harness-internal.
- **No circular dependencies** between subsystems.

## Testing strategy

| Layer | Tooling | Scope |
|-------|---------|-------|
| Unit | `cargo test`, mock provider | ReAct loop, approval policy, context cap, tool execution, snapshot undo/redo, config load + secret substitution, token counter, model capability negotiation, text tool parsing, tier selection, edit format validation, tool execution interrupt/cancel, **agent profile parsing, sub-agent spawn validation, tool subset selection, token budget child accounting, depth enforcement, approval bubbling routing** — no network |
| Integration | `tests/` | CLI modes, headless JSON event parity with TUI events, session log append, snapshot round-trip, `--resume` restore, graceful shutdown flush, small-model text-based tool calling, interruptible tool execution, cost cap enforcement, **sub-agent delegation end-to-end: spawn, progress events, approval bubbling, completion, result aggregation, workspace modes (inherit/branch)** |
| TUI snapshot | golden buffer files | Rendered frame at 80×24 and 120×40; resize reflow; modal re-center; **FleetView panel with active sub-agents** |
| Visual audit | automated contrast check | WCAG AA/AAA ratios per DESIGN.md §2 on both canvases |
| Reduced motion | config toggle | No time-dependent frame changes when disabled |
| Event parity | harness test harness | Every TUI state transition has a matching JSON event (DESIGN.md §7), **including SubAgentProgress, SubAgentComplete, bubbled ApprovalRequest** |
| Clippy/fmt | CI | `cargo clippy -- -D warnings`, `cargo fmt --check` |

## Scalability considerations

- **Context:** Hard cap at MVP (no compaction). Selective file inclusion and history-turn capping are tier-aware (small models get fewer files and turns). If compaction is added later, it lives in `src/context/` behind the `ContextCap` seam; the ReAct loop treats it as a context-source change, not a rewrite.
- **Concurrency:** Single-threaded tokio `current_thread` for the ReAct loop. Parallel tool execution can be added later behind the `ToolRegistry` without changing the loop signature. **Sub-agents run as independent background tasks; each has its own `ReactLoop` on the same runtime. Concurrency bounded by global spawn budget (default 64) and per-session token budget.**
- **Providers:** New providers implement `Model` + declare `ModelCapabilities`; no changes to harness logic. Provider selection is config-driven (feature flags at build time, env/CLI at runtime). Local/self-hosted models require explicit TOML capability override. **Per-agent model override enables cost-optimized routing (Haiku for exploration, Opus for reasoning).**
- **Tools:** New tools implement `Tool` and register in the registry. Tool subset is selected by model tier; small models get a minimal subset. No harness changes. **Sub-agent tool subset is a further filter of the tier subset via profile allowlist/denylist.**
- **Transport:** stdio JSON-RPC first; HTTP/WebSocket via `jsonrpsee` behind the `rpc` feature. Adding a transport means implementing `HeadlessDriver`, not touching the loop.
- **Session log:** Append-only JSONL is unbounded by design at MVP; rotation/archiving can be added in `src/session/` without affecting the loop. **Sub-agent events are tagged with `subagent_id` in the parent's log for traceability.**
- **Workspace isolation:** `branch` mode creates git worktrees via gix; cleanup on sub-agent completion. Number of concurrent worktrees bounded by spawn budget. No persistent disk growth.

## Tooling, skills and extensibility (update)

- **Tool order** is deterministic (BTreeMap) for prompt-cache stability; per-tier sets come from 	ool::builtin_registry.
- **read** is line-numbered with offset/limit; **grep** uses the ignore crate (gitignore, glob filter, context); **glob** finds files; **edit** is available on every tier.
- **MCP**: parallel connect, per-call 	imeout_secs, one reconnect, stderr drained, spec cache (DeferCached), tool deferral behind 	ool_search when more than defer_tools_over tools exist, project .mcp.json. Unannotated MCP tools require approval (Network). Streamable HTTP is not enabled in this build (needs a C toolchain for ws-lc-rs).
- **Multimodal MCP results (M34)**: `render_result` keeps text + placeholders and returns accepted image blocks in `ToolExecutionResult.images`; the ReAct loop attaches them to the tool message (`MessageExtras.images`) only if `ModelCapabilities.vision` is true (catalog `vision`, override `[model.capabilities] vision`; default false). Adapters: Anthropic puts image blocks inside `tool_result`; OpenAI Responses uses `input_image` in `function_call_output`; OpenAI Chat and Google append images as a user message/part after the tool results. Pruned tool results drop their images.
- **Built-in MCP servers**: `context7` (library docs, `https://mcp.context7.com/mcp`) and `gh_grep` (GitHub code search, `https://mcp.grep.app`) are connected by default as read-only Streamable HTTP servers (no login, no approval prompts, 8s connect budget, `DeferCached` startup unless `[mcp] startup_policy` is set; tools are deferred behind `tool_search`). Failures are non-fatal (offline works). Query text is sent to those third-party services: disable all with `[mcp] builtin_servers = false` or one via `[[mcp.servers]] name = "gh_grep" enabled = false`. `read_only = true` on any server downgrades its tools to the Read category.
- **Skills**: <root>/<name>/SKILL.md with frontmatter; only a name/description index is in the system prompt and the skill tool loads bodies. Roots: configured dir, .okeanos/, .claude/, .agents/ skills (project, then home).
- **Hooks**: [[hooks]] events `PreToolUse`, `PostToolUse`, `UserPromptSubmit`, `Stop`, `SessionStart`, `SubagentStop` (regex matcher, command, `fail_closed`). Matching hooks run concurrently. Exit 2 blocks (stderr is the reason); stdout may be JSON `{"decision":"allow|ask|deny","reason","updated_input","additional_context"}` (block > ask > allow; `updated_input` rewrites tool args before the policy check) or plain text added as context. `Stop` blocking makes the model continue (max 3 rounds). Tool hooks run for parallel read batches and sub-agents too.
- **Permissions**: `harness::gate::evaluate` is the single path (PreToolUse hooks, then `Policy`) used by serial, parallel and sub-agent calls. `[permissions]` `deny`/`ask`/`allow` rules (`tool[:pattern]`, deny > ask > allow); `run` commands are normalized past global flags (`git -C x push` is `git push`); unparseable ones can't be session-granted. Modes: default, accept-edits, plan, yolo (`--permission-mode`, Shift+Tab). Sub-agents inherit deny/ask rules and the mode. `read`/`grep` skip credential files unless `protect_secrets = false`; `run` path-checks common mutating programs against the write roots. Windows has no OS sandbox backend (`okeanos doctor` reports it).
- **Project trust**: `okeanos.toml`, `.mcp.json` and `.okeanos/agents/*.toml` are trusted per directory by content fingerprint (`okeanos trust [--revoke]`, `OKEANOS_TRUST_PROJECT=1`, prompt on first interactive run). Untrusted projects only contribute a restrictive overlay (context/cost/ui/snapshots/compaction/tool_cache, model basics, `deny`/`ask`, plan mode, a sandbox); hooks, MCP, providers, agents and allow rules are ignored.
- **ACP**: okeanos::acp provides the JSON-RPC/event translation layer; run it with `okeanos --acp` (stdio, newline-delimited JSON-RPC; supports session/new, session/prompt, session/cancel and session/request_permission).
