//! SQLite schema and migrations.
//!
//! Raw facts are persisted; derived metrics are recomputed on load. That keeps the schema small and
//! means changing a metric definition needs no migration — which matters, since the metric
//! definitions are the part most likely to change.

use anyhow::{Context, Result};
use rusqlite::Connection;

/// Bumped whenever the DDL below changes. `user_version` is compared against it on open.
pub const SCHEMA_VERSION: i64 = 1;

const DDL: &str = r#"
CREATE TABLE IF NOT EXISTS repos (
    id            INTEGER PRIMARY KEY,
    owner         TEXT NOT NULL,
    name          TEXT NOT NULL,
    UNIQUE(owner, name)
);

CREATE TABLE IF NOT EXISTS prs (
    repo_id           INTEGER NOT NULL REFERENCES repos(id) ON DELETE CASCADE,
    number            INTEGER NOT NULL,
    title             TEXT NOT NULL,
    author            TEXT NOT NULL,
    state             TEXT NOT NULL,
    is_draft          INTEGER NOT NULL,
    base_ref          TEXT NOT NULL,
    head_ref          TEXT NOT NULL,
    created_at        TEXT NOT NULL,
    updated_at        TEXT NOT NULL,
    ready_at          TEXT,
    merged_at         TEXT,
    closed_at         TEXT,
    merge_commit_oid  TEXT,
    additions         INTEGER NOT NULL,
    deletions         INTEGER NOT NULL,
    changed_files     INTEGER NOT NULL,
    truncated         INTEGER NOT NULL,
    PRIMARY KEY (repo_id, number)
);
CREATE INDEX IF NOT EXISTS prs_merged  ON prs(repo_id, merged_at);
CREATE INDEX IF NOT EXISTS prs_author  ON prs(repo_id, author, state);

CREATE TABLE IF NOT EXISTS pr_labels (
    repo_id  INTEGER NOT NULL,
    number   INTEGER NOT NULL,
    label    TEXT NOT NULL,
    PRIMARY KEY (repo_id, number, label)
);

-- Only human reviews are stored; bot filtering happens before persistence so the cache can't
-- reintroduce bot latency into the metrics.
CREATE TABLE IF NOT EXISTS reviews (
    repo_id       INTEGER NOT NULL,
    number        INTEGER NOT NULL,
    author        TEXT NOT NULL,
    state         TEXT NOT NULL,
    submitted_at  TEXT NOT NULL,
    PRIMARY KEY (repo_id, number, author, submitted_at, state)
);

CREATE TABLE IF NOT EXISTS deploy_tags (
    repo_id      INTEGER NOT NULL,
    name         TEXT NOT NULL,
    env          TEXT NOT NULL,
    component    TEXT,
    -- Deploy time, from the tag name. See domain::trunk::TrunkTag for why not the commit date.
    deployed_at  TEXT NOT NULL,
    commit_oid   TEXT NOT NULL,
    commit_date  TEXT NOT NULL,
    PRIMARY KEY (repo_id, name)
);
CREATE INDEX IF NOT EXISTS tags_env ON deploy_tags(repo_id, env, deployed_at);

-- Every fetched ancestor with its first parent, so the trunk can be rebuilt from cache without
-- refetching. Off-trunk commits are kept deliberately: a later refresh may put them on the trunk.
CREATE TABLE IF NOT EXISTS commits (
    repo_id          INTEGER NOT NULL,
    oid              TEXT NOT NULL,
    committed_date   TEXT NOT NULL,
    first_parent_oid TEXT,
    parent_count     INTEGER NOT NULL,
    PRIMARY KEY (repo_id, oid)
);

CREATE TABLE IF NOT EXISTS sync_state (
    repo_id  INTEGER NOT NULL,
    key      TEXT NOT NULL,
    value    TEXT NOT NULL,
    PRIMARY KEY (repo_id, key)
);
"#;

/// Open (or create) the database and bring the schema up to date.
pub fn open(path: &std::path::Path) -> Result<Connection> {
    let conn = Connection::open(path)
        .with_context(|| format!("opening database at {}", path.display()))?;

    conn.pragma_update(None, "journal_mode", "WAL")
        .context("enabling WAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")
        .context("enabling foreign keys")?;

    migrate(&conn)?;
    Ok(conn)
}

/// Apply the schema, recreating it from scratch if the on-disk version is older.
///
/// The cache is disposable — everything in it can be refetched — so an old version is dropped
/// rather than migrated. That is a deliberate trade: it avoids carrying migration code for data
/// that costs about thirty API calls to rebuild.
fn migrate(conn: &Connection) -> Result<()> {
    let version: i64 = conn
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .context("reading user_version")?;

    if version != 0 && version != SCHEMA_VERSION {
        for table in [
            "sync_state", "commits", "deploy_tags", "reviews", "pr_labels", "prs", "repos",
        ] {
            conn.execute(&format!("DROP TABLE IF EXISTS {table}"), [])
                .with_context(|| format!("dropping stale table {table}"))?;
        }
    }

    conn.execute_batch(DDL).context("applying schema")?;
    conn.pragma_update(None, "user_version", SCHEMA_VERSION)
        .context("stamping user_version")?;
    Ok(())
}

/// Test-only hook so store tests can build an in-memory database.
#[cfg(test)]
pub fn migrate_for_tests(conn: &Connection) -> Result<()> {
    migrate(conn)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_schema_in_a_fresh_database() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        let v: i64 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(v, SCHEMA_VERSION);
        // Table exists and is queryable.
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM prs", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn migrate_is_idempotent() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        migrate(&conn).unwrap();
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM repos", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0);
    }

    /// A cache written by any other schema version is dropped rather than migrated, and must not
    /// leave rows behind. Version 0 is excluded because that is the fresh-database sentinel: a
    /// brand new file reports 0 and must be created, not rebuilt.
    #[test]
    fn mismatched_schema_version_is_rebuilt() {
        for other in [SCHEMA_VERSION + 1, 99] {
            let conn = Connection::open_in_memory().unwrap();
            migrate(&conn).unwrap();
            conn.execute("INSERT INTO repos (owner, name) VALUES ('a','b')", [])
                .unwrap();
            conn.pragma_update(None, "user_version", other).unwrap();

            migrate(&conn).unwrap();
            let n: i64 = conn
                .query_row("SELECT COUNT(*) FROM repos", [], |r| r.get(0))
                .unwrap();
            assert_eq!(n, 0, "rows from version {other} must be cleared");
            let v: i64 = conn
                .pragma_query_value(None, "user_version", |r| r.get(0))
                .unwrap();
            assert_eq!(v, SCHEMA_VERSION);
        }
    }

    /// A fresh file reports user_version 0 and must simply be created.
    #[test]
    fn fresh_database_is_not_treated_as_stale() {
        let conn = Connection::open_in_memory().unwrap();
        let v: i64 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(v, 0, "a new database starts at 0");
        migrate(&conn).unwrap();
        conn.execute("INSERT INTO repos (owner, name) VALUES ('a','b')", [])
            .unwrap();
        // Re-running must preserve data, since the version now matches.
        migrate(&conn).unwrap();
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM repos", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1, "matching version must not clear data");
    }
}
