# Ralph: AI Agent Orchestration System

Ralph automates multi-step software engineering tasks by decomposing them
into concrete work items and executing them through specialized AI agents.
It manages the full lifecycle: planning, implementation, testing, review,
retry on failure, and version control checkpointing.

This document is intended for developers and AI agents who will work on
projects orchestrated by Ralph.

## How it works in one paragraph

You give Ralph a natural-language request (or a spec file). Ralph invokes
a **Planner** agent to decompose it into a dependency-ordered task list.
Then `ralph run` iterates: for each ready task it spawns an **Implementer**
agent to write code, a **Tester** agent to run the test suite, and a
**Reviewer** agent to check correctness. If testing or review fails, the
failure feedback is forwarded to the implementer and the task retries (up
to a configurable limit). When all tasks reach Done, a final review runs.
Ralph commits progress to Jujutsu (`jj`) after each group of tasks.

## Architecture

```
                     ┌────────────┐
                     │ ralph plan │
                     └─────┬──────┘
                           │  writes
                           ▼
                  .ralph/tasks.jsonl
                           │
                     ┌─────┴──────┐
                     │ ralph run  │
                     └─────┬──────┘
                           │
              ┌────────────┼────────────┐
              ▼            ▼            ▼
         Implementer    Tester      Reviewer
         (claude -p)  (claude -p)  (claude -p)
              │            │            │
              └────────────┼────────────┘
                           │
                     jj checkpoint
```

### Modules

| Module          | Purpose                                                |
|-----------------|--------------------------------------------------------|
| `main.rs`       | CLI: `init`, `plan`, `run`, `status`, `skip/fail/reset`, `archive/restore`|
| `task.rs`       | Task model, JSONL parsing, validation                  |
| `state.rs`      | Per-task execution state (phase, attempts, feedback)   |
| `config.rs`     | TOML configuration loading and defaults                |
| `agent.rs`      | Agent invocation, process management, status parsing   |
| `scheduler.rs`  | Dependency resolution, parallel partitioning           |
| `orchestrator.rs`| Main loop, workspace management, checkpointing        |

## Task format

Tasks live in `.ralph/tasks.jsonl`, one JSON object per line. The Planner
agent writes this file; Ralph never modifies it.

```json
{"id":"T1","title":"Add foo function","description":"Create foo() in src/lib.rs ...","priority":1,"blocked_by":[]}
{"id":"T2","title":"Add tests for foo","description":"...","priority":2,"blocked_by":["T1"]}
```

| Field        | Type     | Required | Notes                                |
|-------------|----------|----------|--------------------------------------|
| `id`        | string   | yes      | Unique, no whitespace (e.g. "AUTH-1")|
| `title`     | string   | yes      | Imperative one-liner                 |
| `description`| string  | no       | What to change, where, and why       |
| `priority`  | integer  | yes      | 1 = highest                          |
| `blocked_by`| string[] | no       | IDs that must complete first         |

All `blocked_by` references must point to an `id` in the same file or
in the archive (`archive.jsonl`). Duplicate IDs, empty IDs, and IDs
containing whitespace are rejected.

## Execution state

Execution metadata lives separately in `.ralph/state.json` so the task
file stays clean for human review. State is saved atomically (write to
`.tmp`, then rename). Crash recovery promotes `.json.tmp` if the main
file is missing.

Each task tracks:

- **phase**: `Pending` → `Implementing` → `Testing` → `Reviewing` → `Done` (or `Failed`)
- **attempts**: number of implementation cycles
- **files_changed**: paths modified by the implementer
- **feedback**: full agent output from failed test/review runs, forwarded
  to the implementer on retry (truncated to 8 KB)
- **timestamps**: `started_at`, `completed_at`, `phase_entered_at`
- **last_error**: most recent failure reason

## Agent roles

Ralph spawns agents by invoking `claude -p <prompt> --output-format json`.
Each role has a prompt template in `prompts/` with `{{PLACEHOLDER}}`
variables that Ralph fills in.

### Planner (`prompts/planner.md`)

- **Input**: the user's request text
- **Job**: read the codebase, decompose the request into tasks
- **Output**: write `.ralph/tasks.jsonl`

### Implementer (`prompts/implementer.md`)

- **Input**: one task (id, title, description) + optional feedback from
  prior failed attempts
- **Job**: make minimal code changes to complete the task
- **Constraints**: must not commit (`jj commit`/`jj new`), must not `cd`.
  Should run tests to validate before declaring success.

