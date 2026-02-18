# Parallel agent process groups not cleaned up on Ctrl+C

## Problem

When Ralph runs multiple agents concurrently (`run_group_with_workspaces`),
each agent is spawned in its own process group and waited on inside a
`tokio::select!` that races against `ctrl_c()`. On Ctrl+C, the first
`invoke_agent` to observe the signal kills its child's process group and
calls `std::process::exit(130)`, which terminates Ralph immediately. The
other concurrent agents' process groups are never signaled, leaving their
child processes (claude, rust-analyzer, cargo, etc.) as orphans.

## Scope

This only affects the parallel execution path (groups with 2+ independent
tasks on retry). Singleton execution (first attempts, or groups of one)
is handled correctly.

## Possible approaches

- **Track all active process groups centrally.** Maintain a shared
  `Vec<u32>` (or similar) of live child PGIDs. Install a single
  top-level `ctrl_c` handler that iterates and kills all of them before
  exiting. Remove PGIDs as agents complete normally.

- **Use a `CancellationToken`.** Have the Ctrl+C handler set a token
  instead of exiting. Each `invoke_agent` observes the token, kills its
  own process group, and returns an error. The orchestrator collects
  all results and exits cleanly.
