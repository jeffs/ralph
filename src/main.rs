mod agent;
mod config;
mod db;
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
        task_id: Option<String>,
        /// Reset all failed (unarchived) tasks
        #[arg(long)]
        failed: bool,
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
    /// Move terminal tasks to archive
    Archive {
        /// Task ID to archive (omit for bulk with --done)
        task_id: Option<String>,
        /// Archive all Done + Skipped tasks
        #[arg(long)]
        done: bool,
    },
    /// Restore an archived task back to active
    Restore {
        /// Task ID to restore
        task_id: String,
    },
    /// Dump full project state (tasks, directives, nits)
    Dump {
        /// Output as JSON
        #[arg(long)]
        json: bool,
        /// Include archived tasks
        #[arg(long)]
        all: bool,
    },
    /// Import flat files (.ralph/) into the SQLite database
    Import {
        /// Source directory containing tasks.jsonl, state.json, etc.
        #[arg(long, default_value = ".ralph")]
        dir: PathBuf,
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
    /// Run the triager agent on all open nits
    Triage,
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
        Command::Reset { task_id, failed } => cmd_reset(task_id, failed).await,
        Command::Hint { task_id, text } => cmd_hint(&task_id, &text).await,
        Command::Unhint { task_id } => cmd_unhint(&task_id).await,
        Command::Archive { task_id, done } => cmd_archive(task_id, done).await,
        Command::Restore { task_id } => cmd_restore(&task_id).await,
        Command::Dump { json, all } => cmd_dump(json, all).await,
        Command::Import { dir } => cmd_import(dir).await,
        Command::Nits { all, action } => match action {
            None => cmd_nits_list(all).await,
            Some(NitsAction::Promote { nit_id }) => cmd_nits_promote(&nit_id).await,
            Some(NitsAction::Dismiss { nit_id }) => cmd_nits_dismiss(&nit_id).await,
            Some(NitsAction::Triage) => cmd_nits_triage().await,
        },
    }
}

const DEFAULT_PROMPTS: &[(&str, &str)] = &[
    ("planner.md", include_str!("../prompts/planner.md")),
    ("implementer.md", include_str!("../prompts/implementer.md")),
    ("tester.md", include_str!("../prompts/tester.md")),
    ("reviewer.md", include_str!("../prompts/reviewer.md")),
    ("triager.md", include_str!("../prompts/triager.md")),
];

