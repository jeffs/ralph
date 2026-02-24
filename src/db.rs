use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};

use crate::nit::{Nit, NitStatus};
use crate::task::{Phase, Task};

/// Default path to the SQLite database.
pub fn db_path() -> PathBuf {
    PathBuf::from(".ralph/ralph.db")
}

/// Open (or create) the database at `path`, configure pragmas, and
/// ensure all tables exist.
pub fn open(path: &Path) -> Result<Connection> {
    let conn = Connection::open(path)?;
    init_conn(&conn)?;
    Ok(conn)
}

/// Open an in-memory database with the same schema. Used in tests.
#[cfg(test)]
pub fn open_memory() -> Result<Connection> {
    let conn = Connection::open_in_memory()?;
    init_conn(&conn)?;
    Ok(conn)
}

fn init_conn(conn: &Connection) -> Result<()> {
    conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA foreign_keys = ON;")?;
    create_schema(conn)?;
    Ok(())
}

fn create_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS tasks (
            id               TEXT    PRIMARY KEY,
            title            TEXT    NOT NULL,
            description      TEXT    NOT NULL DEFAULT '',
            priority         INTEGER NOT NULL DEFAULT 0,
            phase            TEXT    NOT NULL DEFAULT 'Pending',
            attempts         INTEGER NOT NULL DEFAULT 0,
            last_error       TEXT,
            files_changed    TEXT    NOT NULL DEFAULT '[]',
            feedback         TEXT    NOT NULL DEFAULT '[]',
            guidance         TEXT    NOT NULL DEFAULT '[]',
            phase_entered_at INTEGER,
            started_at       INTEGER,
            completed_at     INTEGER,
            postmortem       TEXT,
            archived         INTEGER NOT NULL DEFAULT 0
        );

        CREATE TABLE IF NOT EXISTS task_deps (
            task_id TEXT NOT NULL REFERENCES tasks(id),
            dep_id  TEXT NOT NULL,
            PRIMARY KEY (task_id, dep_id)
        );

        CREATE TABLE IF NOT EXISTS nits (
            id          TEXT    PRIMARY KEY,
            source_task TEXT    NOT NULL,
            source_role TEXT    NOT NULL,
            attempt     INTEGER NOT NULL DEFAULT 0,
            content     TEXT    NOT NULL,
            summary     TEXT    NOT NULL DEFAULT '',
            status      TEXT    NOT NULL DEFAULT 'Open',
            promoted_to TEXT,
            created_at  INTEGER NOT NULL DEFAULT 0
        );

        CREATE TABLE IF NOT EXISTS meta (
            key   TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );

        INSERT OR IGNORE INTO meta (key, value) VALUES ('schema_version', '1');
        ",
    )?;
    Ok(())
}

// ── Internal helpers ─────────────────────────────────────────

fn phase_to_str(phase: Phase) -> &'static str {
    match phase {
        Phase::Pending => "Pending",
        Phase::Implementing => "Implementing",
        Phase::Testing => "Testing",
        Phase::Reviewing => "Reviewing",
        Phase::Done => "Done",
        Phase::Failed => "Failed",
        Phase::Skipped => "Skipped",
    }
}

fn phase_from_str(s: &str) -> Result<Phase> {
    match s {
        "Pending" => Ok(Phase::Pending),
        "Implementing" => Ok(Phase::Implementing),
        "Testing" => Ok(Phase::Testing),
        "Reviewing" => Ok(Phase::Reviewing),
        "Done" => Ok(Phase::Done),
        "Failed" => Ok(Phase::Failed),
        "Skipped" => Ok(Phase::Skipped),
        _ => anyhow::bail!("unknown phase: {s}"),
    }
}

fn nit_status_to_str(status: &NitStatus) -> &'static str {
    match status {
        NitStatus::Open => "Open",
        NitStatus::Promoted => "Promoted",
        NitStatus::Dismissed => "Dismissed",
    }
}

fn nit_status_from_str(s: &str) -> Result<NitStatus> {
    match s {
        "Open" => Ok(NitStatus::Open),
        "Promoted" => Ok(NitStatus::Promoted),
        "Dismissed" => Ok(NitStatus::Dismissed),
        _ => anyhow::bail!("unknown nit status: {s}"),
    }
}

/// Intermediate row data before deps and JSON fields are resolved.
struct PartialTask {
    id: String,
    title: String,
    description: String,
    priority: u32,
    phase_str: String,
    attempts: u32,
    last_error: Option<String>,
    files_json: String,
    feedback_json: String,
    guidance_json: String,
    phase_entered_at: Option<u64>,
    started_at: Option<u64>,
    completed_at: Option<u64>,
    postmortem: Option<String>,
    archived: bool,
}

fn row_to_partial(row: &rusqlite::Row) -> rusqlite::Result<PartialTask> {
    Ok(PartialTask {
        id: row.get(0)?,
        title: row.get(1)?,
        description: row.get(2)?,
        priority: row.get::<_, i64>(3)? as u32,
        phase_str: row.get(4)?,
        attempts: row.get::<_, i64>(5)? as u32,
        last_error: row.get(6)?,
        files_json: row.get(7)?,
        feedback_json: row.get(8)?,
        guidance_json: row.get(9)?,
        phase_entered_at: row.get::<_, Option<i64>>(10)?.map(|v| v as u64),
        started_at: row.get::<_, Option<i64>>(11)?.map(|v| v as u64),
        completed_at: row.get::<_, Option<i64>>(12)?.map(|v| v as u64),
        postmortem: row.get(13)?,
        archived: row.get::<_, i64>(14)? != 0,
    })
}

fn partial_into_task(p: PartialTask, blocked_by: Vec<String>) -> Result<Task> {
    Ok(Task {
        phase: phase_from_str(&p.phase_str)?,
        files_changed: serde_json::from_str(&p.files_json)
            .context("deserializing files_changed")?,
        feedback: serde_json::from_str(&p.feedback_json)
            .context("deserializing feedback")?,
        guidance: serde_json::from_str(&p.guidance_json)
            .context("deserializing guidance")?,
        id: p.id,
        title: p.title,
        description: p.description,
        priority: p.priority,
        blocked_by,
        attempts: p.attempts,
        last_error: p.last_error,
        phase_entered_at: p.phase_entered_at,
        started_at: p.started_at,
        completed_at: p.completed_at,
        postmortem: p.postmortem,
        archived: p.archived,
    })
}

/// Upsert a single task without starting a new transaction.
/// Use within an already-open transaction for batch imports.
pub fn upsert_task_no_tx(conn: &Connection, task: &Task) -> Result<()> {
    upsert_task_in(conn, task)
}

