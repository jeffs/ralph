mod agent;
mod config;
mod orchestrator;
mod scheduler;
mod state;
mod task;

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "ralph", about = "AI agent orchestration loop")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Decompose a request into tasks via planner agent
    Plan {
        /// Inline description
        description: Option<String>,
        /// Read description from a file
        #[arg(long)]
        spec: Option<PathBuf>,
        /// Read description from stdin
        #[arg(long)]
        stdin: bool,
    },
    /// Run the orchestration loop on tasks
    Run {
        /// Path to task file (default: .ralph/tasks.jsonl)
        #[arg(long)]
        tasks: Option<PathBuf>,
        /// Max iterations before stopping
        #[arg(long, default_value_t = 50)]
        max_iterations: usize,
    },
    /// Initialize .ralph/ in current directory
    Init,
    /// Show current execution state
    Status {
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Mark a task as Done without running it
    Skip {
        /// Task ID to skip (e.g. "T3")
        task_id: String,
    },
    /// Mark a task as Failed immediately
    Fail {
        /// Task ID to fail (e.g. "T3")
        task_id: String,
    },
    /// Reset a task to Pending (clear attempts)
    Reset {
        /// Task ID to reset (e.g. "T3")
        task_id: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Init => cmd_init().await,
        Command::Plan {
            description,
            spec,
            stdin,
        } => cmd_plan(description, spec, stdin).await,
        Command::Run {
            tasks,
            max_iterations,
        } => cmd_run(tasks, max_iterations).await,
        Command::Status { json } => cmd_status(json).await,
        Command::Skip { task_id } => cmd_override_task(&task_id, "skip").await,
        Command::Fail { task_id } => cmd_override_task(&task_id, "fail").await,
        Command::Reset { task_id } => cmd_override_task(&task_id, "reset").await,
    }
}

async fn cmd_init() -> Result<()> {
    let ralph_dir = PathBuf::from(".ralph");
    tokio::fs::create_dir_all(&ralph_dir).await?;

    let gitignore = ralph_dir.join(".gitignore");
    if !gitignore.exists() {
        tokio::fs::write(&gitignore, "*\n").await?;
    }

    let config_path = ralph_dir.join("config.toml");
    if !config_path.exists() {
        let default = config::Config::default();
        let toml_str = toml::to_string_pretty(&default)?;
        tokio::fs::write(&config_path, toml_str).await?;
    }

    eprintln!("Initialized .ralph/");
    Ok(())
}

async fn cmd_plan(description: Option<String>, spec: Option<PathBuf>, stdin: bool) -> Result<()> {
    let input = if stdin {
        use tokio::io::AsyncReadExt;
        let mut buf = String::new();
        tokio::io::stdin().read_to_string(&mut buf).await?;
        buf
    } else if let Some(path) = spec {
        tokio::fs::read_to_string(&path).await?
    } else if let Some(desc) = description {
        desc
    } else {
        anyhow::bail!("Provide a description, --spec <file>, or --stdin");
    };

    let config = config::Config::load().await?;
    let ralph_dir = PathBuf::from(".ralph");
    tokio::fs::create_dir_all(&ralph_dir).await?;

    let registry = agent::ProcessRegistry::new(config.kill_grace_secs);
    orchestrator::spawn_signal_handler(registry.clone());

    let tasks_path = ralph_dir.join("tasks.jsonl");
    let result = agent::invoke_agent(
        agent::AgentRole::Planner,
        &agent::AgentContext::plan(&input),
        &config,
        None,
        &registry,
    )
    .await?;

    if registry.is_shutdown() {
        eprintln!("[ralph] shutdown requested, aborting plan.");
        return Ok(());
    }

    // The planner writes tasks.jsonl directly via claude's
    // file access. But we also extract any JSONL from the
    // result as a fallback.
    if !tasks_path.exists()
        && let Some(jsonl) = result.extract_jsonl()
    {
        tokio::fs::write(&tasks_path, jsonl).await?;
    }

    // Validate the output
    let tasks = task::load_tasks(&tasks_path).await?;
    task::validate_deps(&tasks)?;
    eprintln!("Planned {} tasks → {}", tasks.len(), tasks_path.display());
    for t in &tasks {
        eprintln!("  [{}] {} (pri={})", t.id, t.title, t.priority);
    }
    Ok(())
}

async fn cmd_run(tasks_path: Option<PathBuf>, max_iterations: usize) -> Result<()> {
    let tasks_path = tasks_path.unwrap_or_else(|| PathBuf::from(".ralph/tasks.jsonl"));
    let config = config::Config::load().await?;
    orchestrator::run_loop(&tasks_path, max_iterations, &config).await
}

