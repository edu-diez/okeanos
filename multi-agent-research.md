# Multi-Agent Delegation in AI Coding Agents: Implementation Comparison

Research conducted November 2026 across official docs, GitHub repos, and technical deep-dives.

---

## Executive Summary

| Agent | Sub-Agent Model | Context Isolation | Max Depth | Parallel Exec | Worktree Isolation | Built-in Types |
|-------|-----------------|-------------------|-----------|---------------|-------------------|----------------|
| **Claude Code** | Task tool (Agent) | Full (clean slate) | 3 levels | Yes | Optional (`isolation: worktree`) | Explore, Plan, General, claude, web-fetch |
| **Codex** | Spawn/wait/close tools | Full (clean slate) | Unlimited* | Yes (default 6 concurrent) | Not native | default, worker, explorer |
| **OpenCode** | `subagent` tool | Full (child session) | 1 level (configurable) | Yes | Not native | build, plan, general, explore |
| **Pi** | `subagent` tool (package) | Full (detached runner) | 1 level (configurable 64 spawns/run) | Yes | Optional (worktree isolation) | scout, researcher, evidence-auditor, worker, reviewer, oracle, delegate |
| **Antigravity** | `invoke_subagent` tool | Full (clean slate) | Unlimited | Yes | 3 modes: inherit/branch/share | research, browser, self |
| **Cursor** | Task tool (auto + explicit) | Full (clean slate) | 2 levels (parent + 1 child) | Yes | Optional (cloud subagents) | explore, browser |
| **Windsurf** | Fast Context (internal) + Cascade | Specialized (retrieval-only) | N/A (internal) | Yes (8 parallel calls) | N/A | Fast Context (SWE-grep) |
| **Devin** | Subagents (CLI) | Shared tools/context, separate conversation | Unlimited | Yes | Cloud sessions | Various (cloud) |
| **OpenHands** | Delegation tools (DelegateAction) | Shared workspace, separate conversation | Unlimited (opt-in) | Yes | Shared workspace | Worker preset |

*Codex has configurable `max_concurrent_threads_per_session` (default 6)

---

## 1. Claude Code (Anthropic)

### How Sub-Agents Are Spawned
- **Tool**: `Agent` tool (renamed from `Task` in v2.1.63)
- **Invocation**: Automatic (Claude decides based on subagent `description`) or explicit via `@-mention` or "Use the X subagent"
- **CLI**: `--agents` flag for session-only definitions

### Context Model
- **Complete isolation**: Each subagent starts with a **clean context window**
- **Parent context NOT inherited**: No conversation history, no prior tool results
- **What IS passed**: Only the delegation prompt + working directory + basic env (CWD, git status for some)
- **What loads at startup**: CLAUDE.md files (unless `omitClaudeMd: true`), MCP servers, skills (if configured), but NOT parent's conversation

### Tool Subsets
- **Inheritance**: Inherits parent's available tools minus a deny-list
- **Always removed**: `Agent` (at depth limit), `AskUserQuestion`, `EndConversation`, `EnterPlanMode`, `ExitPlanMode`, `propose_skills`, `ScheduleWakeup`, `WaitForMcpServers`, `Workflow`
- **Background subagents** (default): Further reduced — keeps only read/edit/bash/glob/grep/LSP/WebFetch/WebSearch/TodoWrite/Skill/ToolSearch/EnterWorktree/ExitWorktree/Monitor/TaskStop/SendMessage/Artifact/ListPlugins/ListSkills/SearchPlugins/SearchSkills + MCP tools
- **Restriction**: `tools` (allowlist) and `disallowedTools` (denylist) fields in frontmatter
- **MCP scoping**: `mcpServers` field for subagent-specific servers

### Result Aggregation
- Subagent returns **compact summary** to parent via `SubagentHandback` tool
- Parent sees: subagent name, task description, result summary
- Full transcript available via `/tasks` command or debug logs
- **Resumable**: `maxTurns` limit triggers partial result; parent can resume with follow-up

### Approval/Confirmation
- **No approval needed** for spawning (Claude decides autonomously)
- **Permission modes** inherited: subagent runs in parent's permission mode unless `permissionMode` set in frontmatter
- **Hooks**: `SubagentStart`, `SubagentStop` for lifecycle monitoring
- **Deny specific subagents**: `permissions.deny: ["Agent(subagent-name)"]`