/// Check for legacy flat files without a database, and bail with a
/// migration message if found.
fn check_legacy_files() -> Result<()> {
    let db_path = PathBuf::from(".ralph/ralph.db");
    let legacy_path = PathBuf::from(".ralph/tasks.jsonl");
    if !db_path.exists() && legacy_path.exists() {
        anyhow::bail!(
            "Found legacy flat files but no database. Run `ralph import` to migrate."
        );
    }
    Ok(())
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

    let prompts_dir = PathBuf::from("prompts");
    tokio::fs::create_dir_all(&prompts_dir).await?;
    for (name, content) in DEFAULT_PROMPTS {
        let path = prompts_dir.join(name);
        if !path.exists() {
            tokio::fs::write(&path, content).await?;
        }
    }

    db::open(&db::db_path())?;

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

    let conn = db::open(&db::db_path())?;

    // Get all in-use IDs (active + archived) for collision detection.
    let taken_ids = db::task_ids_in_use(&conn)?;
    // Get summary of existing ID ranges for the planner prompt.
    let existing_ids_summary = db::id_ranges_summary(&conn)?;
    // Count active tasks for the post-plan log message.
    let existing_active_count = db::list_active_tasks(&conn)?.len();

    let result = agent::invoke_agent(
        agent::AgentRole::Planner,
        &agent::AgentContext::plan(&input, &existing_ids_summary),
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

    // Extract new tasks from the planner's stdout.
    let jsonl = result
        .extract_jsonl()
        .ok_or_else(|| anyhow::anyhow!("planner produced no JSONL output"))?;
    let mut new_task_defs = task::parse_tasks(&jsonl)?;

    // Renumber any IDs that collide with existing tasks.
    let renames = task::renumber_collisions(&mut new_task_defs, &taken_ids);
    for (old, new) in &renames {
        eprintln!("[ralph] renumbered colliding ID: {old} → {new}");
    }

    // Validate deps: new tasks can reference each other or any known DB ID.
    let extra_ids: std::collections::HashSet<&str> =
        taken_ids.iter().map(|s| s.as_str()).collect();
    task::validate_deps(&new_task_defs, &extra_ids)?;

    // Convert TaskDef → Task and insert into the database.
    let new_tasks: Vec<task::Task> = new_task_defs.iter().map(task::Task::from_def).collect();
    let new_count = new_tasks.len();
    db::insert_tasks(&conn, &new_tasks)?;

    eprintln!(
        "Planned {} new task(s), {} total → {}",
        new_count,
        existing_active_count + new_count,
        db::db_path().display()
    );
    for t in &new_task_defs {
        eprintln!("  [{}] {} (pri={})", t.id, t.title, t.priority);
    }
    Ok(())
}

async fn cmd_run(tasks_path: Option<PathBuf>, max_iterations: usize) -> Result<()> {
    let _ = tasks_path; // tasks are now loaded from the database
    check_legacy_files()?;
    let config = config::Config::load().await?;
    std::fs::create_dir_all(".ralph")?;
    let conn = db::open(&db::db_path())?;
    orchestrator::run_loop(&conn, max_iterations, &config).await
}

async fn cmd_override_task(task_id: &str, action: &str) -> Result<()> {
    check_legacy_files()?;
    std::fs::create_dir_all(".ralph")?;
    let conn = db::open(&db::db_path())?;

    if db::get_task(&conn, task_id)?.is_none() {
        let tasks = db::list_active_tasks(&conn)?;
        let valid_ids: Vec<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
        anyhow::bail!(
            "Unknown task ID '{}'. Valid IDs: {}",
            task_id,
            valid_ids.join(", ")
        );
    }

    let directive_action = match action {
        "skip" => task::DirectiveAction::Skip,
        "fail" => task::DirectiveAction::Fail,
        "reset" => task::DirectiveAction::Reset,
        _ => unreachable!(),
    };

    db::insert_directive(&conn, task_id, directive_action)?;
    eprintln!("Queued {action} for {task_id}");
    Ok(())
}

async fn cmd_reset(task_id: Option<String>, failed: bool) -> Result<()> {
    match (task_id, failed) {
        (Some(id), false) => cmd_override_task(&id, "reset").await,
        (None, true) => {
            check_legacy_files()?;
            std::fs::create_dir_all(".ralph")?;
            let conn = db::open(&db::db_path())?;
            let tasks = db::list_all_tasks(&conn)?;
            let failed_ids: Vec<String> = tasks
                .iter()
                .filter(|t| !t.archived && matches!(t.phase, task::Phase::Failed))
                .map(|t| t.id.clone())
                .collect();
            if failed_ids.is_empty() {
                eprintln!("No failed tasks to reset.");
                return Ok(());
            }
            for id in &failed_ids {
                db::insert_directive(&conn, id, task::DirectiveAction::Reset)?;
            }
            eprintln!("Queued reset for {} task(s): {}", failed_ids.len(), failed_ids.join(", "));
            Ok(())
        }
        (Some(_), true) => anyhow::bail!("Provide either a task ID or --failed, not both"),
        (None, false) => anyhow::bail!("Provide a task ID or --failed"),
    }
}

async fn cmd_status(json: bool) -> Result<()> {
    check_legacy_files()?;
    let db_path = db::db_path();
    if !db_path.exists() {
        if json {
            println!("{{\"tasks\":[]}}");
        } else {
            println!("No tasks found. Run `ralph plan` first.");
        }
        return Ok(());
    }

    let conn = db::open(&db_path)?;
    let tasks = db::list_active_tasks(&conn)?;

    if json {
        return cmd_status_json(&tasks);
    }

    let now = task::unix_now();

    let mut done = 0u32;
    let mut failed = 0u32;
    let mut in_progress = 0u32;
    let mut pending = 0u32;
    let mut skipped = 0u32;

    println!("Tasks: {}", tasks.len());
    for t in &tasks {
        match t.phase {
            task::Phase::Done => done += 1,
            task::Phase::Failed => failed += 1,
            task::Phase::Skipped => skipped += 1,
            task::Phase::Pending => pending += 1,
            task::Phase::Implementing | task::Phase::Testing | task::Phase::Reviewing => {
                in_progress += 1
            }
        }
        let duration = match (t.started_at, t.completed_at) {
            (Some(s), Some(c)) => format!(" ({}s)", c.saturating_sub(s)),
            (Some(s), None) => format!(" ({}s elapsed)", now.saturating_sub(s)),
            _ => String::new(),
        };
        let error = t
            .last_error
            .as_deref()
            .map(|e| {
                let truncated = if e.len() > 80 { &e[..80] } else { e };
                format!(" err={truncated}")
            })
            .unwrap_or_default();
        let info = format!("{:?} attempts={}{duration}{error}", t.phase, t.attempts);
        println!("  [{}] {} — {}", t.id, t.title, info);
    }
    println!(
        "Summary: {} done, {} skipped, {} failed, {} in-progress, {} pending",
        done, skipped, failed, in_progress, pending
    );

    let directives = db::peek_directives(&conn)?;
    if !directives.is_empty() {
        println!("Pending directives: {}", directives.len());
    }

    let all_tasks = db::list_all_tasks(&conn)?;
    let archived_count = all_tasks.iter().filter(|t| t.archived).count();
    if archived_count > 0 {
        println!("Archived: {} task(s)", archived_count);
    }

    let open_nits = db::list_nits(&conn, false)?;
    if !open_nits.is_empty() {
        println!("Nits: {} open (run `ralph nits` to see them)", open_nits.len());
    }

    Ok(())
}

fn cmd_status_json(tasks: &[task::Task]) -> Result<()> {
    #[derive(serde::Serialize)]
    struct TaskStatus<'a> {
        id: &'a str,
        title: &'a str,
        description: &'a str,
        priority: u32,
        blocked_by: &'a [String],
        phase: &'a task::Phase,
        phase_ordinal: u8,
        attempts: u32,
        last_error: Option<&'a str>,
        files_changed: &'a [std::path::PathBuf],
        started_at: Option<u64>,
        completed_at: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        postmortem: Option<&'a str>,
    }
    let out: Vec<TaskStatus> = tasks
        .iter()
        .map(|t| TaskStatus {
            id: &t.id,
            title: &t.title,
            description: &t.description,
            priority: t.priority,
            blocked_by: &t.blocked_by,
            phase: &t.phase,
            phase_ordinal: t.phase.phase_ordinal(),
            attempts: t.attempts,
            last_error: t.last_error.as_deref(),
            files_changed: &t.files_changed,
            started_at: t.started_at,
            completed_at: t.completed_at,
            postmortem: t.postmortem.as_deref(),
        })
        .collect();
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}

