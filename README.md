# Ralph

AI agent orchestration for software engineering tasks.

> [!WARNING]
> Work in progress. Expect frequent breaking changes to the CLI, database schema, and agent protocol.

Ralph decomposes a natural-language request into dependency-ordered tasks,
then drives each task through an implement → test → review loop using
AI coding agents. It handles retries with failure feedback, version control
checkpointing (via [Jujutsu](https://jj-vcs.github.io/jj/latest/)), cost
tracking, and automated nit triage.

Supported backends: **Claude Code**, **Codex**, **Gemini CLI**, and
**OpenCode** (any provider/model pair). Each role in the pipeline can use a
different backend — set the model string in config and Ralph routes
accordingly.

## How it works

```
ralph plan "add pagination to the /users endpoint"
ralph run
ralph status
```

1. **Plan** — a planner agent reads your codebase and breaks the request into
   concrete tasks with dependency edges.
2. **Run** — the orchestrator executes tasks serially. Each task cycles through
   Implementing → Testing → Reviewing → Done. Failures feed context back to
   the implementer for retry (up to a configurable limit).
3. **Checkpoint** — after each task, Ralph commits progress with `jj`.

## Features

- **Dependency-aware scheduling** — tasks run in topological order; blocked
  tasks wait automatically.
- **Feedback loop** — test/review failures are captured and injected into
  the next implementation attempt so the agent doesn't rediscover the problem
  from scratch.
- **Process management** — agents run in process groups with hard timeouts,
  idle detection, and stuck-process detection. SIGINT/SIGTERM cleans up
  everything.
- **Cost tracking** — accumulated spend is logged each iteration; an optional
  budget cap stops execution when exceeded.
- **Nit triage** — after the final review, a triager agent promotes actionable
  nits to new tasks and dismisses the rest.
- **Manual overrides** — `skip`, `fail`, `reset`, and `hint` let you steer
  execution without editing internals.
- **SQLite state** — all state lives in `.ralph/ralph.db` (WAL mode).
  Inspect with `ralph dump`; migrate legacy flat files with `ralph import`.

## CLI

```
ralph init                        # create .ralph/ with default config
ralph plan <description>          # decompose request into tasks
ralph plan --spec <file>          # read request from a file
ralph plan --stdin                # read request from stdin
ralph run                         # execute the orchestration loop
ralph status                      # show task progress
ralph skip|fail|reset <task_id>   # manually override task state
ralph hint <task_id> <text>       # add guidance for implementer
ralph unhint <task_id>            # clear guidance for a task
ralph archive --done              # archive completed tasks
ralph nits                        # show open nits
ralph nits promote|dismiss <id>   # promote nit to task or dismiss
ralph nits triage                 # run triager agent on open nits
ralph dump [--json]               # inspect all tasks and state
```

## Configuration

`.ralph/config.toml` — all fields optional with sensible defaults:

```toml
[models]
planner = "opus"
implementer = "sonnet"
tester = "haiku"
reviewer = "opus"
triager = "opus"

max_attempts = 3
agent_timeout_secs = 1800
agent_idle_timeout_secs = 180
max_cost_usd = 10.0
escalation_after = 2
escalation_model = "opus"
auto_triage = true
```

## Building

Single binary, Rust 2024 edition:

```
cargo build
cargo test
```
