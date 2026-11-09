# Phase 1 — Visual Design & Art Direction

**Project:** Rust CLI TUI Agent Harness
**Status:** Phase 1 — design only (no implementation code)
**Owner:** UI/UX (Design)
**Validation:** Workflow manager checks this against product requirements
**Output path:** DESIGN.md

## Summary

The harness is a single static Rust binary that turns an LLM into an autonomous coding agent (ReAct loop, tool calls, approval policies, todo tool, headless/RPC mode, JSONL session log, git snapshots). The primary interface is a terminal TUI; headless/RPC mode emits the same state as JSON. This document defines the visual design so Phase 2+ can build the TUI without visual rework.

## 1. Art direction principles

Five axioms:

1. **Information density over chrome.** Every character cell carries status or content. Decorative borders are used only where they structurally separate regions; no filler patterns, no gradients for decoration alone.
2. **Status is always visible.** Model, token count, cost, elapsed time, and retry state never hide behind modals or scroll. They live in a persistent status bar.
3. **Approval interrupts deliberately.** Tool approval demands full attention: centered modal, high contrast, explicit allow/deny/allow-always choices, keyboard-first. No subtle cues.
4. **One source of truth.** The TUI and headless/RPC JSON are views of the same state machine. Every visual change maps 1:1 to a JSON event; headless is not a second-class UI.
5. **Respect the terminal.** Professional at 80×24, comfortable at 120×40, graceful under resize. Works on default terminal colors with optional theme override.

## 2. Color palette

### Dark mode (primary)

Canvas: `#0b0e11` (near-black, cool) · foreground: `#e2e8f0` · muted: `#94a3b8` · border: `#1e293b`

| Role | Hex | Use | Contrast vs `#0b0e11` |
|------|-----|-----|----------------------|
| ok | `#4ade80` | successful tool result, completed todo | ~11.5:1 (AAA) |
| warn | `#fbbf24` | retrying, rate-limit warning, stale snapshot | ~11.6:1 (AAA) |
| error | `#f87171` | failed tool call, auth error, parse error | ~7.1:1 (AAA) |
| pending | `#94a3b8` | in-progress stream, queued call | ~7.7:1 (AAA) |
| tool | `#818cf8` | tool-call name, args chip | ~6.6:1 (AAA) |
| system | `#cbd5e1` | log lines, file paths, meta text | ~13.2:1 (AAA) |
| user | `#e2e8f0` | user message bubble | ~16:1 (AAA) |
| assistant | `#818cf8` | assistant message bubble | ~6.6:1 (AAA) |

### Light mode variant

Canvas: `#f8fafc` · foreground: `#0f172a` · muted: `#475569` · border: `#cbd5e1`

| Role | Hex | Contrast vs `#f8fafc` |
|------|-----|----------------------|
| ok | `#15803d` | ~4.7:1 (AA) |
| warn | `#b45309` | ~4.6:1 (AA) |
| error | `#b91c1c` | ~6.3:1 (AAA) |
| pending | `#475569` | ~6.8:1 (AAA) |
| tool | `#4338ca` | ~7.3:1 (AAA) |
| system | `#475569` | ~6.8:1 (AAA) |
| user | `#0f172a` | ~17.8:1 (AAA) |
| assistant | `#4338ca` | ~7.3:1 (AAA) |

### Contrast rationale

- Small body text meets WCAG AA (4.5:1) minimum on both canvases; headings and badges target AAA (7:1).
- Status is never color-only: ok/warn/error/pending are paired with distinct glyphs (✓/⚠/✗/⟳) and text labels, so colorblind users and monochrome terminals are not disadvantaged.
- Semantic hues shift lightness between modes (lighter accents on dark canvas, darker accents on light canvas) rather than inverting hue, keeping meaning constant.

## 3. Typography

Single monospace stack with graceful fallback:

`"JetBrains Mono", "Fira Code", "Cascadia Code", "Hack", "IBM Plex Mono", monospace`

Requirements: box-drawing (`─ │ ┌ ┐ └ ┘`), block elements, and braille spinner glyphs must render. Nerd Font icons are optional with a text fallback. Ligatures off by default.