### Token Budget / Depth Limits
- **Max nesting depth**: 3 levels below main conversation (configurable via `CLAUDE_CODE_SUBAGENT_MAX_DEPTH`?)
- **Turn limit**: `maxTurns` frontmatter field (default: no limit)
- **Auto-compact**: `autoCompactWindow` (min 100k tokens)
- **Description token limit**: 15,000 tokens combined for all custom subagent descriptions
- **Cost**: Counts toward same usage limits as main conversation

### Configuration Options
```yaml
# .claude/agents/name.md frontmatter
name: string (required, max 256 chars, no colons)
description: string (required)  # delegation trigger
tools: "Read, Grep, Glob" | []  # allowlist
disallowedTools: "Write, Edit"  # denylist
model: "sonnet" | "opus" | "haiku" | "inherit" | full-model-id
permissionMode: "default" | "acceptEdits" | "auto" | "dontAsk" | "bypassPermissions" | "plan"
maxTurns: integer
autoCompactWindow: integer (>= 100000)
skills: ["skill-path"]
mcpServers: [{server-name: config}] | ["server-name"]
hooks: {SubagentStart: ..., SubagentStop: ...}
memory: "user" | "project" | "local"
background: true | false
omitClaudeMd: true | false
effort: "low" | "medium" | "high" | "xhigh" | "max"
isolation: "worktree"  # git worktree isolation
color: "red" | "blue" | "green" | "yellow" | "purple" | "orange" | "pink" | "cyan"
initialPrompt: string
experimental: {cacheTtl: "5m" | "1h"}
```

### Scopes (priority order)
1. Managed settings (org-wide)
2. `--agents` CLI flag (session-only)
3. `.claude/agents/` (project)
4. `~/.claude/agents/` (user)
5. Plugin `agents/` directory

### Agent Teams (Experimental)
- Separate from subagents: **independent Claude Code instances**
- Shared task list, mailbox messaging, tmux/iTerm2 split panes
- Enable: `CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS=1`
- Teammates can message each other directly (not just via parent)

---

## 2. Codex (OpenAI)

### How Sub-Agents Are Spawned
- **Tools**: `spawn_agent`, `wait_agent`, `close_agent` (built-in)
- **Invocation**: Explicit user request ("spawn two agents"), or via `AGENTS.md`/skill instructions
- **Config**: `~/.codex/agents/*.toml` (user) or `.codex/agents/*.toml` (project)

### Context Model
- **Complete isolation**: Each subagent gets **fresh context window**
- **Parent context NOT inherited**: No conversation history
- **What IS passed**: Explicit spawn prompt with task description
- **Inheritance**: Model, reasoning effort, sandbox policy, approval policy, MCP servers, provider from parent

### Tool Subsets
- **Inheritance**: Full parent toolset by default
- **Custom agent restriction**: `features.shell_tool`, `features.apps`, `features.plugins`, `features.memories`, `features.request_permissions_tool` can be set to `false`
- **Skills**: `skills.config` to disable individual skills, `skills.bundled.enabled = false`
- **Cannot override**: `sandbox_mode`, `approval_policy`, `model_provider`, `mcp_servers` in custom agent file

### Result Aggregation
- Codex **orchestrates automatically**: spawns, routes follow-ups, waits for results, closes threads
- **Blocking wait**: Parent waits for all requested subagents by default
- **Consolidated response**: Returns combined summary from all subagents
- **Thread inspection**: UI shows each subagent thread; user can open to inspect progress/results

### Approval/Confirmation
- **No approval for spawning** (user explicitly requests or AGENTS.md authorizes)
- **Permission mode**: Selected on parent turn before delegation; subagents inherit
- **Sandbox policy**: Inherited from parent; custom agents can't create separate read-only sandbox
- **Interrupt message**: `agents.interrupt_message` (default true) records model-visible message on interruption

### Token Budget / Depth Limits
- **Concurrency limit**: `agents.max_concurrent_threads_per_session` (default 6, legacy `max_threads`)
- **No explicit nesting depth limit** documented
- **Token cost**: "Subagent workflows consume more tokens than comparable single-agent runs" — each subagent does own model/tool work
- **Model/reasoning**: Subagent inherits parent's model + reasoning effort unless overridden