/// Execute the upsert + deps replacement for a single task.
/// Caller must wrap this in a transaction if atomicity is needed.
fn upsert_task_in(conn: &Connection, task: &Task) -> Result<()> {
    let files_json = serde_json::to_string(&task.files_changed)?;
    let feedback_json = serde_json::to_string(&task.feedback)?;
    let guidance_json = serde_json::to_string(&task.guidance)?;
    let phase_str = phase_to_str(task.phase);

    conn.execute(
        "INSERT INTO tasks (id, title, description, priority, phase, attempts,
         last_error, files_changed, feedback, guidance, phase_entered_at,
         started_at, completed_at, postmortem, archived)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)
         ON CONFLICT(id) DO UPDATE SET
           title            = excluded.title,
           description      = excluded.description,
           priority         = excluded.priority,
           phase            = excluded.phase,
           attempts         = excluded.attempts,
           last_error       = excluded.last_error,
           files_changed    = excluded.files_changed,
           feedback         = excluded.feedback,
           guidance         = excluded.guidance,
           phase_entered_at = excluded.phase_entered_at,
           started_at       = excluded.started_at,
           completed_at     = excluded.completed_at,
           postmortem       = excluded.postmortem,
           archived         = excluded.archived",
        params![
            &task.id,
            &task.title,
            &task.description,
            task.priority as i64,
            phase_str,
            task.attempts as i64,
            &task.last_error,
            &files_json,
            &feedback_json,
            &guidance_json,
            task.phase_entered_at.map(|v| v as i64),
            task.started_at.map(|v| v as i64),
            task.completed_at.map(|v| v as i64),
            &task.postmortem,
            task.archived as i32
        ],
    )?;

    conn.execute(
        "DELETE FROM task_deps WHERE task_id = ?1",
        params![&task.id],
    )?;
    for dep in &task.blocked_by {
        conn.execute(
            "INSERT INTO task_deps (task_id, dep_id) VALUES (?1, ?2)",
            params![&task.id, dep],
        )?;
    }
    Ok(())
}

/// Query tasks matching `filter` (a WHERE clause or empty string), ordered by priority.
fn list_tasks_where(conn: &Connection, filter: &str) -> Result<Vec<Task>> {
    let sql = format!(
        "SELECT id, title, description, priority, phase, attempts, last_error, \
                files_changed, feedback, guidance, phase_entered_at, started_at, \
                completed_at, postmortem, archived \
         FROM tasks {filter} ORDER BY priority"
    );
    let mut stmt = conn.prepare(&sql)?;
    let partials: Vec<PartialTask> = stmt
        .query_map([], row_to_partial)?
        .collect::<rusqlite::Result<_>>()?;

    let deps_sql = format!(
        "SELECT task_id, dep_id FROM task_deps \
         WHERE task_id IN (SELECT id FROM tasks {filter}) \
         ORDER BY task_id, dep_id"
    );
    let mut deps_stmt = conn.prepare(&deps_sql)?;
    let pairs: Vec<(String, String)> = deps_stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let mut deps: HashMap<String, Vec<String>> = HashMap::new();
    for (task_id, dep_id) in pairs {
        deps.entry(task_id).or_default().push(dep_id);
    }

    partials
        .into_iter()
        .map(|p| {
            let blocked_by = deps.remove(&p.id).unwrap_or_default();
            partial_into_task(p, blocked_by)
        })
        .collect()
}

// ── Task CRUD ─────────────────────────────────────────────────

/// Upsert a task row and replace its deps atomically.
#[cfg(test)]
pub fn insert_task(conn: &Connection, task: &Task) -> Result<()> {
    let tx = conn.unchecked_transaction()?;
    upsert_task_in(&tx, task)?;
    tx.commit()?;
    Ok(())
}

/// Batch-upsert a slice of tasks in a single transaction.
pub fn insert_tasks(conn: &Connection, tasks: &[Task]) -> Result<()> {
    let tx = conn.unchecked_transaction()?;
    for task in tasks {
        upsert_task_in(&tx, task)?;
    }
    tx.commit()?;
    Ok(())
}

/// Fetch a single task by ID, joining deps. Returns `None` if not found.
pub fn get_task(conn: &Connection, id: &str) -> Result<Option<Task>> {
    let partial = conn
        .query_row(
            "SELECT id, title, description, priority, phase, attempts, last_error, \
                    files_changed, feedback, guidance, phase_entered_at, started_at, \
                    completed_at, postmortem, archived \
             FROM tasks WHERE id = ?1",
            params![id],
            row_to_partial,
        )
        .optional()?;

    let Some(p) = partial else {
        return Ok(None);
    };

    let blocked_by: Vec<String> = {
        let mut stmt =
            conn.prepare("SELECT dep_id FROM task_deps WHERE task_id = ?1 ORDER BY dep_id")?;
        stmt.query_map(params![id], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?
    };

    Ok(Some(partial_into_task(p, blocked_by)?))
}

/// List non-archived tasks, ordered by priority.
pub fn list_active_tasks(conn: &Connection) -> Result<Vec<Task>> {
    list_tasks_where(conn, "WHERE archived = 0")
}

/// List all tasks including archived, ordered by priority.
pub fn list_all_tasks(conn: &Connection) -> Result<Vec<Task>> {
    list_tasks_where(conn, "")
}

/// Count non-archived tasks that are not in a terminal phase (Done or Skipped).
pub fn count_non_terminal(conn: &Connection) -> Result<u64> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM tasks WHERE archived = 0 AND phase NOT IN ('Done', 'Skipped')",
        [],
        |row| row.get(0),
    )?;
    Ok(count as u64)
}

/// Count archived tasks.
pub fn count_archived(conn: &Connection) -> Result<u64> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM tasks WHERE archived = 1",
        [],
        |row| row.get(0),
    )?;
    Ok(count as u64)
}

/// Return the maximum priority among non-archived tasks, or `None` if there are none.
pub fn max_priority(conn: &Connection) -> Result<Option<u32>> {
    let max: Option<i64> = conn.query_row(
        "SELECT MAX(priority) FROM tasks WHERE archived = 0",
        [],
        |row| row.get(0),
    )?;
    Ok(max.map(|v| v as u32))
}

/// Update phase and related timestamps.
///
/// - Always sets `phase_entered_at` to `now`.
/// - Sets `started_at` to `now` on the first non-Pending transition
///   (i.e. only when `started_at` is currently NULL).
/// - Sets `completed_at` to `now` when transitioning to Done/Failed/Skipped.
pub fn update_phase(conn: &Connection, id: &str, phase: Phase, now: u64) -> Result<()> {
    let phase_str = phase_to_str(phase);
    let now_i64 = now as i64;
    let is_not_pending: i32 = if !matches!(phase, Phase::Pending) { 1 } else { 0 };
    let is_terminal: i32 =
        if matches!(phase, Phase::Done | Phase::Failed | Phase::Skipped) { 1 } else { 0 };

    conn.execute(
        "UPDATE tasks SET
            phase            = ?1,
            phase_entered_at = ?2,
            started_at       = CASE WHEN started_at IS NULL AND ?3 = 1 THEN ?2 ELSE started_at END,
            completed_at     = CASE WHEN ?4 = 1 THEN ?2 ELSE completed_at END
         WHERE id = ?5",
        params![phase_str, now_i64, is_not_pending, is_terminal, id],
    )?;
    Ok(())
}

pub fn update_attempts(conn: &Connection, id: &str, attempts: u32) -> Result<()> {
    conn.execute(
        "UPDATE tasks SET attempts = ?1 WHERE id = ?2",
        params![attempts as i64, id],
    )?;
    Ok(())
}

pub fn update_last_error(conn: &Connection, id: &str, error: Option<&str>) -> Result<()> {
    conn.execute(
        "UPDATE tasks SET last_error = ?1 WHERE id = ?2",
        params![error, id],
    )?;
    Ok(())
}

/// Replace `files_changed`, serialized as a JSON array of path strings.
pub fn update_files_changed(conn: &Connection, id: &str, files: &[PathBuf]) -> Result<()> {
    let files_json = serde_json::to_string(files)?;
    conn.execute(
        "UPDATE tasks SET files_changed = ?1 WHERE id = ?2",
        params![files_json, id],
    )?;
    Ok(())
}

