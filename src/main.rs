mod agent;
mod config;
mod nit;
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
    /// Skip a task (satisfies deps, shown as Skipped)
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
    /// Add persistent guidance for a task's implementer
    Hint {
        /// Task ID (e.g. "T3")
        task_id: String,
        /// Guidance text (accumulated across calls)
        text: String,
    },
    /// Clear all guidance for a task
    Unhint {
        /// Task ID (e.g. "T3")
        task_id: String,
    },
    /// Manage captured nits (improvement suggestions)
    Nits {
        /// Show all nits (including dismissed/promoted)
        #[arg(long)]
        all: bool,
        #[command(subcommand)]
        action: Option<NitsAction>,
    },
}

#[derive(Subcommand)]
enum NitsAction {
    /// Create a task from a nit
    Promote {
        /// Nit ID (e.g. "NIT-1")
        nit_id: String,
    },
    /// Mark a nit as dismissed
    Dismiss {
        /// Nit ID (e.g. "NIT-1")
        nit_id: String,
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
        Command::Hint { task_id, text } => cmd_hint(&task_id, &text).await,
        Command::Unhint { task_id } => cmd_unhint(&task_id).await,
        Command::Nits { all, action } => match action {
            None => cmd_nits_list(all).await,
            Some(NitsAction::Promote { nit_id }) => cmd_nits_promote(&nit_id).await,
            Some(NitsAction::Dismiss { nit_id }) => cmd_nits_dismiss(&nit_id).await,
        },
    }
}

const DEFAULT_PROMPTS: &[(&str, &str)] = &[
    ("planner.md", include_str!("../prompts/planner.md")),
    ("implementer.md", include_str!("../prompts/implementer.md")),
    ("tester.md", include_str!("../prompts/tester.md")),
    ("reviewer.md", include_str!("../prompts/reviewer.md")),
];

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

    let prompts_dir = PathBuf::from("prompts");
    tokio::fs::create_dir_all(&prompts_dir).await?;
    for (name, content) in DEFAULT_PROMPTS {
        let path = prompts_dir.join(name);
        if !path.exists() {
            tokio::fs::write(&path, content).await?;
        }
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
        0,
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

    let directive_action = match action {
        "skip" => state::DirectiveAction::Skip,
        "fail" => state::DirectiveAction::Fail,
        "reset" => state::DirectiveAction::Reset,
        _ => unreachable!(),
    };

    state::append_directive(&state::Directive {
        task_id: task_id.to_string(),
        action: directive_action,
    })
    .await?;

    eprintln!("Queued {action} for {task_id}");
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
    let mut exec_state = state::ExecutionState::load(&state_path).await?;

    // Merge pending directives into the in-memory view (don't save back).
    let pending_directives = state::load_directives().await.unwrap_or_default();
    let directive_count = pending_directives.len();
    if !pending_directives.is_empty() {
        let task_ids: Vec<String> = tasks.iter().map(|t| t.id.clone()).collect();
        exec_state.apply_directives(&pending_directives, &task_ids);
    }

    if json {
        return cmd_status_json(&tasks, &exec_state);
    }

    let now = state::unix_now();

    let mut done = 0u32;
    let mut failed = 0u32;
    let mut in_progress = 0u32;
    let mut pending = 0u32;
    let mut skipped = 0u32;

    println!("Tasks: {}", tasks.len());
    for t in &tasks {
        let info = if let Some(e) = exec_state.tasks.get(&t.id) {
            match e.phase {
                state::Phase::Done => done += 1,
                state::Phase::Failed => failed += 1,
                state::Phase::Skipped => skipped += 1,
                state::Phase::Pending => pending += 1,
                state::Phase::Implementing
                | state::Phase::Testing
                | state::Phase::Reviewing => in_progress += 1,
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
            format!("{:?} attempts={}{duration}{error}", e.phase, e.attempts)
        } else {
            pending += 1;
            "Pending".to_string()
        };
        println!("  [{}] {} — {}", t.id, t.title, info);
    }
    println!(
        "Summary: {} done, {} skipped, {} failed, {} in-progress, {} pending",
        done, skipped, failed, in_progress, pending
    );

    if directive_count > 0 {
        println!("Pending directives: {directive_count}");
    }

    let nits_path = PathBuf::from(".ralph/nits.jsonl");
    if let Ok(nits) = nit::load_nits(&nits_path).await {
        let open_count = nits
            .iter()
            .filter(|n| n.status == nit::NitStatus::Open)
            .count();
        if open_count > 0 {
            println!("Nits: {open_count} open (run `ralph nits` to see them)");
        }
    }
    Ok(())
}

fn cmd_status_json(tasks: &[task::Task], exec_state: &state::ExecutionState) -> Result<()> {
    #[derive(serde::Serialize)]
    struct TaskStatus<'a> {
        id: &'a str,
        title: &'a str,
        description: &'a str,
        priority: u32,
        blocked_by: &'a [String],
        phase: &'a state::Phase,
        phase_ordinal: u8,
        attempts: u32,
        last_error: Option<&'a str>,
        files_changed: &'a [std::path::PathBuf],
        started_at: Option<u64>,
        completed_at: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        postmortem: Option<&'a str>,
    }
    let default_exec = state::TaskExecution::default();
    let out: Vec<TaskStatus> = tasks
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
                phase_ordinal: e.phase.phase_ordinal(),
                attempts: e.attempts,
                last_error: e.last_error.as_deref(),
                files_changed: &e.files_changed,
                started_at: e.started_at,
                completed_at: e.completed_at,
                postmortem: e.postmortem.as_deref(),
            }
        })
        .collect();
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}

