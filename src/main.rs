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
    Status,
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
        Command::Status => cmd_status().await,
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

async fn cmd_plan(
    description: Option<String>,
    spec: Option<PathBuf>,
    stdin: bool,
) -> Result<()> {
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
        anyhow::bail!(
            "Provide a description, --spec <file>, or --stdin"
        );
    };

    let config = config::Config::load().await?;
    let ralph_dir = PathBuf::from(".ralph");
    tokio::fs::create_dir_all(&ralph_dir).await?;

    let tasks_path = ralph_dir.join("tasks.jsonl");
    let result = agent::invoke_agent(
        agent::AgentRole::Planner,
        &agent::AgentContext::plan(&input),
        &config,
    )
    .await?;

    // The planner writes tasks.jsonl directly via claude's
    // file access. But we also extract any JSONL from the
    // result as a fallback.
    if !tasks_path.exists()
        && let Some(jsonl) = result.extract_jsonl() {
            tokio::fs::write(&tasks_path, jsonl).await?;
        }

    // Validate the output
    let tasks = task::load_tasks(&tasks_path).await?;
    eprintln!("Planned {} tasks → {}", tasks.len(), tasks_path.display());
    for t in &tasks {
        eprintln!("  [{}] {} (pri={})", t.id, t.title, t.priority);
    }
    Ok(())
}

async fn cmd_run(
    tasks_path: Option<PathBuf>,
    max_iterations: usize,
) -> Result<()> {
    let tasks_path =
        tasks_path.unwrap_or_else(|| PathBuf::from(".ralph/tasks.jsonl"));
    let config = config::Config::load().await?;
    orchestrator::run_loop(&tasks_path, max_iterations, &config).await
}

async fn cmd_status() -> Result<()> {
    let state_path = PathBuf::from(".ralph/state.json");
    let tasks_path = PathBuf::from(".ralph/tasks.jsonl");

    if !tasks_path.exists() {
        eprintln!("No tasks found. Run `ralph plan` first.");
        return Ok(());
    }

    let tasks = task::load_tasks(&tasks_path).await?;
    let exec_state = state::ExecutionState::load(&state_path).await?;

    eprintln!("Tasks: {}", tasks.len());
    for t in &tasks {
        let phase = exec_state
            .tasks
            .get(&t.id)
            .map(|e| format!("{:?} (attempts: {})", e.phase, e.attempts))
            .unwrap_or_else(|| "Pending".to_string());
        eprintln!("  [{}] {} — {}", t.id, t.title, phase);
    }
    Ok(())
}