### Tester (`prompts/tester.md`)

- **Input**: task id + list of files changed
- **Job**: discover and run the project's test suite, report results
- **Constraints**: must not modify code

### Reviewer (`prompts/reviewer.md`)

- **Input**: task details + diff
- **Job**: verify correctness, conventions, security
- **Constraints**: must not modify code

### Status protocol

Every agent must end its response with exactly one status line. Ralph
parses the **last** `STATUS:` line in the output:

```
STATUS: SUCCESS
STATUS: FAILURE: <reason>
STATUS: NEEDS_RETRY: <reason>
STATUS: APPROVED_WITH_NITS: <suggestions>     (reviewer only)
```

The reason can be inline or on subsequent lines (Ralph collects both).
If no status line is found, Ralph treats it as `NEEDS_RETRY`.

## Task lifecycle

```
Pending ──► Implementing ──[implementer]──► Testing ──[tester]──► Reviewing ──[reviewer]──► Done
   ▲              │                            │                      │
   └──────────────┴────────────────────────────┴──────────────────────┘
                     (on failure, reset to Pending with feedback)
```

After `max_attempts` failures (default: 3), the task moves to `Failed`
instead of back to `Pending`.

## The main loop (`ralph run`)

Each iteration:

1. Load tasks and state
2. If all tasks are Done → run a **final review** of the entire diff
3. Resume any in-flight tasks (stuck at Testing or Reviewing)
4. Find Pending tasks whose dependencies are satisfied
5. Partition ready tasks into parallel groups (file-disjoint sets)
6. Execute each group:
   - **Singleton** (1 task): runs in the default working copy
   - **Multi-task**: each task gets its own `jj workspace`; on success,
     changes are squashed back into the default workspace
7. Run testers and reviewers for completed implementations
8. Commit progress with `jj commit`
9. Repeat until convergence, stagnation, or iteration cap

The loop detects **stagnation** (all remaining tasks have failed) and
**dependency deadlocks** (nothing is ready but nothing has failed).

## Parallelism

- **Implementers** run in parallel when their file sets are disjoint
  (based on files touched in prior attempts). First-attempt tasks run
  alone to establish their file footprint.
- **Testers** run in parallel when their file sets are disjoint.
- **Reviewers** always run in parallel (read-only).

Multi-task parallel groups use jj workspace isolation:

```
.ralph/ws-T1/     ← jj workspace "ralph-T1"
.ralph/ws-T2/     ← jj workspace "ralph-T2"
```

Shared files (e.g. `Cargo.lock`) are symlinked from the project root.
Each workspace can optionally get its own `CARGO_TARGET_DIR` to avoid
cargo lock contention (`workspace.isolate_target_dir = true`, the default).

## Version control integration (Jujutsu)

Ralph uses **Jujutsu (`jj`)** exclusively — never Git commands.

- **Agents must never run** `jj commit`, `jj new`, or any state-changing
  jj command. Ralph handles all version control.
- On startup, Ralph isolates any pre-existing dirty files with `jj new`.
- After each task group, Ralph commits with a descriptive message like
  `ralph: T1 (done), T2 (testing)`.
- Stale workspaces from interrupted runs are cleaned up automatically.
- File attribution uses `jj diff --summary` to track which files each
  task modified.

## Configuration

`.ralph/config.toml`:

```toml
# Per-role model configuration (all roles required)
[models]
planner = "opus"
implementer = "sonnet"
tester = "sonnet"
reviewer = "opus"
triager = "opus"

# Max retries per task before marking Failed
max_attempts = 3

# Agent timeouts (seconds)
agent_timeout_secs = 1800        # 30 min hard ceiling
agent_idle_timeout_secs = 180    # kill if CPU < 1% for this long

# Grace period between SIGTERM and SIGKILL
kill_grace_secs = 5

# Budget limit (stop if exceeded)
max_cost_usd = 10.0

# Automatically triage open nits after final review
auto_triage = true
max_triage_rounds = 3

# Directory containing prompt templates
prompts_dir = "prompts"

# Workspace settings
[workspace]
shared = ["Cargo.lock"]           # Symlinked into per-task workspaces
isolate_target_dir = true         # Separate CARGO_TARGET_DIR per workspace

# Environment variables forwarded to agents
[env]
passthrough = ["MY_TOKEN"]

[env.set]
CUSTOM_VAR = "value"
```