/// Append one entry to the task's feedback JSON array.
pub fn push_feedback(conn: &Connection, id: &str, entry: &str) -> Result<()> {
    let feedback_json: String = conn.query_row(
        "SELECT feedback FROM tasks WHERE id = ?1",
        params![id],
        |row| row.get(0),
    )?;
    let mut feedback: Vec<String> =
        serde_json::from_str(&feedback_json).context("deserializing feedback")?;
    feedback.push(entry.to_string());
    let new_json = serde_json::to_string(&feedback)?;
    conn.execute(
        "UPDATE tasks SET feedback = ?1 WHERE id = ?2",
        params![new_json, id],
    )?;
    Ok(())
}

/// Reset the feedback array to empty.
pub fn clear_feedback(conn: &Connection, id: &str) -> Result<()> {
    conn.execute(
        "UPDATE tasks SET feedback = '[]' WHERE id = ?1",
        params![id],
    )?;
    Ok(())
}

/// Replace the entire guidance array.
pub fn set_guidance(conn: &Connection, id: &str, guidance: &[String]) -> Result<()> {
    let guidance_json = serde_json::to_string(guidance)?;
    conn.execute(
        "UPDATE tasks SET guidance = ?1 WHERE id = ?2",
        params![guidance_json, id],
    )?;
    Ok(())
}

#[cfg(test)]
pub fn update_postmortem(conn: &Connection, id: &str, text: Option<&str>) -> Result<()> {
    conn.execute(
        "UPDATE tasks SET postmortem = ?1 WHERE id = ?2",
        params![text, id],
    )?;
    Ok(())
}

pub fn archive_task(conn: &Connection, id: &str) -> Result<()> {
    conn.execute(
        "UPDATE tasks SET archived = 1 WHERE id = ?1",
        params![id],
    )?;
    Ok(())
}

pub fn restore_task(conn: &Connection, id: &str) -> Result<()> {
    conn.execute(
        "UPDATE tasks SET archived = 0 WHERE id = ?1",
        params![id],
    )?;
    Ok(())
}

/// Archive all non-archived tasks in a terminal phase (Done or Skipped).
/// Returns the number of rows affected.
pub fn archive_done_tasks(conn: &Connection) -> Result<u64> {
    let rows = conn.execute(
        "UPDATE tasks SET archived = 1 WHERE archived = 0 AND phase IN ('Done', 'Skipped')",
        [],
    )?;
    Ok(rows as u64)
}

/// Reset all non-archived Failed tasks back to Pending.
///
/// Sets `phase = 'Pending'`, `phase_entered_at = now`, `attempts = 0`,
/// `last_error = NULL`, and `feedback = '[]'`. Guidance is preserved.
/// Returns the number of rows affected.
pub fn reset_all_failed(conn: &Connection) -> Result<u64> {
    let now = crate::task::unix_now() as i64;
    let rows = conn.execute(
        "UPDATE tasks SET
            phase            = 'Pending',
            phase_entered_at = ?1,
            attempts         = 0,
            last_error       = NULL,
            feedback         = '[]'
         WHERE archived = 0 AND phase = 'Failed'",
        params![now],
    )?;
    Ok(rows as u64)
}

// ── Dependency queries ────────────────────────────────────────

/// Return the set of all task IDs (active + archived).
pub fn task_ids_in_use(conn: &Connection) -> Result<HashSet<String>> {
    let mut stmt = conn.prepare("SELECT id FROM tasks")?;
    let ids: HashSet<String> = stmt
        .query_map([], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(ids)
}

/// Verify that every dep_id in task_deps references an existing task ID and
/// that no dependency cycles exist.
pub fn validate_deps(conn: &Connection) -> Result<()> {
    let all_ids = task_ids_in_use(conn)?;
    let mut stmt = conn.prepare("SELECT task_id, dep_id FROM task_deps")?;
    let deps: Vec<(String, String)> = stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;

    let mut bad = Vec::new();
    for (task_id, dep_id) in &deps {
        if !all_ids.contains(dep_id) {
            bad.push(format!("{task_id} blocked_by unknown task {dep_id}"));
        }
    }

    // DFS-based cycle detection among tasks present in the database.
    // Returns the cycle as [a, b, ..., a] (start node repeated at end), or None.
    fn dfs(
        id: &str,
        adj: &HashMap<String, Vec<String>>,
        state: &mut HashMap<String, u8>,
        path: &mut Vec<String>,
    ) -> Option<Vec<String>> {
        state.insert(id.to_string(), 1);
        path.push(id.to_string());
        if let Some(deps) = adj.get(id) {
            for dep in deps {
                match *state.get(dep).unwrap_or(&0) {
                    1 => {
                        // dep is on the current DFS stack — cycle found.
                        if let Some(idx) = path.iter().position(|x| x == dep) {
                            let mut cycle = path[idx..].to_vec();
                            cycle.push(dep.clone());
                            return Some(cycle);
                        }
                    }
                    0 => {
                        if let Some(c) = dfs(dep, adj, state, path) {
                            return Some(c);
                        }
                    }
                    _ => {}
                }
            }
        }
        path.pop();
        state.insert(id.to_string(), 2);
        None
    }

    // Build adjacency list from dep entries where both ends exist in the DB.
    let mut adj: HashMap<String, Vec<String>> = HashMap::new();
    for (task_id, dep_id) in &deps {
        if all_ids.contains(dep_id) {
            adj.entry(task_id.clone()).or_default().push(dep_id.clone());
        }
    }
    let mut state: HashMap<String, u8> = HashMap::new();
    for id in &all_ids {
        if *state.get(id).unwrap_or(&0) == 0 {
            let mut path = Vec::new();
            if let Some(cycle) = dfs(id, &adj, &mut state, &mut path) {
                bad.push(format!("dependency cycle: {}", cycle.join(" -> ")));
            }
        }
    }

    if bad.is_empty() {
        Ok(())
    } else {
        anyhow::bail!("dependency validation failed:\n  {}", bad.join("\n  "))
    }
}

// ── Nit CRUD ──────────────────────────────────────────────────

/// Counts of nits grouped by status.
pub struct NitCounts {
    pub open: u64,
    pub promoted: u64,
    pub dismissed: u64,
}

pub fn insert_nit(conn: &Connection, nit: &Nit) -> Result<()> {
    let status_str = nit_status_to_str(&nit.status);
    conn.execute(
        "INSERT INTO nits (id, source_task, source_role, attempt, content, summary,
                           status, promoted_to, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
         ON CONFLICT(id) DO UPDATE SET
           source_task = excluded.source_task,
           source_role = excluded.source_role,
           attempt     = excluded.attempt,
           content     = excluded.content,
           summary     = excluded.summary,
           status      = excluded.status,
           promoted_to = excluded.promoted_to,
           created_at  = excluded.created_at",
        params![
            &nit.id,
            &nit.source_task,
            &nit.source_role,
            nit.attempt as i64,
            &nit.content,
            &nit.summary,
            status_str,
            &nit.promoted_to,
            nit.created_at as i64
        ],
    )?;
    Ok(())
}

/// List nits. When `include_all` is false, only open nits are returned.
pub fn list_nits(conn: &Connection, include_all: bool) -> Result<Vec<Nit>> {
    let sql = if include_all {
        "SELECT id, source_task, source_role, attempt, content, summary, \
                status, promoted_to, created_at \
         FROM nits ORDER BY created_at"
    } else {
        "SELECT id, source_task, source_role, attempt, content, summary, \
                status, promoted_to, created_at \
         FROM nits WHERE status = 'Open' ORDER BY created_at"
    };

    let mut stmt = conn.prepare(sql)?;
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, Option<String>>(7)?,
                row.get::<_, i64>(8)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    rows.into_iter()
        .map(
            |(id, source_task, source_role, attempt, content, summary, status_str, promoted_to, created_at)| {
                Ok(Nit {
                    id,
                    source_task,
                    source_role,
                    attempt: attempt as u32,
                    content,
                    summary,
                    status: nit_status_from_str(&status_str)?,
                    promoted_to,
                    created_at: created_at as u64,
                })
            },
        )
        .collect()
}

/// Fetch a single nit by ID. Returns `None` if not found.
pub fn get_nit(conn: &Connection, id: &str) -> Result<Option<Nit>> {
    let row = conn
        .query_row(
            "SELECT id, source_task, source_role, attempt, content, summary, \
                    status, promoted_to, created_at \
             FROM nits WHERE id = ?1",
            params![id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, Option<String>>(7)?,
                    row.get::<_, i64>(8)?,
                ))
            },
        )
        .optional()?;

    let Some((id, source_task, source_role, attempt, content, summary, status_str, promoted_to, created_at)) = row else {
        return Ok(None);
    };

    Ok(Some(Nit {
        id,
        source_task,
        source_role,
        attempt: attempt as u32,
        content,
        summary,
        status: nit_status_from_str(&status_str)?,
        promoted_to,
        created_at: created_at as u64,
    }))
}