async fn cmd_hint(task_id: &str, text: &str) -> Result<()> {
    check_legacy_files()?;
    std::fs::create_dir_all(".ralph")?;
    let conn = db::open(&db::db_path())?;

    let t = db::get_task(&conn, task_id)?;
    if t.is_none() {
        let tasks = db::list_active_tasks(&conn)?;
        let valid_ids: Vec<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
        anyhow::bail!(
            "Unknown task ID '{}'. Valid IDs: {}",
            task_id,
            valid_ids.join(", ")
        );
    }

    let mut guidance = t.unwrap().guidance;
    guidance.push(text.to_string());
    let count = guidance.len();
    db::set_guidance(&conn, task_id, &guidance)?;

    eprintln!("Added guidance to {} ({} total)", task_id, count);
    Ok(())
}

async fn cmd_unhint(task_id: &str) -> Result<()> {
    check_legacy_files()?;
    std::fs::create_dir_all(".ralph")?;
    let conn = db::open(&db::db_path())?;

    let t = db::get_task(&conn, task_id)?;
    if t.is_none() {
        let tasks = db::list_active_tasks(&conn)?;
        let valid_ids: Vec<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
        anyhow::bail!(
            "Unknown task ID '{}'. Valid IDs: {}",
            task_id,
            valid_ids.join(", ")
        );
    }

    let count = t.unwrap().guidance.len();
    db::set_guidance(&conn, task_id, &[])?;

    eprintln!("Cleared {count} guidance entries from {task_id}");
    Ok(())
}