All fields have sensible defaults. A bare `ralph init` creates a working
configuration.

## Process management

Ralph runs agents in their own **process groups** so child processes
(rust-analyzer, cargo, etc.) are cleaned up together:

- **Hard timeout**: kills the group after `agent_timeout_secs`
- **Idle detection**: samples CPU every 30s; kills after cumulative idle
  time exceeds `agent_idle_timeout_secs`
- **Stuck detection**: watches stderr for "waiting for file lock" patterns;
  kills after 60s grace
- **Signal handling**: SIGINT/SIGTERM kills all registered process groups,
  saves state, and exits cleanly
- **Orphan audit**: each iteration checks for leftover process groups from
  prior runs and kills them

## CLI reference

```
ralph init                    # Create .ralph/ with default config
ralph plan <description>      # Decompose request into tasks
ralph plan --spec <file>      # Read request from a file
ralph plan --stdin            # Read request from stdin
ralph run                     # Execute the orchestration loop
ralph run --max-iterations 20 # Limit iteration count
ralph status                  # Show task progress
ralph skip <task_id>          # Mark a task Done (skip it)
ralph fail <task_id>          # Mark a task Failed
ralph reset <task_id>         # Reset a task to Pending
ralph archive <task_id>       # Move a terminal task to archive.jsonl
ralph archive --done          # Archive all Done + Skipped tasks
ralph restore <task_id>       # Restore a task from archive.jsonl
```

### Automated nit triage

After the final review passes, Ralph automatically triages any open
nits by invoking a **Triager** agent. The triager reads all open nits
and the current task summary, then emits a promote/dismiss decision
for each. Promoted nits become new tasks and the main loop continues;
dismissed nits are marked as such. This repeats up to
`max_triage_rounds` times (default: 3).

Disable with `auto_triage = false` in config.

```
```

## Rules for agents working under Ralph

If you are an AI agent being orchestrated by Ralph, follow these rules:

1. **Do not touch version control.** Never run `jj commit`, `jj new`,
   `jj squash`, or any state-changing jj command. Ralph handles all VCS.
2. **Do not change directories.** Stay in the working directory Ralph
   gives you. Ralph manages workspace paths.
3. **Do not modify `.ralph/tasks.jsonl`.** This is the planner's output.
   Ralph reads it but never writes to it after planning.
4. **Read before writing.** Always read relevant source files and
   CLAUDE.md before making changes. Understand existing patterns.
5. **Minimal changes only.** Do what the task says. No unrelated
   refactoring, no extra features, no gratuitous cleanup.
6. **Run tests.** If the project has a test command, run it to validate
   your changes before declaring success.
7. **End with a status line.** Your response must end with exactly one
   `STATUS:` line (see the status protocol above).
8. **Use feedback.** If you're retrying a task, read the "Previous
   Attempt Feedback" section carefully and address the issues it raises.

## Feedback loop

When an implementer's work fails testing or review, Ralph captures the
full agent output and stores it as feedback. On the next attempt, the
feedback is injected into the implementer's prompt under a
"Previous Attempt Feedback" heading. This gives the implementer
actionable context about what went wrong, without requiring it to
re-discover the problem from scratch. Feedback is truncated to 8 KB.

## Cost tracking

The `claude` CLI reports `total_cost_usd` in its JSON output. Ralph
accumulates this across all agent invocations and logs it each iteration.
If `max_cost_usd` is configured, Ralph stops execution when the budget
is exceeded.

## Project integration

To signal that a project is orchestrated by Ralph, add a section to the
project's `CLAUDE.md`:

```markdown
## Ralph Integration

This project is orchestrated by Ralph. Agents should:

- **Not** run `jj commit`, `jj new`, or any jj state-changing commands.
  Ralph handles all version control.
- **Not** `cd` to other directories. Ralph manages the working directory.
- Read this file and any spec before starting any task.
```

This ensures agents spawned by Ralph (or manually invoked in the same
repo) respect the orchestration protocol.

## Files at a glance

```
.ralph/
  config.toml       # Orchestration settings
  tasks.jsonl       # Task definitions (planner output, human-editable)
  archive.jsonl     # Archived terminal tasks (same format as tasks.jsonl)
  state.json        # Execution state (managed by Ralph, not hand-edited)
  ws-<task_id>/     # Temporary jj workspaces (auto-cleaned)
  .gitignore        # Contains "*" — nothing in .ralph/ is committed
```