pub fn update_nit_status(
    conn: &Connection,
    id: &str,
    status: NitStatus,
    promoted_to: Option<&str>,
) -> Result<()> {
    let status_str = nit_status_to_str(&status);
    conn.execute(
        "UPDATE nits SET status = ?1, promoted_to = ?2 WHERE id = ?3",
        params![status_str, promoted_to, id],
    )?;
    Ok(())
}

/// Return counts of nits for each status. Missing statuses default to 0.
pub fn nit_status_counts(conn: &Connection) -> Result<NitCounts> {
    let mut stmt = conn.prepare("SELECT status, COUNT(*) FROM nits GROUP BY status")?;
    let rows: Vec<(String, i64)> = stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;

    let mut counts = NitCounts { open: 0, promoted: 0, dismissed: 0 };
    for (status, count) in rows {
        match status.as_str() {
            "Open" => counts.open = count as u64,
            "Promoted" => counts.promoted = count as u64,
            "Dismissed" => counts.dismissed = count as u64,
            _ => {}
        }
    }
    Ok(counts)
}

/// Return the next available NIT-N ID.
pub fn next_nit_id(conn: &Connection) -> Result<String> {
    let max: Option<i64> = conn.query_row(
        "SELECT MAX(CAST(SUBSTR(id, 5) AS INTEGER)) FROM nits WHERE id LIKE 'NIT-%'",
        [],
        |row| row.get(0),
    )?;
    Ok(format!("NIT-{}", max.unwrap_or(0) + 1))
}

// ── ID helpers ────────────────────────────────────────────────

/// Summarise all in-use ID ranges. Delegates to `task::id_ranges_summary_from_ids`.
pub fn id_ranges_summary(conn: &Connection) -> Result<String> {
    let mut ids: Vec<String> = task_ids_in_use(conn)?.into_iter().collect();
    ids.sort_unstable(); // deterministic order for tests
    Ok(crate::task::id_ranges_summary_from_ids(&ids))
}

/// Return the next available GEN-N ID.
pub fn next_generated_id(conn: &Connection) -> Result<String> {
    let max: Option<i64> = conn.query_row(
        "SELECT MAX(CAST(SUBSTR(id, 5) AS INTEGER)) FROM tasks WHERE id LIKE 'GEN-%'",
        [],
        |row| row.get(0),
    )?;
    Ok(format!("GEN-{}", max.unwrap_or(0) + 1))
}