### Configuration Options
```toml
# ~/.codex/config.toml
[agents]
enabled = true
max_concurrent_threads_per_session = 8
default_subagent_model = "gpt-6-luna"
default_subagent_reasoning_effort = "high"
interrupt_message = true

# ~/.codex/agents/custom-agent.toml
name = "reviewer"
description = "PR reviewer focused on correctness, security, missing tests"
model = "gpt-6.2-sol"
model_reasoning_effort = "medium"
developer_instructions = """
Review code like an owner. Do not edit files.
Prioritize correctness, security, behavior regressions, missing test coverage.
"""

# Feature flags
features.shell_tool = false
features.apps = false
skills.config = [{path = "...", enabled = false}]
```

### Built-in Agents
- `default`: General-purpose fallback
- `worker`: Execution-focused (implementation/fixes)
- `explorer`: Read-heavy codebase exploration

---

## 3. OpenCode (This Project)

### How Sub-Agents Are Spawned
- **Tool**: `subagent` tool (built-in)
- **Invocation**: Parent agent calls `subagent` tool with agent name and prompt
- **Config**: `.opencode/agents/*.md` (project) or `~/.config/opencode/agents/*.md` (user) or `opencode.jsonc`

### Context Model
- **Complete isolation**: Subagents run in **child sessions with fresh context**
- **Parent context NOT inherited**: "Subagents run with fresh context in their own child sessions"
- **What IS passed**: The `subagent` tool's prompt string (task description)
- **Background only**: "They always run in the background: the parent receives a report when a child goes idle, and again after each later round of work"

### Tool Subsets
- **Own permissions**: "The child uses its own configured permissions"
- **Parent control**: Parent's `subagent` permissions control which agents it may launch
- **Permission system**: Ordered rules (action, resource, effect: allow/ask/deny)
- **Actions**: `shell`, `edit`, `subagent`, `read`, `glob`, `grep`, `webfetch`, `websearch`, `skill`, `external_directory`

### Result Aggregation
- **Async reporting**: Parent receives report when child goes idle, then after each work round
- **No blocking wait**: Parent continues working; gets notifications
- **FleetView**: TUI shows active subagents, transcripts, can steer/stop

### Approval/Confirmation
- **Permission gating**: Parent must have `subagent` permission for specific agent
- **Tool activation**: Fresh sessions expose `subagents_enable` loader; model calls it when delegation authorized
- **No auto-delegation**: "Complexity alone does not authorize delegation"

### Token Budget / Depth Limits
- **Nesting depth**: **1 level only** — "Children cannot start subagents of their own. The nesting depth is one, whatever `experimental.subagent_depth` says"
- **Spawn budget**: `maxSubagentSpawnsPerRun` (default 64) — cumulative logical children in one run tree
- **Steps limit**: `steps` field (positive integer) — max model steps before forced summary

### Configuration Options
```yaml
# .opencode/agents/reviewer.md
---
description: Reviews changes for correctness and regressions
mode: subagent  # or primary, all
model: anthropic/claude-sonnet-5-5#high
permissions:
  - action: edit
    resource: "*"
    effect: deny
  - action: shell
    resource: "*"
    effect: deny
---

Review the current changes. List findings in severity order with file and line references.
```

```jsonc
# opencode.jsonc
{
  "$schema": "https://opencode.ai/config.json",
  "default_agent": "writer",
  "agents": {
    "orchestrator": {
      "permissions": [
        { "action": "subagent", "resource": "*", "effect": "deny" },
        { "action": "subagent", "resource": "reviewer", "effect": "allow" },
      ],
    },
  },
}
```

**Agent Fields**: `description`, `mode`, `model`, `system`, `permissions`, `steps`, `hidden`, `color`, `disabled`, `request`

### Built-in Agents
| Agent | Mode | Purpose |
|-------|------|---------|
| `build` | primary | Default coding agent; broad tool access |
| `plan` | primary | Explore/plan without editing |
| `general` | subagent | Research/multi-step; broad tools; **cannot launch subagents** |
| `explore` | subagent | Read-only codebase search |

