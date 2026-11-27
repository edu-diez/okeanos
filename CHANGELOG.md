# Changelog

All notable changes to okeanos are documented here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/). While the version is `0.x`, any minor release may contain breaking changes.

Milestone-level detail (scope, acceptance criteria) lives in [docs/roadmap.md](docs/roadmap.md).

## [0.27.1]
### Changed
- Relicensed under AGPL-3.0-or-later (previously MIT).
- Release binaries now include the `rpc`, `mcp`, `otel` and `markdown` features, and are published as `.tar.gz` (Linux, macOS) or `.zip` (Windows) archives with a `SHA256SUMS.txt`.
- Documentation reorganized: new README, CONTRIBUTING and CHANGELOG, docs moved under `docs/design/`, roadmap and `AGENTS.md` rewritten.

## [0.27.0]
### Added
- Weak-model support, part 2 (M39): `[verify] on_fail/candidates`, per-model overrides under `[models."<id>"]`, `okeanos eval --suite weak-model`, behaviour-based tier adjustment.

## [0.26.0]
### Added
- Weak-model support (M38): the small tier gains `grep`/`glob`, forgiving edits, stall detection, goal pinning, optional `[verify]` config.
### Changed
- System prompts reworked per tier.

## [0.25.1]
### Changed
- Refactor only: `main.rs` split into `commands/`, `modes/` and `wiring`; `harness/react.rs` split into modules (M36).

## [0.25.0]
### Added
- OAuth for Streamable HTTP MCP servers; `okeanos mcp login` / `logout` (M35).

## [0.24.0]
### Added
- Multimodal MCP results; new `vision` model capability (M34).

## [0.23.0]
### Added
- Policy depth: exec-policy files, compound command parsing, `permissions.env_allowlist` (M33).
### Changed
- `run` / `job_kill` tool category changed from `network` to `process`.

## [0.22.0]
### Added
- Background processes (M32).

## [0.16.0] – [0.21.0]
### Added
- 0.16.0: skills, `[[hooks]]`, `okeanos --acp`, MCP overhaul (parallel connect, `tool_search`, `.mcp.json`, Streamable HTTP), `glob` tool, built-in MCP servers (M32a).
- 0.17.0: permissions, hooks and guardrails overhaul.
- 0.18.0: sub-agent spawning and planning overhaul.
- 0.19.0: model and surface portability, models.dev catalog.
- 0.20.0: observability, tracing and evals.
- 0.21.0: IO safety and reliability hardening.

## [0.15.0]
### Added
- Incremental `tool_output` event. Consumers matching `Event` exhaustively must handle it (M31).

## [0.14.0]
### Added
- Optional OS sandbox via `permissions.sandbox` (default `none`) (M30).

## [0.13.0]
### Added
- `permissions.extra_write_roots`.
### Changed
- Execution hardening (M29): `run` defaults to a 120 s timeout, scrubs secret env vars and closes stdin; `write`/`edit`/`delete` refuse paths outside the project root, `.git` and `.env*`; `allow-always` on `run` is command-scoped.

## [0.12.0]
### Added
- Durable cross-session memory: `[memory]` config and the `remember` tool; project instructions in the system prompt (M28).

## [0.11.0]
### Added
- Multi-agent delegation: `agent` tool, agent profiles, approval bubbling (M27).

## [0.9.0]
### Added
- Markdown rendering in the TUI behind the `markdown` feature (M26).

## [0.8.0]
### Added
- Confirmation for expensive calls (M24).

## [0.7.0]
### Added
- TTL cache for read-only tool results (M23).

## [0.6.0]
### Added
- Session log rotation / compression (M22).

## [0.5.0]
### Added
- MCP client (M21).

## [0.4.0]
### Added
- WebSocket RPC transport (M20).

## [0.3.0]
### Added
- Anthropic, OpenAI and Google providers (M19).

## [0.2.0]
### Added
- Context compaction (M18).

## [0.1.0]
### Added
- MVP (M0–M17): ReAct loop, tool system, approval policy, JSONL sessions with resume, git snapshots with `/undo` and `/redo`, cost budget, TUI, headless/RPC mode.
