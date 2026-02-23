# Ralph

AI agent orchestration system. Single binary crate, Rust 2024 edition.

## Build and test

    cargo build
    cargo test          # 158 unit tests, no external deps

## Structure

src/main.rs — CLI (clap derive): plan, run, status, skip/fail/reset, hint, archive, nits
src/task.rs — Task model, JSONL parsing, validation, archive/restore
src/state.rs — Execution state (phase, attempts, feedback), atomic save
src/config.rs — TOML config loading, defaults
src/agent.rs — Agent invocation, process groups, status parsing
src/scheduler.rs — Dependency resolution, parallel partitioning
src/orchestrator.rs — Main loop, workspace management, checkpointing
prompts/ — Prompt templates with {{PLACEHOLDER}} substitution

## Key details

- Agents are spawned via `claude -p <prompt> --output-format json`
- VCS is Jujutsu (`jj`), never git — Ralph handles all commits
- Runtime state in `.ralph/` (gitignored); task file is `.ralph/tasks.jsonl`
- See `doc/overview.md` for full architecture and agent protocol