### pi-subagents Package (Pi on OpenCode)
- Two-way communication: children can message/question parent
- Peer messaging, batches, context forks, isolated worktrees
- Watchdog reviewer, supervisor messaging

---

## 4. Pi (Inflection) — via `pi-subagents` Package

### How Sub-Agents Are Spawned
- **Tool**: `subagent` tool (added by package)
- **Invocation**: Natural language ("Use reviewer to review this diff") or explicit `/verifier`
- **Auto-activation**: Fresh sessions expose `subagents_enable` loader; model calls it when authorized

### Context Model
- **Complete isolation**: Each subagent is a **focused child Pi session**
- **Foreground vs Background**:
  - Foreground: Runs inside parent Pi process, streams in conversation
  - Background: Runs in detached runner process, keeps working, checkable later
- **Parent context NOT inherited**: Only task prompt passed

### Tool Subsets
- **Per-agent config**: Each built-in agent has tailored tool access
- **Web research**: `researcher` and `evidence-auditor` require `pi-web-access` in child
- **Watchdog**: Opt-in adversarial change reviewer with LSP checks
- **Extension API**: Can define custom tools, skills, MCP servers per agent

### Result Aggregation
- **Foreground**: Streams progress in conversation
- **Background**: FleetView in TUI shows active work; `/subagents-fleet` inspector to browse/steer/stop
- **Missions**: Durable mission records with delivery receipts, timed/recurring runs

### Approval/Confirmation
- **Tool activation gate**: Delegation must be authorized by prompt or project instructions
- **Acceptance gates**: Configurable via `subagent` tool parameters
- **Supervisor replies**: `bg_wait` and supervisor replies available without activation

### Token Budget / Depth Limits
- **Nesting depth**: 1 level (configurable but enforced at 1)
- **Spawn budget**: `maxSubagentSpawnsPerRun` (default 64) — cumulative per run tree
- **Concurrency**: Separate from spawn budget

### Configuration Options
```json
// config.json keys (from pi-subagents docs)
{
  "toolActivation": "auto",  // when to enable subagent tool
  "maxSubagentSpawnsPerRun": 64,
  "agents": {
    "custom-name": {
      "model": "anthropic/claude-sonnet-5-5",
      "tools": [...],
      "extensions": [...],
      "skills": [...]
    }
  }
}
```

### Built-in Agents
| Agent | Purpose |
|-------|---------|
| `scout` | Fast local codebase recon |
| `researcher` | Web/docs research with sources (needs pi-web-access) |
| `evidence-auditor` | Checks research claims against sources (needs pi-web-access) |
| `worker` | Implementation; edits files, validates, escalates unapproved decisions |
| `reviewer` | Read-only code review against task/plan/tests/edge cases |
| `oracle` | Second opinion; challenges assumptions without editing |
| `delegate` | Lightweight general delegate (close to parent session) |

### Workflows
- `/council` and `council-mode`: Model-based advisor debate
- `/parallel-review`, `/review-loop`: Packaged prompt shortcuts
- Worktree isolation for parallel implementation

---

## 5. Antigravity (Google)

### How Sub-Agents Are Spawned
- **Tool**: `invoke_subagent` tool
- **Invocation**: Parent agent calls tool with role + initial prompt
- **Config**: `.agents/agents/<name>.md` (workspace) or `~/.gemini/config/agents/<name>.md` (global) or plugins

### Context Model
- **Complete isolation**: "Does not inherit the parent's existing conversation history (context window), starting with a clean slate"
- **Workspace options**:
  - `inherit`: Same workspace as parent
  - `branch`: Isolated Git worktree
  - `share`: Shared directory storage
- **Direct messaging**: Can message subagent directly via its conversation ID

### Tool Subsets
- **Explicit `tools` list**: `view_file`, `replace_file_content`, `grep_search`, `run_command`, etc.
- **Model selection**: `inherit`, `flash`, `pro`
- **Command execution policy**: `off`, `auto`, `eager`, `sandbox`
- **MCP servers**: Per-subagent `mcpServers` config
- **Skills/Plugins**: Per-subagent `skills` / `plugins` arrays

### Result Aggregation
- **Async background**: Subagent runs independently, parent monitors via subagent panel (Alt+J in CLI)
- **States**: Running → Idle (completed, sent result) → Terminated
- **Auto-wake**: Sending message to idle subagent re-awakens it with full context retention
- **Transcripts**: Historical JSONL logs remain readable