| Level | Size | Weight | Color (dark) | Use |
|-------|------|--------|--------------|-----|
| Heading | 1.1× body | bold | `#e2e8f0` | pane titles, modal titles |
| Label | 0.9× body | bold | semantic role | chip labels ("tool", "ok") |
| Body | 1× | regular | `#e2e8f0` | message text, args, results |
| Mono-data | 0.9× | regular | `#cbd5e1` | JSON, file paths, diff hunks |
| Muted | 0.9× | regular | `#94a3b8` | timestamps, retry counts, log tail |

Terminal note: one character cell equals one grid unit; most terminals render cells at roughly 2:1 width-to-height, so vertical motion feels slower than horizontal. Design for cell grids, not pixels.

## 4. Layout grid

Grid unit: 1 terminal character cell. Regions stack vertically by default; columns appear when width allows.

### Breakpoints

| Size | Mode | Layout |
|------|------|--------|
| 80×24 | minimum | Single column: header (1 row), conversation (rest − 2), status bar (2 rows). Right-rail content folds into the conversation bottom as collapsible sections. |
| 120×40 | comfortable | 3 regions: conversation left (~60%), right rail (~40%: tool panel + todos + log tail), status bar bottom (2 rows). |
| < 100 cols | narrow | Right rail collapses; tool results and todos show as inline sections below the conversation. |
| < 40 rows | short | Status bar compresses to 1 row; todo panel hidden by default; conversation shrinks. |

### Region definitions

- **Header** (1 row): app name, mode indicator (interactive / headless / RPC), connection dot.
- **Conversation pane** (main): scrollback of user/assistant/tool messages; assistant streams with typewriter cursor.
- **Right rail** (comfortable only): tool-call invocation panel, todo panel, log tail panel (tabbed or stacked).
- **Status bar** (1–2 rows, bottom): model, tokens in/out, cost, elapsed, retry count, headless/RPC indicator. Always visible.
- **Modal layer** (overlay): approval/confirm, command palette. Centered, dims background.
- **Command palette** (overlay): center-bottom, searchable command list (`/undo`, `/redo`, `/help`, …).

Responsive behavior: on resize, regions reflow immediately; modal overlays re-center; conversation scroll position is preserved.

## 5. Component hierarchy & wireframes (descriptions only)

No code here — structural descriptions for implementation.

1. **Conversation pane** — vertical message list. User messages left-aligned with a `user` chip; assistant messages with an `assistant` chip and streaming token render; tool results indented under the assistant turn, collapsed by default with an expand affordance. Newest message at bottom; autoscroll follows the stream unless the user scrolls up.
2. **Streaming token render** — monospace text with a blinking block cursor at the insert point; typing indicator (three pulsing dots) while the model is thinking between tokens.
3. **Tool-call invocation panel** — name chip (`tool` color), arguments as scrollable mono-data with key/string coloring, result block (expandable), status badge (`pending` → `ok`/`error`), retry counter next to the badge.
4. **Approval/confirm modal** — centered, full width at narrow sizes. Title “Approve tool call?”, tool name + args preview, three actions: Allow / Deny / Allow always for this session. Keyboard-first: Enter allows, Esc denies, arrow keys move focus. High-contrast focus ring.
5. **Todo panel** — ordered list; each row shows checkbox glyph (`[ ]` / `[x]`), short title, status chip. Add via `/todo`; completing toggles the row. Completed items fade to `muted`.
6. **Status bar** — left-to-right: model name, `tokens in / out`, cost estimate, elapsed time, retry count, mode dot (interactive / headless / RPC). Two rows at comfortable height, one row at minimum height.
7. **Command palette** — centered overlay listing `/undo`, `/redo`, `/help`, `/approve`, `/deny`, `/todos`, `/logs`. Prefix/fuzzy match as you type; Esc closes; Enter runs.
8. **Log tail panel** — newest JSONL line at bottom; autoscroll on new lines; level color (`system`/`ok`/`error`); optional filter chips.

## 6. Motion & transitions

| Moment | Default animation | Reduced-motion alternative |
|--------|-------------------|----------------------------|
| Typing indicator | Three dots pulse, 600 ms cycle | Static `…` |
| Spinner (bounded retries) | Braille frame cycle, ~100 ms per frame | Static `…` or last frame |
| Unbounded wait | Continuous spin | Static glyph |
| Modal open/close | Slide-in + fade, ~180 ms ease-out | Instant appear/disappear |
| Git snapshot progress | Filled bar + percent + elapsed, linear | Static percent text |
| Token stream | Typewriter appearance, cursor blink | Instant line appearance, no blink |
| Status changes | Cross-fade badge color, ~150 ms | Instant color swap |