async fn cmd_archive(task_id: Option<String>, done: bool) -> Result<()> {
    check_legacy_files()?;
    std::fs::create_dir_all(".ralph")?;
    let conn = db::open(&db::db_path())?;

    if let Some(ref id) = task_id {
        match db::get_task(&conn, id)? {
            None => {
                let tasks = db::list_active_tasks(&conn)?;
                let valid_ids: Vec<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
                anyhow::bail!(
                    "Unknown task ID '{}'. Valid IDs: {}",
                    id,
                    valid_ids.join(", ")
                );
            }
            Some(t)
                if !matches!(
                    t.phase,
                    task::Phase::Done | task::Phase::Failed | task::Phase::Skipped
                ) =>
            {
                anyhow::bail!(
                    "Task '{}' is not in a terminal phase (Done/Failed/Skipped)",
                    id
                );
            }
            Some(_) => {
                db::archive_task(&conn, id)?;
                eprintln!("Archived 1 task(s)");
            }
        }
    } else if done {
        let tasks = db::list_active_tasks(&conn)?;
        let to_archive: Vec<String> = tasks
            .iter()
            .filter(|t| matches!(t.phase, task::Phase::Done | task::Phase::Skipped))
            .map(|t| t.id.clone())
            .collect();

        if to_archive.is_empty() {
            eprintln!("No tasks to archive.");
            return Ok(());
        }

        let count = to_archive.len();
        for id in &to_archive {
            db::archive_task(&conn, id)?;
        }
        eprintln!("Archived {} task(s)", count);
    } else {
        anyhow::bail!("Provide a task ID or use --done to archive all completed tasks");
    }

    Ok(())
}

async fn cmd_restore(task_id: &str) -> Result<()> {
    check_legacy_files()?;
    std::fs::create_dir_all(".ralph")?;
    let conn = db::open(&db::db_path())?;

    match db::get_task(&conn, task_id)? {
        None => anyhow::bail!("Task '{}' not found in archive", task_id),
        Some(t) if !t.archived => {
            anyhow::bail!("Task ID '{}' already exists in active tasks", task_id)
        }
        Some(_) => {
            db::restore_task(&conn, task_id)?;
            eprintln!("Restored {task_id}");
        }
    }

    Ok(())
}

/// Format a Unix timestamp as RFC 3339 UTC (e.g. "2025-01-15T10:30:00Z").
fn fmt_rfc3339_utc(ts: u64) -> String {
    let time_of_day = ts % 86400;
    let days = ts / 86400;
    let h = time_of_day / 3600;
    let m = (time_of_day % 3600) / 60;
    let s = time_of_day % 60;

    // civil_from_days algorithm: https://howardhinnant.github.io/date_algorithms.html
    let z = days as i64 + 719468;
    let era = (if z >= 0 { z } else { z - 146096 }) / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe as i64 + era * 400 + if month <= 2 { 1 } else { 0 };

    format!("{year:04}-{month:02}-{day:02}T{h:02}:{m:02}:{s:02}Z")
}

fn format_task_md(out: &mut String, t: &task::Task) {
    let phase_name = format!("{:?}", t.phase);
    let attempt_word = if t.attempts == 1 { "attempt" } else { "attempts" };
    out.push_str(&format!(
        "\n### [{}] {} ({}, {} {})\n",
        t.id, t.title, phase_name, t.attempts, attempt_word
    ));

    let mut meta = format!("Priority: {}", t.priority);
    if let Some(s) = t.started_at {
        meta.push_str(&format!(" | Started: {}", fmt_rfc3339_utc(s)));
    }
    if let Some(c) = t.completed_at {
        meta.push_str(&format!(" | Completed: {}", fmt_rfc3339_utc(c)));
    }
    out.push_str(&meta);
    out.push('\n');

    if !t.blocked_by.is_empty() {
        out.push_str(&format!("Blocked by: {}\n", t.blocked_by.join(", ")));
    }

    if !t.description.is_empty() {
        out.push('\n');
        out.push_str(&t.description);
        out.push('\n');
    }

    if !t.files_changed.is_empty() {
        out.push_str("\nFiles changed:\n");
        for f in &t.files_changed {
            out.push_str(&format!("- {}\n", f.display()));
        }
    }

    if !t.feedback.is_empty() {
        out.push_str("\nFeedback:\n");
        for fb in &t.feedback {
            out.push_str(&format!("- {fb}\n"));
        }
    }

    if !t.guidance.is_empty() {
        out.push_str("\nGuidance:\n");
        for g in &t.guidance {
            out.push_str(&format!("- {g}\n"));
        }
    }

    if let Some(err) = &t.last_error {
        out.push_str(&format!("\nLast error: {err}\n"));
    }

    if let Some(pm) = &t.postmortem {
        out.push_str(&format!("\nPostmortem: {pm}\n"));
    }
}