### Approval/Confirmation
- **Permission bubbling**: Subagent auth requests bubble up to main UI/panel
- **Inherited safety**: Subagents inherit parent's allowed command prefixes, file scopes, sandbox settings
- **Parent access**: Parent retains full access to subagent workspaces (including isolated worktrees)

### Token Budget / Depth Limits
- **No documented depth limit** for subagent nesting
- **Inter-agent communication**: Flexible routing via agent conversation IDs
- **Shared transcripts**: Agents can read each other's conversation transcripts

### Configuration Options
```yaml
# .agents/agents/code-auditor.md
---
name: code-auditor
description: Specialized subagent for security audits, static analysis, code quality reviews.
tools:
  - view_file
  - grep_search
  - run_command
subagent: true
mainAgent: false
model: pro
commandExecutionPolicy: sandbox
skills:
  - skills/security-checklist
---

# System Prompt
You are an expert security auditor...
```

### Built-in Subagents
| Agent | Purpose |
|-------|---------|
| `research` | Codebase research, file navigation, structural exploration |
| `browser` | Web interaction, UI playtesting, screenshots, console/network inspection |
| `self` | Direct clone of calling agent (identical system instructions + toolset) |

### Multi-Agent Orchestrators (Higher-level than subagents)
| Orchestrator | Use Case | Mechanism |
|--------------|----------|-----------|
| **Boost** (`/boost`) | Hard problems (bugs, algorithms) | Iterative pipeline: implement → challenge → fix → verify |
| **Swarm** (`/swarm`) | Broad, divisible tasks | Named workers + verifier on shared message board with @mentions |
| **Teamwork** (`/teamwork`) | Long-horizon projects | Scoping interview → milestone decomposition → specialized roles + audit gates |

---

## 6. Cursor

### How Sub-Agents Are Spawned
- **Tool**: `Task` tool (auto + explicit)
- **Invocation**: Automatic (Agent decides) or explicit via `/name` syntax or natural mention
- **Config**: `.cursor/agents/*.md` (project) or `~/.cursor/agents/*.md` (user) — also reads `.claude/agents/` and `.codex/agents/`

### Context Model
- **Complete isolation**: "Each subagent has its own context window. Subagents start with a clean context."
- **Parent context NOT inherited**: "The parent agent includes relevant information in the prompt since subagents don't have access to prior conversation history."
- **What IS passed**: Full task prompt with necessary context

### Tool Subsets
- **Inheritance**: All parent tools by default
- **`readonly: true`**: Restricts to read-only (no file edits, no state-changing shell commands)
- **`is_background: true`**: Runs without blocking parent
- **Cloud subagents**: Run on cloud VM with team's MCP servers (not local)

### Result Aggregation
- **Foreground**: Blocks until complete, returns result immediately
- **Background**: Returns immediately; parent checks progress via `~/.cursor/subagents/`
- **Resumable**: Each execution returns agent ID; pass to resume with full context preserved
- **Parallel**: "Agent sends multiple Task tool calls in a single message, so subagents run simultaneously"

### Approval/Confirmation
- **No approval for spawning** (automatic or explicit request)
- **Team admin restrictions**: Can block models
- **Plan limitations**: May fall back to compatible model

### Token Budget / Depth Limits
- **Nesting depth**: **2 levels** — "Since Cursor 2.5, subagents can launch child subagents... main agent and its direct subagents can launch subagents, but a subagent launched by another subagent can't launch further ones"
- **Concurrency**: No hard limit documented
- **Token cost**: "Running five subagents in parallel uses roughly five times the tokens of a single agent"
- **Model params**: `model: "claude-opus-5[effort=high,context=300k]"`

### Configuration Options
```yaml
# .cursor/agents/security-auditor.md
---
name: security-auditor
description: Security specialist. Use when implementing auth, payments, or handling sensitive data.
model: inherit  # or specific model ID
readonly: true
is_background: false
---

You are a security expert auditing code for vulnerabilities.
When invoked:
1. Identify security-sensitive code paths
2. Check for common vulnerabilities...
```

