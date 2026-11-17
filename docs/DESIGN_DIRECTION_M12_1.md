# Design Direction: M12.1 — TUI Configuration & Input UX

**Purpose**: Concrete doc updates for `DESIGN.md` and `MILESTONES.md` to address:
- Visible input composer on idle main screen (not hidden behind welcome screen)
- In-TUI provider configuration flow (first-run setup without `okeanos init`)
- Event map additions for config flow
- M12.1 acceptance criteria refinements

---

## 1. Visible Input Composer — Layout & Behavior Changes

### Current problem (from screenshots)
- Welcome screen dismisses on **any key** (`welcome_screen.rs:74-81`)
- `render_ui()` renders **only** welcome screen when visible (`mod.rs:671-674`)
- Input composer exists but is **never visible** until welcome screen dismissed
- User sees empty panes with no focusable input area

### Design changes

#### DESIGN.md §4 Layout Grid (lines 83-91)
**Update the minimum breakpoint row (80×24):**
```
| 80×24 | minimum | Single column: header (1 row), conversation (rest − 5), **input composer (2–3 rows, always visible)**, status bar (2 rows). Welcome screen overlays as centered modal; composer remains below it, focused. Right-rail content folds into conversation bottom as collapsible sections. |
```

**Add a note after the breakpoint table:**
> **Input composer visibility**: The input composer is **always rendered** in the main layout (above status bar). The welcome screen renders as a centered modal overlay (`Clear` + panel) that does **not** replace the main layout. On first run with no config, the composer starts focused with placeholder "Describe what you want to do…" and key hints visible. The welcome modal offers a "Configure provider" action (Enter) alongside "Dismiss" (Esc).

#### DESIGN.md §5 Component Hierarchy — Item 9 Input Composer (line 116)
**Update the placeholder behavior:**
> Placeholder: **"Describe what you want to do…"** (no config loaded) or **"Type your prompt…"** (config loaded). Key hints on right: `Enter` send · `Shift+Enter` newline · `Ctrl+C` cancel · `Ctrl+D` exit. During in-flight turn: disabled, shows spinner + "Running…" status. **Always rendered in layout; never hidden by welcome screen.**

#### DESIGN.md §5 Component Hierarchy — Item 10 Welcome Screen (line 118)
**Rewrite entirely:**
> **Welcome screen** — centered modal overlay (not a full-screen replacement) shown on startup when no config exists and no prompt provided via CLI. Panel contains: app name/version, provider/model status (configured / not configured), key bindings summary (`:` palette, `Enter` send, `Shift+Enter` newline, `Ctrl+C` cancel, `Ctrl+D` quit), and two actions: **[Enter] Configure provider** · **[Esc] Dismiss**. Does not auto-dismiss on arbitrary keys. Dismisses on Esc or when "Configure provider" completes. Input composer remains visible and focused underneath.

#### DESIGN.md §4 Region Definitions (lines 92-100)
**Update Input composer bullet (line 98):**
> - **Input composer** (1+ rows, above status bar): multi-line text area for user prompts; **always rendered in layout**; shows key hints (`Enter` send, `Shift+Enter` newline, `Ctrl+C` cancel turn, `Ctrl+D` exit); disabled during in-flight turn. Placeholder reflects config state. Supports slash commands inline (`/undo`, `/redo`, `/help`, `/todos`, `/logs`, `/approve`, `/deny`, `/config`).

**Add Welcome screen to region definitions (after Modal layer):**
> - **Welcome overlay** (centered modal): shown on first run when no config; offers "Configure provider" and "Dismiss" actions; dims background; input composer stays visible and focused beneath.

---

## 2. In-TUI Provider Configuration Flow

### New component: Config Modal/Form
Add to DESIGN.md §5 Component Hierarchy as **Item 11**:

> **11. Configuration modal** — centered overlay (wider than welcome, ~70% width, max 100 cols). Stepper flow:
> - **Step 1: Provider kind** — radio/list: OpenRouter (OpenAI-compatible), Custom OpenAI-compatible, Ollama (local). Default: OpenRouter.
> - **Step 2: Base URL** — prefilled per provider (OpenRouter: `https://openrouter.ai/api/v1`, Ollama: `http://localhost:11434/v1`), editable.
> - **Step 3: API key** — input with `password` masking toggle; hint: "Use `{env:VAR}` or `{file:path}` for secrets". Validation: non-empty unless Ollama.
> - **Step 4: Model** — if provider supports discovery (OpenRouter), fetch & show searchable list; else free-text input with known model suggestions.
> - **Step 5: Confirm & write** — summary panel; **Enter** writes `okeanos.toml` at git root (or CWD), emits `ConfigSaved` event, dismisses modal, updates welcome screen status to "Configured ✓", composer placeholder changes to "Type your prompt…".
> - **Navigation**: `Tab`/`Shift+Tab` between fields, `Enter` next step, `Esc` back/cancel, `Ctrl+C` abort entirely.
> - **Accessibility**: high-contrast focus ring, labels associated with inputs, reduced-motion respected.

### Config file written
- Path: `okeanos.toml` at git root (walk up from CWD) or CWD if no git repo
- Format (TOML):
```toml
[model]
name = "selected-model"
provider = "openrouter"  # or "openai-compatible", "ollama"

[provider]
base_url = "https://openrouter.ai/api/v1"
api_key = "{env:OPENROUTER_API_KEY}"  # or {file:~/.openrouter_key}
```
- Secret substitution uses existing `{env:VAR}` / `{file:path}` syntax (M1)

---

## 3. DESIGN.md §7 Event Map Additions

Add these rows to the event table (after line 150):