fn cmd_dump_markdown(
    tasks: &[task::Task],
    directives: &[task::Directive],
    nits: &[nit::Nit],
    all: bool,
) -> Result<()> {
    let mut out = String::new();
    out.push_str("# Ralph Project State\n");

    let active: Vec<&task::Task> = tasks.iter().filter(|t| !t.archived).collect();
    out.push_str("\n## Active Tasks\n");
    if active.is_empty() {
        out.push_str("\n_(none)_\n");
    } else {
        for t in &active {
            format_task_md(&mut out, t);
        }
    }

    if all {
        let archived: Vec<&task::Task> = tasks.iter().filter(|t| t.archived).collect();
        out.push_str("\n## Archived Tasks\n");
        if archived.is_empty() {
            out.push_str("\n_(none)_\n");
        } else {
            for t in &archived {
                format_task_md(&mut out, t);
            }
        }
    }

    if !directives.is_empty() {
        out.push_str("\n## Directives (pending)\n");
        for d in directives {
            let action = match d.action {
                task::DirectiveAction::Skip => "Skip",
                task::DirectiveAction::Fail => "Fail",
                task::DirectiveAction::Reset => "Reset",
            };
            out.push_str(&format!("- {action} {}\n", d.task_id));
        }
    }

    if !nits.is_empty() {
        out.push_str("\n## Open Nits\n");
        for n in nits {
            out.push_str(&format!(
                "\n### {} (from {} on {}, attempt {})\n",
                n.id, n.source_role, n.source_task, n.attempt
            ));
            out.push_str(&n.content);
            out.push('\n');
        }
    }

    print!("{out}");
    Ok(())
}

fn cmd_dump_json(
    tasks: &[task::Task],
    directives: &[task::Directive],
    nits: &[nit::Nit],
) -> Result<()> {
    #[derive(serde::Serialize)]
    struct DumpTask<'a> {
        id: &'a str,
        title: &'a str,
        description: &'a str,
        priority: u32,
        blocked_by: &'a [String],
        phase: &'a task::Phase,
        attempts: u32,
        last_error: Option<&'a str>,
        files_changed: &'a [std::path::PathBuf],
        feedback: &'a [String],
        guidance: &'a [String],
        phase_entered_at: Option<u64>,
        started_at: Option<u64>,
        completed_at: Option<u64>,
        postmortem: Option<&'a str>,
        archived: bool,
    }

    #[derive(serde::Serialize)]
    struct DumpDirective<'a> {
        task_id: &'a str,
        action: &'a task::DirectiveAction,
    }

    #[derive(serde::Serialize)]
    struct Dump<'a> {
        tasks: Vec<DumpTask<'a>>,
        directives: Vec<DumpDirective<'a>>,
        nits: &'a [nit::Nit],
    }

    let dump = Dump {
        tasks: tasks
            .iter()
            .map(|t| DumpTask {
                id: &t.id,
                title: &t.title,
                description: &t.description,
                priority: t.priority,
                blocked_by: &t.blocked_by,
                phase: &t.phase,
                attempts: t.attempts,
                last_error: t.last_error.as_deref(),
                files_changed: &t.files_changed,
                feedback: &t.feedback,
                guidance: &t.guidance,
                phase_entered_at: t.phase_entered_at,
                started_at: t.started_at,
                completed_at: t.completed_at,
                postmortem: t.postmortem.as_deref(),
                archived: t.archived,
            })
            .collect(),
        directives: directives
            .iter()
            .map(|d| DumpDirective {
                task_id: &d.task_id,
                action: &d.action,
            })
            .collect(),
        nits,
    };

    println!("{}", serde_json::to_string_pretty(&dump)?);
    Ok(())
}

