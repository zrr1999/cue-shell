//! SQLite persistence layer for cued.
//!
//! Uses WAL mode for concurrent reads.  The schema is migrated on open.

use std::path::Path;

use anyhow::{Context, Result};
use cue_core::ScopeHash;
use cue_core::scope::Scope;
use rusqlite::Connection;

// ── Schema migration ──

/// Current schema version (bump when adding migrations).
const SCHEMA_VERSION: u32 = 1;

const MIGRATION_V1: &str = r"
CREATE TABLE IF NOT EXISTS scopes (
    hash        BLOB PRIMARY KEY,   -- 32-byte blake3
    parent      BLOB,               -- nullable FK → scopes.hash
    delta_json  TEXT,                -- JSON-encoded EnvDelta (NULL for root)
    snap_json   TEXT                 -- JSON-encoded EnvSnapshot
);

CREATE TABLE IF NOT EXISTS scope_head (
    id   INTEGER PRIMARY KEY CHECK (id = 0),
    hash BLOB NOT NULL
);

CREATE TABLE IF NOT EXISTS crons (
    id          TEXT PRIMARY KEY,    -- e.g. 'C1'
    schedule    TEXT NOT NULL,
    command     TEXT NOT NULL,
    enabled     INTEGER NOT NULL DEFAULT 1,
    created_at  TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE IF NOT EXISTS jobs_history (
    id          TEXT PRIMARY KEY,    -- e.g. 'J1'
    pipeline    TEXT NOT NULL,
    status      TEXT NOT NULL,
    exit_code   INTEGER,
    scope_hash  BLOB,
    created_at  TEXT NOT NULL DEFAULT (datetime('now')),
    finished_at TEXT
);

CREATE TABLE IF NOT EXISTS agents_history (
    id          TEXT PRIMARY KEY,    -- e.g. 'A1'
    kind        TEXT NOT NULL,
    role        TEXT NOT NULL,
    status      TEXT NOT NULL,
    created_at  TEXT NOT NULL DEFAULT (datetime('now')),
    finished_at TEXT
);

CREATE TABLE IF NOT EXISTS config_cache (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
";

/// Open (or create) the database at `path`, apply WAL mode and run migrations.
pub fn open_db(path: &Path) -> Result<Connection> {
    let conn = Connection::open(path)
        .with_context(|| format!("failed to open database at {}", path.display()))?;

    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;

    migrate(&conn)?;
    Ok(conn)
}

fn migrate(conn: &Connection) -> Result<()> {
    let current: u32 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if current < SCHEMA_VERSION {
        conn.execute_batch(MIGRATION_V1)
            .context("failed to run schema migration v1")?;
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    }
    Ok(())
}

// ── Scope CRUD ──

/// Insert a scope (cache + persistence).
pub fn insert_scope(conn: &Connection, scope: &Scope) -> Result<()> {
    let hash_bytes = scope.hash.0.as_slice();
    let parent_bytes = scope.parent.map(|p| p.0.to_vec());
    let delta_json = scope
        .delta
        .as_ref()
        .map(serde_json::to_string)
        .transpose()?;
    let snap_json = scope
        .snapshot
        .as_ref()
        .map(serde_json::to_string)
        .transpose()?;

    conn.execute(
        "INSERT OR IGNORE INTO scopes (hash, parent, delta_json, snap_json) VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![hash_bytes, parent_bytes, delta_json, snap_json],
    )?;
    Ok(())
}

/// Retrieve a scope by hash.
pub fn get_scope(conn: &Connection, hash: &ScopeHash) -> Result<Option<Scope>> {
    let mut stmt =
        conn.prepare("SELECT hash, parent, delta_json, snap_json FROM scopes WHERE hash = ?1")?;

    let result = stmt
        .query_row(rusqlite::params![hash.0.as_slice()], |row| {
            let hash_blob: Vec<u8> = row.get(0)?;
            let parent_blob: Option<Vec<u8>> = row.get(1)?;
            let delta_json: Option<String> = row.get(2)?;
            let snap_json: Option<String> = row.get(3)?;
            Ok((hash_blob, parent_blob, delta_json, snap_json))
        })
        .optional()?;

    let Some((hash_blob, parent_blob, delta_json, snap_json)) = result else {
        return Ok(None);
    };

    let hash = blob_to_scope_hash(&hash_blob)?;
    let parent = parent_blob.as_deref().map(blob_to_scope_hash).transpose()?;
    let delta = delta_json
        .map(|j| serde_json::from_str(&j))
        .transpose()
        .context("corrupt delta_json")?;
    let snapshot = snap_json
        .map(|j| serde_json::from_str(&j))
        .transpose()
        .context("corrupt snap_json")?;

    Ok(Some(Scope {
        hash,
        parent,
        delta,
        snapshot,
    }))
}

/// Get the current HEAD scope hash.
pub fn get_head(conn: &Connection) -> Result<Option<ScopeHash>> {
    let mut stmt = conn.prepare("SELECT hash FROM scope_head WHERE id = 0")?;
    let result = stmt
        .query_row([], |row| {
            let blob: Vec<u8> = row.get(0)?;
            Ok(blob)
        })
        .optional()?;

    match result {
        Some(blob) => Ok(Some(blob_to_scope_hash(&blob)?)),
        None => Ok(None),
    }
}

/// Set (or create) the HEAD pointer.
pub fn set_head(conn: &Connection, hash: &ScopeHash) -> Result<()> {
    conn.execute(
        "INSERT INTO scope_head (id, hash) VALUES (0, ?1)
         ON CONFLICT(id) DO UPDATE SET hash = excluded.hash",
        rusqlite::params![hash.0.as_slice()],
    )?;
    Ok(())
}

// ── Helpers ──

fn blob_to_scope_hash(blob: &[u8]) -> Result<ScopeHash> {
    let arr: [u8; 32] = blob
        .try_into()
        .map_err(|_| anyhow::anyhow!("scope hash blob is not 32 bytes (got {})", blob.len()))?;
    Ok(ScopeHash(arr))
}

/// Extension trait on `rusqlite::Statement` to get optional results.
trait OptionalExt<T> {
    fn optional(self) -> rusqlite::Result<Option<T>>;
}

impl<T> OptionalExt<T> for rusqlite::Result<T> {
    fn optional(self) -> rusqlite::Result<Option<T>> {
        match self {
            Ok(v) => Ok(Some(v)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    use cue_core::scope::EnvSnapshot;

    use super::*;

    fn in_memory_db() -> Connection {
        open_db(Path::new(":memory:")).expect("open in-memory db")
    }

    #[test]
    fn migration_is_idempotent() {
        let conn = in_memory_db();
        // Running migrate again should be a no-op.
        migrate(&conn).expect("second migration");
    }

    #[test]
    fn scope_roundtrip() {
        let conn = in_memory_db();
        let snap = EnvSnapshot {
            env: BTreeMap::from([("PATH".into(), "/usr/bin".into())]),
            cwd: PathBuf::from("/tmp"),
        };
        let scope = Scope::root(snap);
        insert_scope(&conn, &scope).unwrap();
        let loaded = get_scope(&conn, &scope.hash)
            .unwrap()
            .expect("scope exists");
        assert_eq!(loaded.hash, scope.hash);
        assert!(loaded.parent.is_none());
        assert!(loaded.snapshot.is_some());
    }

    #[test]
    fn head_roundtrip() {
        let conn = in_memory_db();
        assert!(get_head(&conn).unwrap().is_none());
        let hash = ScopeHash([42; 32]);
        set_head(&conn, &hash).unwrap();
        assert_eq!(get_head(&conn).unwrap(), Some(hash));

        // Update head.
        let hash2 = ScopeHash([99; 32]);
        set_head(&conn, &hash2).unwrap();
        assert_eq!(get_head(&conn).unwrap(), Some(hash2));
    }
}
