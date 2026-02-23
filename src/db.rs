use std::path::{Path, PathBuf};

use anyhow::Result;
use rusqlite::Connection;

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

        CREATE TABLE IF NOT EXISTS directives (
            id      INTEGER PRIMARY KEY AUTOINCREMENT,
            task_id TEXT NOT NULL,
            action  TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS nits (
            id          TEXT    PRIMARY KEY,
            source_task TEXT    NOT NULL,
            source_role TEXT    NOT NULL,
            attempt     INTEGER NOT NULL DEFAULT 0,
            content     TEXT    NOT NULL,
            summary     TEXT    NOT NULL DEFAULT '',
            status      TEXT    NOT NULL DEFAULT 'open',
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_memory_succeeds_and_schema_version_is_1() {
        let conn = open_memory().expect("open_memory failed");

        // Verify all five tables exist by querying sqlite_master.
        let table_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name IN \
                 ('tasks','task_deps','directives','nits','meta')",
                [],
                |row| row.get(0),
            )
            .expect("sqlite_master query failed");
        assert_eq!(table_count, 5, "expected 5 tables, found {table_count}");

        // Verify schema_version is '1'.
        let version: String = conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'schema_version'",
                [],
                |row| row.get(0),
            )
            .expect("schema_version not found");
        assert_eq!(version, "1");
    }
}