async fn cmd_dump(json: bool, all: bool) -> Result<()> {
    check_legacy_files()?;
    let db_path = db::db_path();
    if !db_path.exists() {
        if json {
            println!(r#"{{"tasks":[],"directives":[],"nits":[]}}"#);
        } else {
            println!("No tasks found. Run `ralph plan` first.");
        }
        return Ok(());
    }

    let conn = db::open(&db_path)?;
    let tasks = if all {
        db::list_all_tasks(&conn)?
    } else {
        db::list_active_tasks(&conn)?
    };
    let directives = db::peek_directives(&conn)?;
    // Markdown shows open nits; JSON shows all nits for a complete snapshot.
    let nits = db::list_nits(&conn, json)?;

    if json {
        cmd_dump_json(&tasks, &directives, &nits)
    } else {
        cmd_dump_markdown(&tasks, &directives, &nits, all)
    }
}

async fn cmd_import(dir: PathBuf) -> Result<()> {
    std::fs::create_dir_all(".ralph")?;
    let conn = db::open(&db::db_path())?;

    // ── Step 1: tasks.jsonl → active TaskDefs ─────────────────
    let tasks_path = dir.join("tasks.jsonl");
    let active_defs = if tasks_path.exists() {
        let contents = tokio::fs::read_to_string(&tasks_path).await?;
        task::parse_tasks(&contents)?
    } else {
        Vec::new()
    };

    // ── Step 2: state.json → ExecutionState ───────────────────
    let state_path = dir.join("state.json");
    let exec_state = if state_path.exists() {
        Some(state::ExecutionState::load(&state_path).await?)
    } else {
        None
    };

    // ── Step 3: archive.jsonl → archived TaskDefs ─────────────
    let archive_path = dir.join("archive.jsonl");
    let archive_defs = if archive_path.exists() {
        let contents = tokio::fs::read_to_string(&archive_path).await?;
        task::parse_tasks(&contents)?
    } else {
        Vec::new()
    };

    // ── Step 4: directives.json → Vec<Directive> (JSONL) ──────
    let directives_path = dir.join("directives.json");
    let directives_list: Vec<task::Directive> = if directives_path.exists() {
        let contents = tokio::fs::read_to_string(&directives_path).await?;
        contents
            .lines()
            .filter(|l| !l.trim().is_empty())
            .enumerate()
            .map(|(i, line)| {
                serde_json::from_str::<task::Directive>(line).map_err(|e| {
                    anyhow::anyhow!("parsing directive on line {}: {e}", i + 1)
                })
            })
            .collect::<anyhow::Result<Vec<_>>>()?
    } else {
        Vec::new()
    };

    // ── Step 5: nits.jsonl → Vec<Nit> ────────────────────────
    let nits_path = dir.join("nits.jsonl");
    let nits_list: Vec<nit::Nit> = if nits_path.exists() {
        let contents = tokio::fs::read_to_string(&nits_path).await?;
        contents
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|line| Ok(serde_json::from_str::<nit::Nit>(line)?))
            .collect::<anyhow::Result<Vec<_>>>()?
    } else {
        Vec::new()
    };

    // ── Merge execution state into Task objects ───────────────
    let apply_exec = |def: &task::TaskDef, archived: bool| -> task::Task {
        let mut t = task::Task::from_def(def);
        t.archived = archived;
        if let Some(ref es) = exec_state {
            if let Some(exec) = es.tasks.get(&def.id) {
                t.phase = exec.phase;
                t.attempts = exec.attempts;
                t.last_error = exec.last_error.clone();
                t.files_changed = exec.files_changed.clone();
                t.feedback = exec.feedback.clone();
                t.guidance = exec.guidance.clone();
                t.phase_entered_at = exec.phase_entered_at;
                t.started_at = exec.started_at;
                t.completed_at = exec.completed_at;
                t.postmortem = exec.postmortem.clone();
            }
        }
        t
    };

    let active_tasks: Vec<task::Task> =
        active_defs.iter().map(|d| apply_exec(d, false)).collect();
    let archived_tasks: Vec<task::Task> =
        archive_defs.iter().map(|d| apply_exec(d, true)).collect();

    // Warn about state entries with no corresponding task definition.
    if let Some(ref es) = exec_state {
        let known_ids: std::collections::HashSet<&str> = active_defs
            .iter()
            .chain(archive_defs.iter())
            .map(|d| d.id.as_str())
            .collect();
        for id in es.tasks.keys() {
            if !known_ids.contains(id.as_str()) {
                eprintln!("[ralph] warning: state.json references unknown task '{id}'");
            }
        }
    }

    // ── Atomic insert ─────────────────────────────────────────
    let tx = conn.unchecked_transaction()?;

    for t in active_tasks.iter().chain(archived_tasks.iter()) {
        db::upsert_task_no_tx(&tx, t)?;
    }
    for d in &directives_list {
        db::insert_directive(&tx, &d.task_id, d.action)?;
    }
    for n in &nits_list {
        db::insert_nit(&tx, n)?;
    }

    tx.commit()?;

    let archived_count = archived_tasks.len();
    let total_tasks = active_tasks.len() + archived_count;
    println!(
        "Imported {} tasks ({} archived), {} directives, {} nits.",
        total_tasks,
        archived_count,
        directives_list.len(),
        nits_list.len()
    );
    Ok(())
}