async fn cmd_hint(task_id: &str, text: &str) -> Result<()> {
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
    exec.guidance.push(text.to_string());
    exec_state.save(&state_path).await?;

    eprintln!(
        "Added guidance to {} ({} total)",
        task_id,
        exec_state.tasks[task_id].guidance.len()
    );
    Ok(())
}

async fn cmd_unhint(task_id: &str) -> Result<()> {
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
    let count = exec.guidance.len();
    exec.guidance.clear();
    exec_state.save(&state_path).await?;

    eprintln!("Cleared {count} guidance entries from {task_id}");
    Ok(())
}

async fn cmd_nits_list(all: bool) -> Result<()> {
    let path = PathBuf::from(".ralph/nits.jsonl");
    let nits = nit::load_nits(&path).await?;

    let open = nits
        .iter()
        .filter(|n| n.status == nit::NitStatus::Open)
        .count();
    let dismissed = nits
        .iter()
        .filter(|n| n.status == nit::NitStatus::Dismissed)
        .count();
    let promoted = nits
        .iter()
        .filter(|n| n.status == nit::NitStatus::Promoted)
        .count();

    println!("Nits: {open} open, {dismissed} dismissed, {promoted} promoted");

    for n in &nits {
        if !all && n.status != nit::NitStatus::Open {
            continue;
        }
        let content_preview = if !n.summary.is_empty() {
            n.summary.clone()
        } else {
            let first_line = n.content.lines().next().unwrap_or(&n.content);
            nit::truncate_with_ellipsis(first_line, 60)
        };
        let status_tag = if all && n.status != nit::NitStatus::Open {
            match n.status {
                nit::NitStatus::Dismissed => " [dismissed]",
                nit::NitStatus::Promoted => " [promoted]",
                nit::NitStatus::Open => "",
            }
        } else {
            ""
        };
        println!(
            "  [{}] {} ({}) — {content_preview}{status_tag}",
            n.id, n.source_task, n.source_role
        );
    }
    Ok(())
}

async fn cmd_nits_promote(nit_id: &str) -> Result<()> {
    let nits_path = PathBuf::from(".ralph/nits.jsonl");
    let mut nits = nit::load_nits(&nits_path).await?;

    let nit_entry = nits
        .iter_mut()
        .find(|n| n.id == nit_id)
        .ok_or_else(|| anyhow::anyhow!("nit '{nit_id}' not found"))?;

    if nit_entry.status != nit::NitStatus::Open {
        let status_name = match nit_entry.status {
            nit::NitStatus::Promoted => "promoted",
            nit::NitStatus::Dismissed => "dismissed",
            nit::NitStatus::Open => "open",
        };
        anyhow::bail!("nit '{nit_id}' is already {status_name}");
    }

    let tasks_path = PathBuf::from(".ralph/tasks.jsonl");
    let tasks = if tasks_path.exists() {
        task::load_tasks(&tasks_path).await?
    } else {
        Vec::new()
    };

    let max_priority = tasks.iter().map(|t| t.priority).max().unwrap_or(0);
    let task_id = nit_id.replace('-', "");

    if tasks.iter().any(|t| t.id == task_id) {
        anyhow::bail!("task ID '{task_id}' already exists");
    }

    let title = if nit_entry.summary.is_empty() {
        let first_line = nit_entry
            .content
            .lines()
            .next()
            .unwrap_or(&nit_entry.content);
        nit::truncate_with_ellipsis(first_line, 60)
    } else {
        nit_entry.summary.clone()
    };
    let new_task = task::Task {
        id: task_id.clone(),
        title,
        description: nit_entry.content.clone(),
        priority: max_priority + 1,
        blocked_by: vec![],
    };

    // Append to tasks.jsonl
    use tokio::io::AsyncWriteExt;
    let mut line = serde_json::to_string(&new_task)?;
    line.push('\n');
    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&tasks_path)
        .await?;
    file.write_all(line.as_bytes()).await?;

    nit_entry.status = nit::NitStatus::Promoted;
    nit_entry.promoted_to = Some(task_id.clone());
    nit::save_nits(&nits_path, &nits).await?;

    eprintln!("Promoted {nit_id} → task {task_id}");
    Ok(())
}

async fn cmd_nits_dismiss(nit_id: &str) -> Result<()> {
    let nits_path = PathBuf::from(".ralph/nits.jsonl");
    let mut nits = nit::load_nits(&nits_path).await?;

    let nit_entry = nits
        .iter_mut()
        .find(|n| n.id == nit_id)
        .ok_or_else(|| anyhow::anyhow!("nit '{nit_id}' not found"))?;

    if nit_entry.status != nit::NitStatus::Open {
        let status_name = match nit_entry.status {
            nit::NitStatus::Promoted => "promoted",
            nit::NitStatus::Dismissed => "dismissed",
            nit::NitStatus::Open => "open",
        };
        anyhow::bail!("nit '{nit_id}' is already {status_name}");
    }

    nit_entry.status = nit::NitStatus::Dismissed;
    nit::save_nits(&nits_path, &nits).await?;

    eprintln!("Dismissed {nit_id}");
    Ok(())
}