async fn cmd_override_task(task_id: &str, action: &str) -> Result<()> {
    let tasks_path = PathBuf::from(".ralph/tasks.jsonl");
    let state_path = PathBuf::from(".ralph/state.json");

    if !tasks_path.exists() {
        anyhow::bail!("No tasks found. Run `ralph plan` first.");
    }

    let tasks = task::load_tasks(&tasks_path).await?;
    if !tasks.iter().any(|t| t.id == task_id) {
        let valid_ids: Vec<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
        anyhow::bail!(
            "Unknown task ID '{}'. Valid IDs: {}",
            task_id,
            valid_ids.join(", ")
        );
    }

    let mut exec_state = state::ExecutionState::load(&state_path).await?;
    let exec = exec_state.entry(task_id);

    match action {
        "skip" => {
            exec.phase = state::Phase::Done;
            eprintln!("Marked {} as Done (skipped)", task_id);
        }
        "fail" => {
            exec.phase = state::Phase::Failed;
            exec.last_error = Some("manually failed via `ralph fail`".to_string());
            eprintln!("Marked {} as Failed", task_id);
        }
        "reset" => {
            exec.phase = state::Phase::Pending;
            exec.attempts = 0;
            exec.last_error = None;
            exec.feedback.clear();
            eprintln!("Reset {} to Pending (attempts cleared)", task_id);
        }
        _ => unreachable!(),
    }

    exec_state.save(&state_path).await?;
    Ok(())
}

async fn cmd_status(json: bool) -> Result<()> {
    let state_path = PathBuf::from(".ralph/state.json");
    let tasks_path = PathBuf::from(".ralph/tasks.jsonl");

    if !tasks_path.exists() {
        if json {
            println!("{{\"tasks\":[]}}");
        } else {
            println!("No tasks found. Run `ralph plan` first.");
        }
        return Ok(());
    }

    let tasks = task::load_tasks(&tasks_path).await?;
    let exec_state = state::ExecutionState::load(&state_path).await?;

    if json {
        return cmd_status_json(&tasks, &exec_state);
    }

    let now = state::unix_now();

    let mut done = 0u32;
    let mut failed = 0u32;
    let mut in_progress = 0u32;
    let mut pending = 0u32;

    println!("Tasks: {}", tasks.len());
    for t in &tasks {
        let info = if let Some(e) = exec_state.tasks.get(&t.id) {
            match e.phase {
                state::Phase::Done => done += 1,
                state::Phase::Failed => failed += 1,
                state::Phase::Pending => pending += 1,
                _ => in_progress += 1,
            }
            let duration = match (e.started_at, e.completed_at) {
                (Some(s), Some(c)) => format!(" ({}s)", c.saturating_sub(s)),
                (Some(s), None) => format!(" ({}s elapsed)", now.saturating_sub(s)),
                _ => String::new(),
            };
            let error = e
                .last_error
                .as_deref()
                .map(|e| {
                    let truncated = if e.len() > 80 { &e[..80] } else { e };
                    format!(" err={truncated}")
                })
                .unwrap_or_default();
            format!(
                "{:?} attempts={}{duration}{error}",
                e.phase, e.attempts
            )
        } else {
            pending += 1;
            "Pending".to_string()
        };
        println!("  [{}] {} — {}", t.id, t.title, info);
    }
    println!(
        "Summary: {} done, {} failed, {} in-progress, {} pending",
        done, failed, in_progress, pending
    );
    Ok(())
}

fn cmd_status_json(tasks: &[task::Task], exec_state: &state::ExecutionState) -> Result<()> {
    #[derive(serde::Serialize)]
    struct StatusOutput<'a> {
        tasks: Vec<TaskStatus<'a>>,
    }
    #[derive(serde::Serialize)]
    struct TaskStatus<'a> {
        id: &'a str,
        title: &'a str,
        description: &'a str,
        priority: u32,
        blocked_by: &'a [String],
        phase: &'a state::Phase,
        attempts: u32,
        last_error: Option<&'a str>,
        files_changed: &'a [std::path::PathBuf],
        started_at: Option<u64>,
        completed_at: Option<u64>,
    }
    let default_exec = state::TaskExecution::default();
    let out = StatusOutput {
        tasks: tasks
            .iter()
            .map(|t| {
                let e = exec_state.tasks.get(&t.id).unwrap_or(&default_exec);
                TaskStatus {
                    id: &t.id,
                    title: &t.title,
                    description: &t.description,
                    priority: t.priority,
                    blocked_by: &t.blocked_by,
                    phase: &e.phase,
                    attempts: e.attempts,
                    last_error: e.last_error.as_deref(),
                    files_changed: &e.files_changed,
                    started_at: e.started_at,
                    completed_at: e.completed_at,
                }
            })
            .collect(),
    };
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}