async fn cmd_nits_list(all: bool) -> Result<()> {
    check_legacy_files()?;
    std::fs::create_dir_all(".ralph")?;
    let conn = db::open(&db::db_path())?;
    // Load all nits for accurate counts; filter display based on `all`.
    let nits = db::list_nits(&conn, true)?;

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
    check_legacy_files()?;
    std::fs::create_dir_all(".ralph")?;
    let conn = db::open(&db::db_path())?;

    let nits = db::list_nits(&conn, true)?;
    let nit_entry = nits
        .iter()
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

    let active_tasks = db::list_active_tasks(&conn)?;
    let max_priority = active_tasks.iter().map(|t| t.priority).max().unwrap_or(0);
    let task_id = nit_id.replace('-', "");

    if db::get_task(&conn, &task_id)?.is_some() {
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

    let def = task::TaskDef {
        id: task_id.clone(),
        title,
        description: nit_entry.content.clone(),
        priority: max_priority + 1,
        blocked_by: vec![],
    };
    let new_task = task::Task::from_def(&def);
    db::insert_tasks(&conn, &[new_task])?;

    db::update_nit_status(&conn, nit_id, nit::NitStatus::Promoted, Some(&task_id))?;

    eprintln!("Promoted {nit_id} → task {task_id}");
    Ok(())
}

async fn cmd_nits_dismiss(nit_id: &str) -> Result<()> {
    check_legacy_files()?;
    std::fs::create_dir_all(".ralph")?;
    let conn = db::open(&db::db_path())?;

    let nits = db::list_nits(&conn, true)?;
    let nit_entry = nits
        .iter()
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

    db::update_nit_status(&conn, nit_id, nit::NitStatus::Dismissed, None)?;

    eprintln!("Dismissed {nit_id}");
    Ok(())
}

async fn cmd_nits_triage() -> Result<()> {
    check_legacy_files()?;
    let config = config::Config::load().await?;
    std::fs::create_dir_all(".ralph")?;
    let conn = db::open(&db::db_path())?;
    let registry = agent::ProcessRegistry::new(config.kill_grace_secs);
    orchestrator::spawn_signal_handler(registry.clone());
    let mut cost = 0.0;
    let promoted = orchestrator::triage_open_nits(&conn, &config, &registry, &mut cost).await?;
    if cost > 0.0 {
        eprintln!("[ralph] triage cost: ${cost:.4}");
    }
    if promoted == 0 {
        eprintln!("No nits promoted.");
    }
    Ok(())
}