All motion is optional and disabled together (one reduced-motion setting, also honored by headless JSON parity — see §7). Cursor blink can be disabled independently for accessibility.

## 7. Headless/RPC visual parity

The TUI consumes the same event stream that headless/RPC mode emits to stdout. Every visible state has a 1:1 JSON event:

| TUI state | JSON event | Fields |
|-----------|------------|--------|
| Message rendered | `message` | `role`, `content`, `ts` |
| Token streamed | `token` | `delta`, `ts` |
| Tool call started | `tool_call` | `id`, `name`, `args`, `status: pending` |
| Tool result | `tool_result` | `id`, `ok`, `output`, `duration_ms` |
| Approval requested | `approval_request` | `id`, `tool`, `args` |
| Approval resolved | `approval` | `id`, `decision` (allow/deny/allow-always) |
| Todo added/completed | `todo` | `id`, `title`, `done` |
| Status bar fields | `status` | `model`, `tokens_in`, `tokens_out`, `cost_usd`, `elapsed_ms`, `retries`, `mode` |
| Git snapshot progress | `snapshot_progress` | `phase`, `percent`, `elapsed_ms` |
| Log line | `log` | `level`, `message` |
| Error | `error` | `code`, `message` |

Headless mode prints identical JSON to stdout/RPC; screen readers and automation consume the canonical JSON UI. The TUI is a renderer of this stream, not a parallel state machine.

## 8. Design tokens

Named constants to be encoded in one theme file (Phase 2). Changing one file re-themes the app; no visual rework.

**Colors** — `canvas.bg`, `canvas.fg`, `canvas.muted`, `canvas.border`; `semantic.ok`, `semantic.warn`, `semantic.error`, `semantic.pending`, `semantic.tool`, `semantic.system`, `semantic.user`, `semantic.assistant`.

**Spacing** (in cells) — `unit = 1`, `gap.small = 1`, `gap.medium = 2`, `pad = 1`, `modal.pad = 2`.

**Frames** — `frame.single` (unicode box-drawing), `frame.double`, `frame.rounded`, `frame.ascii` (legacy-terminal fallback).

**Animation durations** — `dur.modal = 180 ms`, `dur.typing = 600 ms cycle`, `dur.spinner = 100 ms per frame`, `dur.badge = 150 ms`, `dur.progress = linear`.

**Layers** (z-order) — `background < pane < right-rail < status-bar < modal < command-palette`.

**Breakpoints** (cells) — `width.min = 80`, `width.comfortable = 120`, `height.min = 24`, `height.comfortable = 40`.

## 9. Integration contract with Phases 2–6

**Tech stack binding (to confirm in Phase 2):**
- TUI rendering: ratatui (Layout, Block, Paragraph, List, Table, BarChart, buffer drawing).
- Terminal I/O/input: crossterm (raw mode, alternate screen, mouse optional, terminal-size queries).
- Headless/RPC: same event structs serialized to JSONL/stdout; no TUI dependency in the headless binary path.
- No custom pixel painting unless Phase 2 proves ratatui cannot meet a required effect.

**Folder structure:**
- `DESIGN.md` — this brief.
- `src/ui/` — theme/tokens, layout, components, frames.
- `assets/themes/` — theme files (TOML/JSON) consumed by the UI theme module; `dark.toml` and `light.toml` ship by default.
- `src/headless.rs` — JSON event emitter shared with the TUI renderer.

**Testing:**
- Snapshot tests: capture the rendered terminal buffer at fixed sizes (80×24, 120×40) and compare to golden files on every change.
- Resize tests: 80×24 → 120×40 → 40×20; regions reflow, modals re-center, scroll position preserved.
- Contrast audit: automated check that every semantic color meets its ratio target on both canvases.
- Reduced-motion test: with animations disabled, no time-dependent frame changes occur.
- Event-parity test: every TUI state transition produces the matching JSON event in headless mode.

---

**Next step:** Phase 2 confirms the TUI crate (ratatui/crossterm) and implements the UI theme module from §8 tokens. This brief is the visual spec; implementers should not deviate from colors, layout regions, or the §7 event map without design review.