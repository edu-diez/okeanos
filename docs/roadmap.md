# Roadmap

Where okeanos is, how work is organized, and what is next. For the full release history see [CHANGELOG.md](../CHANGELOG.md); for requirements see [design/product-spec.md](design/product-spec.md).

**Current version:** `0.27.0` (pre-1.0: any minor release may contain breaking changes).

## How we work

- Work is split into small milestones (`M<n>`), each delivering one testable thing in 1–3 sessions.
- A milestone is done when the spec docs are updated, tests pass, and CI is green (`cargo fmt --check`, `cargo clippy -- -D warnings`, `cargo test`).
- User-visible milestones bump the minor version and are tagged `v0.x.0`; refactors and fixes bump the patch version.
- Detailed scope, acceptance criteria and definitions of done for each milestone are in the git history of this file (before the rewrite) and in the commit that implemented it.

## Shipped

### MVP (v0.1.0)

| Milestone | Deliverable |
|---|---|
| M0–M0.5 | Scaffolding, CLI, completions, `init`, `config validate`, `doctor`, `sessions` |
| M1 | Global and project config with `{env:VAR}` / `{file:path}` secrets |
| M2–M3 | Core ReAct loop, `Model` trait, hard context cap, model capabilities and tiering |
| M4–M6 | Tool system, text-based tool calling, approval policy, built-in tools, interruptible execution |
| M7–M8 | Rate-limit handling and fallbacks, JSONL session log and `--resume` |
| M9–M11 | OpenAI-compatible provider, cost budget, edit formats |
| M12–M12.5 | TUI shell, config and input UX, generation interruption |
| M13 | Git snapshots with `/undo` and `/redo` |
| M14–M15 | Headless and RPC mode, Apple Silicon builds |
| M16–M17 | Thinking channel, tiered tool sets, selective context, config layer cleanup |

### Post-MVP

| Version | Milestone | Deliverable |
|---|---|---|
| 0.2.0 | M18 | Context compaction (basic and LLM-based, `/compact`) |
| 0.3.0 | M19 | Anthropic, OpenAI and Google providers |
| 0.4.0 | M20 | WebSocket RPC transport |
| 0.5.0 | M21 | MCP client |
| 0.6.0 | M22 | Session log rotation and compression |
| 0.7.0 | M23 | Tool result caching (TTL, read-only tools) |
| 0.8.0 | M24 | Confirmation for expensive calls |
| 0.9.0 | M26 | Markdown rendering in the TUI (`markdown` feature) |
| 0.11.0 | M27 | Multi-agent delegation, agent profiles, approval bubbling |
| 0.12.0 | M28 | Durable cross-session memory and project instruction files |
| 0.13.0 | M29 | Execution hardening: `run` timeouts, env scrubbing, workspace confinement |
| 0.14.0 | M30 | Optional OS sandbox (`bwrap` on Linux, `sandbox-exec` on macOS) |
| 0.15.0 | M31 | Incremental `tool_output` events |
| 0.16.0 | M32a | Skills, hooks, ACP, MCP overhaul, `glob`, built-in MCP servers |
| 0.17.0–0.21.0 | — | Permissions and guardrails, sub-agent and planning overhaul, model catalog, observability and evals, IO reliability |
| 0.22.0 | M32 | Background processes (`job_output`, `job_kill`) |
| 0.23.0 | M33 | Policy depth: exec-policy files, compound commands, env allowlist |
| 0.24.0 | M34 | Multimodal MCP results |
| 0.25.0 | M35 | MCP OAuth for Streamable HTTP servers |
| 0.25.1 | M36 | Split `main.rs` and `react.rs` into modules (no behaviour change) |
| 0.26.0 | M38 | Weak-model support: tier cleanup, forgiving edits, verification, stall detection |
| 0.27.0 | M39 | Weak-model support, part 2: verified retry, candidates, per-model overrides, eval baselines, behaviour-based tiers |

## Not planned or deferred

| Item | Status | Reason |
|---|---|---|
| M25: tree-sitter symbol scanning | Not planned | Needs C dependencies, which conflicts with the pure-Rust, static-binary rule. Selective context uses ripgrep-style scanning instead. |
| M37: Windows sandbox | Deferred | A native sandbox risks false confidence; WSL2 or containers already provide the Linux sandbox. Until then, Windows `run` is guarded by approvals and path checks only. If revived, start with a time-boxed spike that must pass real-process tests (writes outside the workspace fail, `.git` and `.env*` stay read-only, network blocked by default, the process tree dies with its job), and abandon it otherwise. |

## Ideas

Not committed to; none of these have a milestone yet.

- A `/tier` command to pin a model tier at runtime (today: `model.tier` or `[models."<id>"].tier` in config).
- Persistent shell sessions for `run`.
- Job persistence across sessions, stdin to background jobs, and a TUI job panel.
- Landlock and a network allowlist or proxy for the sandbox.

## Versioning

okeanos follows Cargo's `0.y.z` convention until the CLI, config schema and public API are stable enough to promise semver at `1.0.0`.

| Change | Bump |
|---|---|
| New feature or user-visible behaviour change | Minor (`0.x.0`) |
| Bug fix or no-behaviour refactor | Patch (`0.x.y`) |
| Stability commitment | `1.0.0` |

If several milestones land in one release cycle they can share one minor bump.

## Release process

1. All tests pass and CI is green.
2. Bump the version in `Cargo.toml`.
3. Add an entry to [CHANGELOG.md](../CHANGELOG.md).
4. Tag the commit `vX.Y.Z`.