| TUI state | JSON event | Fields |
|-----------|------------|--------|
| Config modal opened | `config_modal` | `action: "open"`, `step: 1` |
| Config step changed | `config_modal` | `action: "step"`, `step: 1-5`, `data: {provider_kind?, base_url?, api_key?, model?}` |
| Config saved | `config_saved` | `path`, `provider_kind`, `model`, `base_url` |
| Config modal closed | `config_modal` | `action: "close"`, `completed: boolean` |
| Provider status changed | `status` | `model`, `provider_configured: true`, `tokens_in`, `tokens_out`, `cost_usd`, `elapsed_ms`, `retries`, `mode` |

**Rationale**: Headless/RPC parity — scripts can detect config state via `config_saved` event; `status` event already exists, just gains `provider_configured` field.

---

## 4. MILESTONES.md M12.1 Refinements

### Scope updates (lines 619-622)
**Replace with:**
> **Scope:**
> - **Visible input composer always rendered**: main layout includes input composer at bottom (above status bar) at all times; welcome screen renders as centered modal overlay, not full-screen replacement. Composer focused on startup with config-aware placeholder.
> - **In-TUI provider configuration**: first-run flow as a stepper modal (provider kind → base URL → API key → model → confirm) writing `okeanos.toml` at git root/CWD. Supports `{env:VAR}` / `{file:path}` secret substitution. Welcome screen offers **[Enter] Configure provider** action.
> - **Config events**: add `config_modal`, `config_saved` to event stream (DESIGN.md §7); `status` event gains `provider_configured` field.
> - **Key binding updates**: welcome screen no longer dismisses on arbitrary keys; uses `Esc` to dismiss, `Enter` to open config. Composer key hints unchanged.

### Acceptance criteria updates (lines 626-630)
**Replace with:**
> **Acceptance criteria:**
> - Launching `./okeanos` with no config opens TUI with **visible, focused input composer** at bottom showing placeholder "Describe what you want to do…" and key hints.
> - Welcome screen appears as **centered modal overlay** (not full screen) with "Configure provider" (Enter) and "Dismiss" (Esc) actions.
> - Pressing **Enter** on welcome screen opens config stepper modal; user can complete provider setup entirely within TUI.
> - On config save: `okeanos.toml` written at git root/CWD, welcome modal updates to "Provider: Configured ✓", composer placeholder changes to "Type your prompt…", user can immediately submit a prompt.
> - No terminal commands required beyond `./okeanos` to reach a working interactive state.
> - Headless mode emits `config_saved` event on completion for script consumption.

### Testing requirements (line 631)
**Update:**
> Unit tests for config stepper modal (validation, secret syntax, file write), welcome overlay key handling (Esc/Enter), input composer visibility when welcome overlay active. Integration test: cold start → config → prompt → turn. No network (mock provider).

---

## 5. Implementation Notes (for Phase 2 implementers)

### Files to modify (design spec only — no code here)
- `src/ui/components/welcome_screen.rs` — change key handling: `Esc` dismiss, `Enter` emits `OpenConfig` action; render as modal overlay (use `Clear` + centered panel)
- `src/ui/components/input_composer.rs` — ensure `visible: true` default; placeholder switches via `set_placeholder()`
- `src/ui/mod.rs` — `render_ui()`: always render main layout first, then welcome overlay if visible; `handle_key()`: route keys to welcome overlay when visible (Esc/Enter only), else to composer
- New: `src/ui/components/config_modal.rs` — stepper form component per §2 above
- `src/events.rs` — add `ConfigModal`, `ConfigSaved` event variants

### Layout behavior
- `LayoutRects` unchanged — composer area calculated same way
- Welcome overlay uses `centered_rect()` (existing helper) over full frame area
- Config modal uses wider centered rect (~70% width, height ~20 rows)

### Focus management
- Startup: welcome overlay visible, **composer focused** (cursor blinking in composer)
- `Esc` on welcome: dismiss overlay, composer keeps focus
- `Enter` on welcome: open config modal, **config modal focused**
- Config modal complete: dismiss both overlays, composer focused, placeholder updated
- During turn: composer disabled, focus moves to conversation (scroll keys work)

---

## 6. Open Questions (resolve before Phase 2)

1. **Config file location**: git root vs CWD — DESIGN.md says "git root or CWD" (M1 behavior). Confirm.
2. **Provider discovery**: OpenRouter `/models` fetch in config modal step 4 — async in TUI? Show spinner?
3. **Multiple configs**: Support project-local + global? M12.1 writes project-local only per M1 pattern.
4. **Migration**: If `okeanos.toml` exists but invalid — show error in welcome overlay or config modal?
5. **Theme**: Config modal uses same design tokens (§8) — confirm border style `frame.rounded` for modals.

---

## Summary of Doc Changes

| File | Section | Change Type |
|------|---------|-------------|
| `DESIGN.md` | §4 Layout Grid | Update minimum breakpoint row; add composer visibility note |
| `DESIGN.md` | §4 Region Definitions | Update input composer bullet; add welcome overlay bullet |
| `DESIGN.md` | §5 Component Hierarchy | Rewrite item 9 (composer), item 10 (welcome); add item 11 (config modal) |
| `DESIGN.md` | §7 Event Map | Add 5 config-related event rows |
| `MILESTONES.md` | M12.1 Scope | Replace with detailed scope above |
| `MILESTONES.md` | M12.1 Acceptance Criteria | Replace with detailed criteria above |
| `MILESTONES.md` | M12.1 Testing | Update testing requirements |

---

**Next step**: Phase 2 implementer updates `DESIGN.md` and `MILESTONES.md` per this direction, then begins implementation. Design review gate before code.