### Built-in Subagents
| Subagent | Purpose | Model |
|----------|---------|-------|
| `explore` | Codebase search/analysis | Faster model (parallel searches) |
| `browser` | Browser automation via MCP | Filters DOM/screenshots to relevant results |

### Cloud Subagents
- `/in-cloud`: Next task runs on cloud VM with own branch
- `/autopilot`: Cloud agent takes over PR, iterates remotely
- Uses team's MCP config from cursor.com/agents

---

## 7. Windsurf (Codeium)

### Sub-Agent Architecture
Windsurf's approach is different: **internal specialized subagents** rather than user-creatable delegation.

#### Fast Context (Specialized Retrieval Subagent)
- **Purpose**: Code retrieval for Cascade
- **Models**: `SWE-grep` (high-intelligence) and `SWE-grep-mini` (ultra-fast, 2800 tok/s)
- **Execution**: Up to 8 parallel tool calls/turn, max 4 turns
- **Tools**: Restricted to `grep`, `read`, `glob` (cross-platform)
- **Trigger**: Automatic when Cascade needs code search
- **User-facing**: Not directly invocable; transparent to user

#### Cascade (Main Agent)
- **Modes**: `CONVERSATIONAL_PLANNER_MODE_DEFAULT` (write), `CONVERSATIONAL_PLANNER_MODE_READ_ONLY` (read), `CONVERSATIONAL_PLANNER_MODE_NO_TOOL` (legacy)
- **No user-creatable subagents** documented in public API
- **Devin Local**: "Subagents share tools and codebase context with the parent agent but operate in their own conversation chain" — but this is Devin integration, not native Windsurf

### What Windsurf Does NOT Have
- User-defined subagent files
- Explicit delegation tool for users
- Configurable subagent tool subsets
- Nesting depth controls

---

## 8. Devin (Cognition)

### Sub-Agents (CLI)
- **Tool**: Native subagent support in Devin CLI
- **Context**: "Subagents share tools and codebase context with the parent agent but operate in their own conversation chain"
- **Isolation**: Less isolated than others — shares tools/context
- **Cloud sessions**: Separate VMs for cloud subagents

### Key Difference
- Devin's subagents **share tools and codebase context** with parent (unlike Claude/Cursor/Codex/OpenCode/Pi/Antigravity which use clean context)
- More like "conversation branches" than isolated workers

---

## 9. OpenHands (All Hands AI)

### Delegation System
- **Tools**: `DelegationTool` and `TaskToolSet` in `openhands.tools`
- **Architecture**: `DelegationManager` (in-memory state + message routing)
- **Communication**: Main ↔ Sub only (no sub-to-sub)
- **Workspace**: Shared with parent
- **Worker Agent Preset**: Specialized config without delegation tools (prevents infinite recursion)

### Implementation (from PR #712)
```python
# Enable delegation
main_agent = get_default_agent(llm=llm, enable_delegation=True)

# Agent automatically uses delegation tools to:
# 1. Spawn sub-agents for parallel tasks
# 2. Coordinate their work
# 3. Aggregate results
# 4. Provide comprehensive analysis
```

### Sub-Agent Types
- File-based agents via `register_file_agents`
- Built-in subagent types in `openhands/sdk/subagent/builtins/`

---

## Comparative Analysis: Key Design Decisions

### Context Isolation Spectrum
| Full Isolation (Clean Slate) | Shared Context |
|------------------------------|----------------|
| Claude Code | Devin |
| Codex | OpenHands (workspace) |
| OpenCode | Windsurf (Fast Context → Cascade) |
| Pi | |
| Antigravity | |
| Cursor | |

**Trade-off**: Full isolation prevents context pollution but requires explicit context passing; shared context enables easier collaboration but risks context rot.

### Nesting Depth Philosophy
| Limited (1-3) | Unlimited |
|---------------|-----------|
| OpenCode (1) | Codex |
| Pi (1) | Antigravity |
| Cursor (2) | OpenHands |
| Claude Code (3) | Devin |

**Rationale**: Limited depth prevents runaway agent trees; unlimited enables recursive problem decomposition.