// ── Tests ─────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_task(id: &str) -> Task {
        Task {
            id: id.to_string(),
            title: format!("Task {id}"),
            description: String::new(),
            priority: 1,
            blocked_by: vec![],
            phase: Phase::Pending,
            attempts: 0,
            last_error: None,
            files_changed: vec![],
            feedback: vec![],
            guidance: vec![],
            phase_entered_at: None,
            started_at: None,
            completed_at: None,
            postmortem: None,
            archived: false,
        }
    }

    fn make_nit(id: &str, status: NitStatus) -> Nit {
        Nit {
            id: id.to_string(),
            source_task: "T1".to_string(),
            source_role: "reviewer".to_string(),
            attempt: 1,
            content: "fix this".to_string(),
            summary: String::new(),
            status,
            promoted_to: None,
            created_at: 1000,
        }
    }

    // ── Schema ──────────────────────────────────────────────

    #[test]
    fn open_memory_succeeds_and_schema_version_is_1() {
        let conn = open_memory().expect("open_memory failed");

        let table_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name IN \
                 ('tasks','task_deps','nits','meta')",
                [],
                |row| row.get(0),
            )
            .expect("sqlite_master query failed");
        assert_eq!(table_count, 4, "expected 4 tables, found {table_count}");

        let version: String = conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'schema_version'",
                [],
                |row| row.get(0),
            )
            .expect("schema_version not found");
        assert_eq!(version, "1");
    }

    // ── Task insert / get ───────────────────────────────────

    #[test]
    fn insert_and_get_task_basic_fields() {
        let conn = open_memory().unwrap();
        let mut task = make_task("T1");
        task.description = "do the thing".to_string();
        task.priority = 5;
        task.phase = Phase::Implementing;
        task.attempts = 2;
        task.last_error = Some("oops".to_string());
        task.postmortem = Some("root cause found".to_string());

        insert_task(&conn, &task).unwrap();
        let got = get_task(&conn, "T1").unwrap().unwrap();

        assert_eq!(got.id, "T1");
        assert_eq!(got.title, "Task T1");
        assert_eq!(got.description, "do the thing");
        assert_eq!(got.priority, 5);
        assert_eq!(got.phase, Phase::Implementing);
        assert_eq!(got.attempts, 2);
        assert_eq!(got.last_error.as_deref(), Some("oops"));
        assert_eq!(got.postmortem.as_deref(), Some("root cause found"));
        assert!(!got.archived);
    }

    #[test]
    fn get_task_not_found_returns_none() {
        let conn = open_memory().unwrap();
        let result = get_task(&conn, "MISSING").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn insert_task_with_deps() {
        let conn = open_memory().unwrap();
        insert_task(&conn, &make_task("A")).unwrap();
        let mut b = make_task("B");
        b.blocked_by = vec!["A".to_string()];
        insert_task(&conn, &b).unwrap();

        let got = get_task(&conn, "B").unwrap().unwrap();
        assert_eq!(got.blocked_by, vec!["A"]);
    }

    #[test]
    fn insert_task_upsert_updates_existing() {
        let conn = open_memory().unwrap();
        insert_task(&conn, &make_task("T1")).unwrap();

        let mut updated = make_task("T1");
        updated.title = "Updated Title".to_string();
        updated.priority = 99;
        insert_task(&conn, &updated).unwrap();

        let got = get_task(&conn, "T1").unwrap().unwrap();
        assert_eq!(got.title, "Updated Title");
        assert_eq!(got.priority, 99);
    }

    #[test]
    fn insert_task_upsert_replaces_deps() {
        let conn = open_memory().unwrap();
        insert_task(&conn, &make_task("A")).unwrap();
        insert_task(&conn, &make_task("C")).unwrap();

        let mut b = make_task("B");
        b.blocked_by = vec!["A".to_string()];
        insert_task(&conn, &b).unwrap();

        // Re-insert with different deps
        let mut b2 = make_task("B");
        b2.blocked_by = vec!["C".to_string()];
        insert_task(&conn, &b2).unwrap();

        let got = get_task(&conn, "B").unwrap().unwrap();
        assert_eq!(got.blocked_by, vec!["C"]);
    }

    #[test]
    fn insert_tasks_batch() {
        let conn = open_memory().unwrap();
        let tasks = vec![make_task("T1"), make_task("T2"), make_task("T3")];
        insert_tasks(&conn, &tasks).unwrap();

        assert!(get_task(&conn, "T1").unwrap().is_some());
        assert!(get_task(&conn, "T2").unwrap().is_some());
        assert!(get_task(&conn, "T3").unwrap().is_some());
    }

    #[test]
    fn json_fields_roundtrip() {
        let conn = open_memory().unwrap();
        let mut task = make_task("T1");
        task.files_changed = vec![
            PathBuf::from("src/main.rs"),
            PathBuf::from("README.md"),
        ];
        task.feedback = vec!["test failed".to_string(), "build error".to_string()];
        task.guidance = vec!["use bun".to_string()];

        insert_task(&conn, &task).unwrap();
        let got = get_task(&conn, "T1").unwrap().unwrap();

        assert_eq!(got.files_changed, task.files_changed);
        assert_eq!(got.feedback, task.feedback);
        assert_eq!(got.guidance, task.guidance);
    }

    // ── List functions ──────────────────────────────────────

    #[test]
    fn list_active_tasks_excludes_archived() {
        let conn = open_memory().unwrap();
        insert_task(&conn, &make_task("T1")).unwrap();
        let mut archived = make_task("T2");
        archived.archived = true;
        insert_task(&conn, &archived).unwrap();

        let active = list_active_tasks(&conn).unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].id, "T1");
    }

    #[test]
    fn list_all_tasks_includes_archived() {
        let conn = open_memory().unwrap();
        insert_task(&conn, &make_task("T1")).unwrap();
        let mut archived = make_task("T2");
        archived.archived = true;
        insert_task(&conn, &archived).unwrap();

        let all = list_all_tasks(&conn).unwrap();
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn list_active_tasks_ordered_by_priority() {
        let conn = open_memory().unwrap();
        let mut t1 = make_task("T1");
        t1.priority = 3;
        let mut t2 = make_task("T2");
        t2.priority = 1;
        let mut t3 = make_task("T3");
        t3.priority = 2;
        insert_tasks(&conn, &[t1, t2, t3]).unwrap();

        let tasks = list_active_tasks(&conn).unwrap();
        assert_eq!(tasks[0].id, "T2"); // priority 1
        assert_eq!(tasks[1].id, "T3"); // priority 2
        assert_eq!(tasks[2].id, "T1"); // priority 3
    }

    #[test]
    fn list_tasks_populates_blocked_by() {
        let conn = open_memory().unwrap();
        insert_task(&conn, &make_task("A")).unwrap();
        let mut b = make_task("B");
        b.blocked_by = vec!["A".to_string()];
        insert_task(&conn, &b).unwrap();

        let tasks = list_all_tasks(&conn).unwrap();
        let b_task = tasks.iter().find(|t| t.id == "B").unwrap();
        assert_eq!(b_task.blocked_by, vec!["A"]);
    }

    #[test]
    fn count_non_terminal_excludes_done_and_skipped() {
        let conn = open_memory().unwrap();

        let mut t1 = make_task("T1");
        t1.phase = Phase::Pending;
        insert_task(&conn, &t1).unwrap();

        let mut t2 = make_task("T2");
        t2.phase = Phase::Implementing;
        insert_task(&conn, &t2).unwrap();

        let mut t3 = make_task("T3");
        t3.phase = Phase::Done;
        insert_task(&conn, &t3).unwrap();

        let mut t4 = make_task("T4");
        t4.phase = Phase::Skipped;
        insert_task(&conn, &t4).unwrap();

        let mut t5 = make_task("T5");
        t5.phase = Phase::Failed;
        insert_task(&conn, &t5).unwrap();

        // Archived task in a non-terminal phase should not be counted
        let mut t6 = make_task("T6");
        t6.phase = Phase::Pending;
        t6.archived = true;
        insert_task(&conn, &t6).unwrap();

        let count = count_non_terminal(&conn).unwrap();
        assert_eq!(count, 3); // Pending, Implementing, Failed
    }

    // ── max_priority ────────────────────────────────────────

    #[test]
    fn max_priority_empty_db_returns_none() {
        let conn = open_memory().unwrap();
        assert_eq!(max_priority(&conn).unwrap(), None);
    }

    #[test]
    fn max_priority_single_task_returns_its_priority() {
        let conn = open_memory().unwrap();
        let mut t = make_task("T1");
        t.priority = 7;
        insert_task(&conn, &t).unwrap();
        assert_eq!(max_priority(&conn).unwrap(), Some(7));
    }

    #[test]
    fn max_priority_multiple_tasks_returns_max() {
        let conn = open_memory().unwrap();
        let mut t1 = make_task("T1");
        t1.priority = 3;
        let mut t2 = make_task("T2");
        t2.priority = 10;
        let mut t3 = make_task("T3");
        t3.priority = 1;
        insert_tasks(&conn, &[t1, t2, t3]).unwrap();
        assert_eq!(max_priority(&conn).unwrap(), Some(10));
    }

    #[test]
    fn max_priority_excludes_archived_tasks() {
        let conn = open_memory().unwrap();
        let mut archived = make_task("T1");
        archived.priority = 99;
        archived.archived = true;
        insert_task(&conn, &archived).unwrap();

        // No active tasks — should return None even though archived task exists.
        assert_eq!(max_priority(&conn).unwrap(), None);

        // Add an active task with lower priority.
        let mut active = make_task("T2");
        active.priority = 5;
        insert_task(&conn, &active).unwrap();
        assert_eq!(max_priority(&conn).unwrap(), Some(5));
    }

    // ── update_phase ────────────────────────────────────────

    #[test]
    fn update_phase_sets_phase_and_entered_at() {
        let conn = open_memory().unwrap();
        insert_task(&conn, &make_task("T1")).unwrap();

        update_phase(&conn, "T1", Phase::Implementing, 1000).unwrap();
        let t = get_task(&conn, "T1").unwrap().unwrap();

        assert_eq!(t.phase, Phase::Implementing);
        assert_eq!(t.phase_entered_at, Some(1000));
    }

    #[test]
    fn update_phase_sets_started_at_on_first_non_pending() {
        let conn = open_memory().unwrap();
        insert_task(&conn, &make_task("T1")).unwrap();

        update_phase(&conn, "T1", Phase::Implementing, 1000).unwrap();
        let t = get_task(&conn, "T1").unwrap().unwrap();

        assert_eq!(t.started_at, Some(1000));
        assert_eq!(t.completed_at, None);
    }

    #[test]
    fn update_phase_does_not_set_started_at_for_pending() {
        let conn = open_memory().unwrap();
        insert_task(&conn, &make_task("T1")).unwrap();

        update_phase(&conn, "T1", Phase::Pending, 1000).unwrap();
        let t = get_task(&conn, "T1").unwrap().unwrap();

        assert_eq!(t.started_at, None);
    }

    #[test]
    fn update_phase_preserves_started_at_on_subsequent_transitions() {
        let conn = open_memory().unwrap();
        insert_task(&conn, &make_task("T1")).unwrap();

        update_phase(&conn, "T1", Phase::Implementing, 1000).unwrap();
        update_phase(&conn, "T1", Phase::Testing, 2000).unwrap();

        let t = get_task(&conn, "T1").unwrap().unwrap();
        assert_eq!(t.started_at, Some(1000)); // preserved
        assert_eq!(t.phase_entered_at, Some(2000)); // updated
    }

    #[test]
    fn update_phase_sets_completed_at_on_done() {
        let conn = open_memory().unwrap();
        insert_task(&conn, &make_task("T1")).unwrap();
        update_phase(&conn, "T1", Phase::Implementing, 500).unwrap();

        update_phase(&conn, "T1", Phase::Done, 9000).unwrap();
        let t = get_task(&conn, "T1").unwrap().unwrap();

        assert_eq!(t.phase, Phase::Done);
        assert_eq!(t.completed_at, Some(9000));
        assert_eq!(t.started_at, Some(500)); // preserved
    }

    #[test]
    fn update_phase_sets_completed_at_on_failed() {
        let conn = open_memory().unwrap();
        insert_task(&conn, &make_task("T1")).unwrap();
        update_phase(&conn, "T1", Phase::Failed, 9000).unwrap();
        let t = get_task(&conn, "T1").unwrap().unwrap();
        assert_eq!(t.completed_at, Some(9000));
    }

    #[test]
    fn update_phase_sets_completed_at_on_skipped() {
        let conn = open_memory().unwrap();
        insert_task(&conn, &make_task("T1")).unwrap();
        update_phase(&conn, "T1", Phase::Skipped, 9000).unwrap();
        let t = get_task(&conn, "T1").unwrap().unwrap();
        assert_eq!(t.completed_at, Some(9000));
    }

    #[test]
    fn update_phase_does_not_set_completed_at_for_non_terminal() {
        let conn = open_memory().unwrap();
        insert_task(&conn, &make_task("T1")).unwrap();
        update_phase(&conn, "T1", Phase::Testing, 5000).unwrap();
        let t = get_task(&conn, "T1").unwrap().unwrap();
        assert_eq!(t.completed_at, None);
    }

    // ── Simple field updates ────────────────────────────────

    #[test]
    fn update_attempts() {
        let conn = open_memory().unwrap();
        insert_task(&conn, &make_task("T1")).unwrap();
        super::update_attempts(&conn, "T1", 7).unwrap();
        let t = get_task(&conn, "T1").unwrap().unwrap();
        assert_eq!(t.attempts, 7);
    }

    #[test]
    fn update_last_error_set_and_clear() {
        let conn = open_memory().unwrap();
        insert_task(&conn, &make_task("T1")).unwrap();

        update_last_error(&conn, "T1", Some("compile failed")).unwrap();
        let t = get_task(&conn, "T1").unwrap().unwrap();
        assert_eq!(t.last_error.as_deref(), Some("compile failed"));

        update_last_error(&conn, "T1", None).unwrap();
        let t = get_task(&conn, "T1").unwrap().unwrap();
        assert!(t.last_error.is_none());
    }

    #[test]
    fn update_files_changed() {
        let conn = open_memory().unwrap();
        insert_task(&conn, &make_task("T1")).unwrap();

        let files = vec![PathBuf::from("a.rs"), PathBuf::from("b.rs")];
        super::update_files_changed(&conn, "T1", &files).unwrap();
        let t = get_task(&conn, "T1").unwrap().unwrap();
        assert_eq!(t.files_changed, files);
    }

    #[test]
    fn push_feedback_appends_entries() {
        let conn = open_memory().unwrap();
        insert_task(&conn, &make_task("T1")).unwrap();

        push_feedback(&conn, "T1", "tester: failed").unwrap();
        push_feedback(&conn, "T1", "reviewer: nope").unwrap();

        let t = get_task(&conn, "T1").unwrap().unwrap();
        assert_eq!(t.feedback.len(), 2);
        assert_eq!(t.feedback[0], "tester: failed");
        assert_eq!(t.feedback[1], "reviewer: nope");
    }

    #[test]
    fn clear_feedback_empties_array() {
        let conn = open_memory().unwrap();
        insert_task(&conn, &make_task("T1")).unwrap();
        push_feedback(&conn, "T1", "some feedback").unwrap();

        clear_feedback(&conn, "T1").unwrap();
        let t = get_task(&conn, "T1").unwrap().unwrap();
        assert!(t.feedback.is_empty());
    }

    #[test]
    fn set_guidance_replaces_array() {
        let conn = open_memory().unwrap();
        insert_task(&conn, &make_task("T1")).unwrap();

        set_guidance(&conn, "T1", &["use bun".to_string(), "no mocks".to_string()]).unwrap();
        let t = get_task(&conn, "T1").unwrap().unwrap();
        assert_eq!(t.guidance, vec!["use bun", "no mocks"]);

        // Replace entirely
        set_guidance(&conn, "T1", &["new rule".to_string()]).unwrap();
        let t = get_task(&conn, "T1").unwrap().unwrap();
        assert_eq!(t.guidance, vec!["new rule"]);
    }

    #[test]
    fn update_postmortem_set_and_clear() {
        let conn = open_memory().unwrap();
        insert_task(&conn, &make_task("T1")).unwrap();

        update_postmortem(&conn, "T1", Some("it was a missing semicolon")).unwrap();
        let t = get_task(&conn, "T1").unwrap().unwrap();
        assert_eq!(t.postmortem.as_deref(), Some("it was a missing semicolon"));

        update_postmortem(&conn, "T1", None).unwrap();
        let t = get_task(&conn, "T1").unwrap().unwrap();
        assert!(t.postmortem.is_none());
    }

    #[test]
    fn archive_and_restore_task() {
        let conn = open_memory().unwrap();
        insert_task(&conn, &make_task("T1")).unwrap();

        archive_task(&conn, "T1").unwrap();
        let t = get_task(&conn, "T1").unwrap().unwrap();
        assert!(t.archived);

        // Not in active list
        let active = list_active_tasks(&conn).unwrap();
        assert!(active.is_empty());

        restore_task(&conn, "T1").unwrap();
        let t = get_task(&conn, "T1").unwrap().unwrap();
        assert!(!t.archived);

        let active = list_active_tasks(&conn).unwrap();
        assert_eq!(active.len(), 1);
    }

    // ── archive_done_tasks ──────────────────────────────────

    #[test]
    fn archive_done_tasks_archives_only_done_and_skipped() {
        let conn = open_memory().unwrap();

        let mut done = make_task("T1");
        done.phase = Phase::Done;
        insert_task(&conn, &done).unwrap();

        let mut skipped = make_task("T2");
        skipped.phase = Phase::Skipped;
        insert_task(&conn, &skipped).unwrap();

        let pending = make_task("T3"); // Phase::Pending by default
        insert_task(&conn, &pending).unwrap();

        let mut failed = make_task("T4");
        failed.phase = Phase::Failed;
        insert_task(&conn, &failed).unwrap();

        let count = archive_done_tasks(&conn).unwrap();
        assert_eq!(count, 2, "expected 2 tasks archived");

        assert!(get_task(&conn, "T1").unwrap().unwrap().archived, "Done task should be archived");
        assert!(get_task(&conn, "T2").unwrap().unwrap().archived, "Skipped task should be archived");
        assert!(!get_task(&conn, "T3").unwrap().unwrap().archived, "Pending task should not be archived");
        assert!(!get_task(&conn, "T4").unwrap().unwrap().archived, "Failed task should not be archived");
    }

    #[test]
    fn archive_done_tasks_skips_already_archived() {
        let conn = open_memory().unwrap();

        // Already-archived Done task — should not be double-counted.
        let mut already = make_task("T1");
        already.phase = Phase::Done;
        already.archived = true;
        insert_task(&conn, &already).unwrap();

        // Active Done task — should be archived.
        let mut active = make_task("T2");
        active.phase = Phase::Done;
        insert_task(&conn, &active).unwrap();

        let count = archive_done_tasks(&conn).unwrap();
        assert_eq!(count, 1);
        assert!(get_task(&conn, "T2").unwrap().unwrap().archived);
    }

    #[test]
    fn archive_done_tasks_returns_zero_when_nothing_to_archive() {
        let conn = open_memory().unwrap();
        insert_task(&conn, &make_task("T1")).unwrap(); // Pending
        let count = archive_done_tasks(&conn).unwrap();
        assert_eq!(count, 0);
    }

    // ── reset_all_failed ────────────────────────────────────

    #[test]
    fn reset_all_failed_resets_only_failed_tasks() {
        let conn = open_memory().unwrap();

        // Failed task with error, feedback, and guidance
        let mut failed = make_task("T1");
        failed.phase = Phase::Failed;
        failed.attempts = 3;
        failed.last_error = Some("compile error".to_string());
        failed.feedback = vec!["reviewer: nope".to_string()];
        failed.guidance = vec!["use bun".to_string()];
        insert_task(&conn, &failed).unwrap();

        // Another failed task
        let mut failed2 = make_task("T2");
        failed2.phase = Phase::Failed;
        failed2.attempts = 1;
        failed2.last_error = Some("test failed".to_string());
        insert_task(&conn, &failed2).unwrap();

        // Pending task — should not be touched
        let pending = make_task("T3");
        insert_task(&conn, &pending).unwrap();

        // Done task — should not be touched
        let mut done = make_task("T4");
        done.phase = Phase::Done;
        done.attempts = 2;
        insert_task(&conn, &done).unwrap();

        // Archived failed task — should not be touched
        let mut archived_failed = make_task("T5");
        archived_failed.phase = Phase::Failed;
        archived_failed.archived = true;
        archived_failed.attempts = 5;
        insert_task(&conn, &archived_failed).unwrap();

        let count = reset_all_failed(&conn).unwrap();
        assert_eq!(count, 2, "expected 2 failed tasks reset");

        // T1: reset to Pending with cleared error/feedback, guidance preserved
        let t1 = get_task(&conn, "T1").unwrap().unwrap();
        assert_eq!(t1.phase, Phase::Pending);
        assert_eq!(t1.attempts, 0);
        assert!(t1.last_error.is_none());
        assert!(t1.feedback.is_empty());
        assert_eq!(t1.guidance, vec!["use bun"], "guidance should be preserved");
        assert!(t1.phase_entered_at.is_some(), "phase_entered_at should be set");

        // T2: also reset
        let t2 = get_task(&conn, "T2").unwrap().unwrap();
        assert_eq!(t2.phase, Phase::Pending);
        assert_eq!(t2.attempts, 0);
        assert!(t2.last_error.is_none());

        // T3: Pending unchanged
        let t3 = get_task(&conn, "T3").unwrap().unwrap();
        assert_eq!(t3.phase, Phase::Pending);
        assert_eq!(t3.attempts, 0);

        // T4: Done unchanged
        let t4 = get_task(&conn, "T4").unwrap().unwrap();
        assert_eq!(t4.phase, Phase::Done);
        assert_eq!(t4.attempts, 2);

        // T5: archived Failed unchanged
        let t5 = get_task(&conn, "T5").unwrap().unwrap();
        assert_eq!(t5.phase, Phase::Failed);
        assert_eq!(t5.attempts, 5);
    }

    #[test]
    fn reset_all_failed_returns_zero_when_no_failed_tasks() {
        let conn = open_memory().unwrap();
        insert_task(&conn, &make_task("T1")).unwrap();
        let count = reset_all_failed(&conn).unwrap();
        assert_eq!(count, 0);
    }

    // ── Dependency queries ──────────────────────────────────

    #[test]
    fn task_ids_in_use_returns_all() {
        let conn = open_memory().unwrap();
        insert_task(&conn, &make_task("A")).unwrap();
        let mut b = make_task("B");
        b.archived = true;
        insert_task(&conn, &b).unwrap();

        let ids = task_ids_in_use(&conn).unwrap();
        assert!(ids.contains("A"));
        assert!(ids.contains("B"));
        assert_eq!(ids.len(), 2);
    }

    #[test]
    fn validate_deps_passes_when_all_valid() {
        let conn = open_memory().unwrap();
        insert_task(&conn, &make_task("A")).unwrap();
        let mut b = make_task("B");
        b.blocked_by = vec!["A".to_string()];
        insert_task(&conn, &b).unwrap();

        assert!(validate_deps(&conn).is_ok());
    }

    #[test]
    fn validate_deps_passes_for_archived_dep() {
        let conn = open_memory().unwrap();
        let mut a = make_task("A");
        a.archived = true;
        insert_task(&conn, &a).unwrap();
        let mut b = make_task("B");
        b.blocked_by = vec!["A".to_string()];
        insert_task(&conn, &b).unwrap();

        assert!(validate_deps(&conn).is_ok());
    }

    #[test]
    fn validate_deps_catches_dangling_reference() {
        let conn = open_memory().unwrap();
        // Insert T1 with a dep on a non-existent task.
        // Since dep_id has no FK constraint, this insert succeeds.
        let mut t = make_task("T1");
        t.blocked_by = vec!["GHOST".to_string()];
        insert_task(&conn, &t).unwrap();

        let err = validate_deps(&conn).unwrap_err();
        assert!(
            err.to_string().contains("GHOST"),
            "error should mention the dangling dep: {err}"
        );
    }

    #[test]
    fn validate_deps_catches_cycle() {
        let conn = open_memory().unwrap();
        let mut a = make_task("A");
        a.blocked_by = vec!["B".to_string()];
        insert_task(&conn, &a).unwrap();
        let mut b = make_task("B");
        b.blocked_by = vec!["A".to_string()];
        insert_task(&conn, &b).unwrap();

        let err = validate_deps(&conn).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("A") && msg.contains("B"),
            "error should name the cycle participants: {err}"
        );
        assert!(msg.contains("cycle"), "error should mention 'cycle': {err}");
    }

    // ── Nit CRUD ────────────────────────────────────────────

    #[test]
    fn insert_and_list_nits() {
        let conn = open_memory().unwrap();
        let nit = Nit {
            id: "NIT-1".to_string(),
            source_task: "T1".to_string(),
            source_role: "reviewer".to_string(),
            attempt: 1,
            content: "missing docs".to_string(),
            summary: "Add docs".to_string(),
            status: NitStatus::Open,
            promoted_to: None,
            created_at: 5000,
        };
        insert_nit(&conn, &nit).unwrap();

        let nits = list_nits(&conn, true).unwrap();
        assert_eq!(nits.len(), 1);
        assert_eq!(nits[0].id, "NIT-1");
        assert_eq!(nits[0].source_task, "T1");
        assert_eq!(nits[0].content, "missing docs");
        assert_eq!(nits[0].summary, "Add docs");
        assert_eq!(nits[0].status, NitStatus::Open);
        assert_eq!(nits[0].created_at, 5000);
    }

    #[test]
    fn list_nits_open_only_filters_non_open() {
        let conn = open_memory().unwrap();
        insert_nit(&conn, &make_nit("NIT-1", NitStatus::Open)).unwrap();
        insert_nit(&conn, &make_nit("NIT-2", NitStatus::Dismissed)).unwrap();
        insert_nit(&conn, &make_nit("NIT-3", NitStatus::Promoted)).unwrap();

        let open_only = list_nits(&conn, false).unwrap();
        assert_eq!(open_only.len(), 1);
        assert_eq!(open_only[0].id, "NIT-1");
    }

    #[test]
    fn list_nits_include_all_returns_all() {
        let conn = open_memory().unwrap();
        insert_nit(&conn, &make_nit("NIT-1", NitStatus::Open)).unwrap();
        insert_nit(&conn, &make_nit("NIT-2", NitStatus::Dismissed)).unwrap();
        insert_nit(&conn, &make_nit("NIT-3", NitStatus::Promoted)).unwrap();

        let all = list_nits(&conn, true).unwrap();
        assert_eq!(all.len(), 3);
    }

    #[test]
    fn update_nit_status_promote() {
        let conn = open_memory().unwrap();
        insert_nit(&conn, &make_nit("NIT-1", NitStatus::Open)).unwrap();

        update_nit_status(&conn, "NIT-1", NitStatus::Promoted, Some("T2")).unwrap();

        let nits = list_nits(&conn, true).unwrap();
        assert_eq!(nits[0].status, NitStatus::Promoted);
        assert_eq!(nits[0].promoted_to.as_deref(), Some("T2"));
    }

    #[test]
    fn update_nit_status_dismiss() {
        let conn = open_memory().unwrap();
        insert_nit(&conn, &make_nit("NIT-1", NitStatus::Open)).unwrap();

        update_nit_status(&conn, "NIT-1", NitStatus::Dismissed, None).unwrap();

        let nits = list_nits(&conn, true).unwrap();
        assert_eq!(nits[0].status, NitStatus::Dismissed);
        assert!(nits[0].promoted_to.is_none());
    }

    #[test]
    fn next_nit_id_empty_table() {
        let conn = open_memory().unwrap();
        assert_eq!(next_nit_id(&conn).unwrap(), "NIT-1");
    }

    #[test]
    fn next_nit_id_increments_past_max() {
        let conn = open_memory().unwrap();
        insert_nit(&conn, &make_nit("NIT-1", NitStatus::Open)).unwrap();
        insert_nit(&conn, &make_nit("NIT-3", NitStatus::Open)).unwrap();

        assert_eq!(next_nit_id(&conn).unwrap(), "NIT-4");
    }

    #[test]
    fn insert_nit_upsert_updates_existing() {
        let conn = open_memory().unwrap();
        insert_nit(&conn, &make_nit("NIT-1", NitStatus::Open)).unwrap();

        let mut updated = make_nit("NIT-1", NitStatus::Dismissed);
        updated.content = "updated content".to_string();
        insert_nit(&conn, &updated).unwrap();

        let nits = list_nits(&conn, true).unwrap();
        assert_eq!(nits.len(), 1);
        assert_eq!(nits[0].status, NitStatus::Dismissed);
        assert_eq!(nits[0].content, "updated content");
    }

    #[test]
    fn get_nit_returns_correct_fields() {
        let conn = open_memory().unwrap();
        let nit = Nit {
            id: "NIT-1".to_string(),
            source_task: "T1".to_string(),
            source_role: "reviewer".to_string(),
            attempt: 2,
            content: "fix the docs".to_string(),
            summary: "Doc fix".to_string(),
            status: NitStatus::Open,
            promoted_to: None,
            created_at: 7777,
        };
        insert_nit(&conn, &nit).unwrap();

        let got = get_nit(&conn, "NIT-1").unwrap().unwrap();
        assert_eq!(got.id, "NIT-1");
        assert_eq!(got.source_task, "T1");
        assert_eq!(got.source_role, "reviewer");
        assert_eq!(got.attempt, 2);
        assert_eq!(got.content, "fix the docs");
        assert_eq!(got.summary, "Doc fix");
        assert_eq!(got.status, NitStatus::Open);
        assert!(got.promoted_to.is_none());
        assert_eq!(got.created_at, 7777);
    }

    #[test]
    fn get_nit_missing_id_returns_none() {
        let conn = open_memory().unwrap();
        let result = get_nit(&conn, "NIT-999").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn nit_status_counts_returns_correct_counts() {
        let conn = open_memory().unwrap();

        // Empty table — all counts should be 0.
        let counts = nit_status_counts(&conn).unwrap();
        assert_eq!(counts.open, 0);
        assert_eq!(counts.promoted, 0);
        assert_eq!(counts.dismissed, 0);

        insert_nit(&conn, &make_nit("NIT-1", NitStatus::Open)).unwrap();
        insert_nit(&conn, &make_nit("NIT-2", NitStatus::Open)).unwrap();
        insert_nit(&conn, &make_nit("NIT-3", NitStatus::Promoted)).unwrap();
        // No Dismissed nits inserted — dismissed should remain 0.

        let counts = nit_status_counts(&conn).unwrap();
        assert_eq!(counts.open, 2);
        assert_eq!(counts.promoted, 1);
        assert_eq!(counts.dismissed, 0);
    }

    // ── ID helpers ──────────────────────────────────────────

    #[test]
    fn id_ranges_summary_empty_table() {
        let conn = open_memory().unwrap();
        let summary = id_ranges_summary(&conn).unwrap();
        assert_eq!(summary, "");
    }

    #[test]
    fn id_ranges_summary_with_prefixed_ids() {
        let conn = open_memory().unwrap();
        let mut t1 = make_task("GEN-1");
        t1.priority = 1;
        let mut t2 = make_task("GEN-2");
        t2.priority = 2;
        insert_tasks(&conn, &[t1, t2]).unwrap();

        let summary = id_ranges_summary(&conn).unwrap();
        assert!(
            summary.contains("GEN: 1 through 2 (next available: GEN-3)"),
            "unexpected summary: {summary}"
        );
    }

    #[test]
    fn id_ranges_summary_includes_archived() {
        let conn = open_memory().unwrap();
        let mut t = make_task("SQL-1");
        t.archived = true;
        insert_task(&conn, &t).unwrap();

        let summary = id_ranges_summary(&conn).unwrap();
        assert!(summary.contains("SQL"), "archived IDs should appear: {summary}");
    }

    #[test]
    fn next_generated_id_empty_table() {
        let conn = open_memory().unwrap();
        assert_eq!(next_generated_id(&conn).unwrap(), "GEN-1");
    }

    #[test]
    fn next_generated_id_increments() {
        let conn = open_memory().unwrap();
        insert_task(&conn, &make_task("GEN-5")).unwrap();
        insert_task(&conn, &make_task("GEN-2")).unwrap();

        assert_eq!(next_generated_id(&conn).unwrap(), "GEN-6");
    }

    #[test]
    fn next_generated_id_ignores_other_prefixes() {
        let conn = open_memory().unwrap();
        insert_task(&conn, &make_task("SQL-10")).unwrap();

        assert_eq!(next_generated_id(&conn).unwrap(), "GEN-1");
    }

    // ── All phases roundtrip ────────────────────────────────

    #[test]
    fn all_phases_roundtrip_through_db() {
        let conn = open_memory().unwrap();
        let phases = [
            Phase::Pending,
            Phase::Implementing,
            Phase::Testing,
            Phase::Reviewing,
            Phase::Done,
            Phase::Failed,
            Phase::Skipped,
        ];
        for (i, phase) in phases.iter().enumerate() {
            let mut task = make_task(&format!("T{i}"));
            task.phase = *phase;
            insert_task(&conn, &task).unwrap();
            let got = get_task(&conn, &format!("T{i}")).unwrap().unwrap();
            assert_eq!(got.phase, *phase);
        }
    }
}