### Parallel Execution Models
| Explicit Parallel (User Controls) | Automatic Parallel (Agent Decides) |
|-----------------------------------|------------------------------------|
| Codex (spawn multiple, wait_all) | Claude Code (auto-delegates) |
| Cursor (multiple Task calls) | Cursor (auto-delegates) |
| Antigravity (invoke multiple) | |
| OpenCode (fire-and-forget background) | |

### Workspace Isolation
| Git Worktree / Branch | Shared Workspace | Cloud VM |
|-----------------------|------------------|----------|
| Claude Code (`isolation: worktree`) | OpenHands | Cursor (`/in-cloud`) |
| Antigravity (`branch` mode) | Devin | Codex (sandbox) |
| Pi (worktree isolation workflows) | OpenCode | Windsurf (cloud) |
| Cursor (swarm isolation) | | |

### Configuration Philosophy
| Declarative Files (Markdown/TOML) | Programmatic / API | UI / Natural Language |
|-----------------------------------|--------------------|----------------------|
| Claude Code (.md) | OpenHands (SDK) | Pi (natural language) |
| Codex (.toml) | | Cursor (natural language) |
| OpenCode (.md + JSONC) | | Antigravity (natural language) |
| Antigravity (.md) | | |
| Cursor (.md) | | |

---

## Notable Patterns & Innovations

### 1. **Claude Code's Background-by-Default**
Subagents run async; parent gets periodic reports. Enables true parallelism without blocking.

### 2. **Codex's Orchestration Layer**
Built-in `spawn_agent`/`wait_agent`/`close_agent` tools handle lifecycle automatically — parent doesn't manage threads.

### 3. **OpenCode's Permission-Gated Delegation**
Parent's `subagent` permissions control WHICH agents can be spawned — security boundary at delegation point.

### 4. **Pi's Detached Background Runner**
Background subagents survive parent process; FleetView provides observability/steering.

### 5. **Antigravity's Three Workspace Modes**
`inherit` / `branch` / `share` — explicit choice per subagent for collision handling.

### 6. **Antigravity's Orchestrators (Boost/Swarm/Teamwork)**
Higher-level patterns: iterative refinement, parallel workers + verifier, milestone-gated teams.

### 7. **Cursor's Cloud Subagents**
Seamless handoff to cloud VM (`/in-cloud`, `/autopilot`) — local session stays responsive.

### 8. **Windsurf's Fast Context**
Specialized retrieval subagent (SWE-grep models) — not user-facing but architectural pattern.

### 9. **OpenHands' DelegationManager**
In-memory relationship tracking + message routing — programmatic, composable.

### 10. **Claude Code's Agent Teams**
Separate processes with mailbox messaging + shared task list — beyond subagents to true multi-agent.

---

## Open Questions / Areas for Further Research

1. **Token accounting**: How exactly do each system account tokens across subagents? (Claude: same limits; Codex: more tokens; Cursor: ~5x)
2. **Failure handling**: Subagent crashes, timeouts, partial results — recovery mechanisms?
3. **Cross-session persistence**: Can subagent state survive parent session restart? (Claude: `/resume` doesn't restore in-process teammates)
4. **Security boundaries**: Tool permission inheritance vs. explicit grants — audit trails?
5. **Cost optimization**: Model routing (Haiku for exploration, Opus for reasoning) — automated vs. manual?
6. **Evaluation**: Benchmarks for multi-agent vs. single-agent on coding tasks?

---

## Sources

- **Claude Code**: https://code.claude.com/docs/en/sub-agents, https://code.claude.com/docs/en/agent-teams
- **Codex**: https://learn.chatgpt.com/docs/agent-configuration/subagents, https://developers.openai.com/codex/multi-agent.md
- **OpenCode**: https://opencode.ai/docs/agents/, pi-subagents package docs
- **Pi**: https://pi.dev/packages/pi-subagents, https://github.com/nicobailon/pi-subagents
- **Antigravity**: https://antigravity.google/docs/subagents, https://antigravity.google/docs/swarm, https://antigravity.google/docs/teamwork
- **Cursor**: https://cursor.com/docs/subagents
- **Windsurf**: https://docs.windsurf.com/context-awareness/fast-context, https://docs.windsurf.com/windsurf/cascade
- **Devin**: https://docs.devin.ai/cli/subagents
- **OpenHands**: https://docs.openhands.dev/sdk/guides/agent-delegation, PR #712