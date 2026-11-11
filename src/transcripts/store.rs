//! `transcripts.sqlite`: how far each transcript has been read, and what was extracted from it.

use std::path::Path;

use anyhow::Context;
use rusqlite::{Connection, OptionalExtension, params};

use super::{Call, Kind};

/// Bumped when parsing or the schema changes, so every transcript is read again.
const PARSER_VERSION: &str = "2";

const META: &str = "CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);";

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS files (
    path TEXT PRIMARY KEY, size INTEGER NOT NULL, mtime INTEGER NOT NULL,
    offset INTEGER NOT NULL, head_len INTEGER NOT NULL, head BLOB NOT NULL,
    session TEXT, cwd TEXT);
CREATE TABLE IF NOT EXISTS sessions (
    id TEXT PRIMARY KEY, harness TEXT NOT NULL, repo TEXT, started INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS calls (
    file TEXT NOT NULL, id TEXT NOT NULL, session TEXT NOT NULL, repo TEXT, kind TEXT NOT NULL,
    ts INTEGER NOT NULL, query TEXT, path TEXT, done_ts INTEGER, results TEXT,
    is_error INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (file, id));
CREATE INDEX IF NOT EXISTS calls_session ON calls (session);
";

/// How far a transcript was read. `head` hashes its first `head_len` bytes, to notice a rewrite.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FileState {
    pub size: u64,
    pub mtime: i64,
    pub offset: u64,
    pub head_len: u64,
    pub head: Vec<u8>,
    pub session: Option<String>,
    pub cwd: Option<String>,
}

pub struct SessionRow {
    pub id: String,
    pub harness: String,
    /// The registered repository its working directory is in, if any.
    pub repo: Option<String>,
    pub started: i64,
}

pub struct CallRow {
    pub session: String,
    /// The repository it worked in; unknown only for some lodestone searches.
    pub repo: Option<String>,
    pub kind: Kind,
    pub ts: i64,
    pub query: Option<String>,
    pub path: Option<String>,
    pub done_ts: Option<i64>,
    pub results: Vec<String>,
    /// The search failed (an error, or denied by the harness).
    pub is_error: bool,
}

pub struct Store {
    conn: Connection,
}

impl Store {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        conn.execute_batch(META)?;
        let version: Option<String> = conn
            .query_row("SELECT value FROM meta WHERE key = 'parser'", [], |r| {
                r.get(0)
            })
            .optional()?;
        if version.as_deref() != Some(PARSER_VERSION) {
            // Read everything again, into the current schema.
            conn.execute_batch(
                "DROP TABLE IF EXISTS files; DROP TABLE IF EXISTS sessions; DROP TABLE IF EXISTS calls;",
            )?;
            conn.execute(
                "INSERT OR REPLACE INTO meta (key, value) VALUES ('parser', ?1)",
                [PARSER_VERSION],
            )?;
        }
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn })
    }

    /// Forgets everything read when the registered repositories changed (`repos`), since calls
    /// in repositories that were not registered were not kept.
    pub fn reset_unless_registered(&self, repos: &str) -> anyhow::Result<()> {
        let seen: Option<String> = self
            .conn
            .query_row("SELECT value FROM meta WHERE key = 'repos'", [], |r| {
                r.get(0)
            })
            .optional()?;
        if seen.as_deref() != Some(repos) {
            self.conn
                .execute_batch("DELETE FROM files; DELETE FROM sessions; DELETE FROM calls;")?;
            self.conn.execute(
                "INSERT OR REPLACE INTO meta (key, value) VALUES ('repos', ?1)",
                [repos],
            )?;
        }
        Ok(())
    }

    pub fn begin(&self) -> anyhow::Result<()> {
        Ok(self.conn.execute_batch("BEGIN")?)
    }

    pub fn commit(&self) -> anyhow::Result<()> {
        Ok(self.conn.execute_batch("COMMIT")?)
    }

    pub fn file(&self, path: &str) -> anyhow::Result<Option<FileState>> {
        Ok(self
            .conn
            .query_row(
                "SELECT size, mtime, offset, head_len, head, session, cwd FROM files WHERE path = ?1",
                [path],
                |r| {
                    Ok(FileState {
                        size: r.get::<_, i64>(0)? as u64,
                        mtime: r.get(1)?,
                        offset: r.get::<_, i64>(2)? as u64,
                        head_len: r.get::<_, i64>(3)? as u64,
                        head: r.get(4)?,
                        session: r.get(5)?,
                        cwd: r.get(6)?,
                    })
                },
            )
            .optional()?)
    }

    pub fn save_file(&self, path: &str, s: &FileState) -> anyhow::Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO files (path, size, mtime, offset, head_len, head, session, cwd)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                path,
                s.size as i64,
                s.mtime,
                s.offset as i64,
                s.head_len as i64,
                s.head,
                s.session,
                s.cwd
            ],
        )?;
        Ok(())
    }

    /// Drops what was extracted from a transcript that was rewritten.
    pub fn forget_calls(&self, path: &str) -> anyhow::Result<()> {
        self.conn
            .execute("DELETE FROM calls WHERE file = ?1", [path])?;
        Ok(())
    }

    /// Records a session; a known session keeps its earliest start and first known repository.
    pub fn add_session(
        &self,
        id: &str,
        harness: &str,
        repo: Option<&str>,
        ts: i64,
    ) -> anyhow::Result<()> {
        self.conn.execute(
            "INSERT INTO sessions (id, harness, repo, started) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (id) DO UPDATE SET started = min(started, excluded.started),
                 repo = coalesce(repo, excluded.repo)",
            params![id, harness, repo, ts],
        )?;
        Ok(())
    }

    pub fn add_call(
        &self,
        file: &str,
        session: &str,
        repo: Option<&str>,
        c: &Call,
    ) -> anyhow::Result<()> {
        let (done_ts, results, is_error) = match &c.done {
            Some((ts, text, err)) if c.kind.is_lodestone() => {
                (Some(*ts), Some(result_paths(text).join("\n")), *err)
            }
            _ => (None, None, false),
        };
        self.conn.execute(
            "INSERT OR IGNORE INTO calls
                 (file, id, session, repo, kind, ts, query, path, done_ts, results, is_error)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                file,
                c.id,
                session,
                repo,
                c.kind.as_str(),
                c.ts,
                c.query,
                c.path,
                done_ts,
                results,
                is_error
            ],
        )?;
        Ok(())
    }

    /// Records a lodestone search's result; other calls' results are not kept.
    pub fn finish_call(
        &self,
        file: &str,
        id: &str,
        ts: i64,
        text: &str,
        is_error: bool,
    ) -> anyhow::Result<()> {
        self.conn.execute(
            "UPDATE calls SET done_ts = ?3, results = ?4, is_error = ?5
             WHERE file = ?1 AND id = ?2 AND done_ts IS NULL AND kind IN ('semantic', 'similar', 'search')",
            params![file, id, ts, result_paths(text).join("\n"), is_error],
        )?;
        Ok(())
    }

    pub fn sessions(&self) -> anyhow::Result<Vec<SessionRow>> {
        let mut st = self
            .conn
            .prepare("SELECT id, harness, repo, started FROM sessions")?;
        let rows = st.query_map([], |r| {
            Ok(SessionRow {
                id: r.get(0)?,
                harness: r.get(1)?,
                repo: r.get(2)?,
                started: r.get(3)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn calls(&self) -> anyhow::Result<Vec<CallRow>> {
        let mut st = self.conn.prepare(
            "SELECT session, repo, kind, ts, query, path, done_ts, results, is_error FROM calls ORDER BY ts",
        )?;
        let rows = st.query_map([], |r| {
            let kind: String = r.get(2)?;
            let results: Option<String> = r.get(7)?;
            Ok((
                kind,
                r.get(0)?,
                r.get(1)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
                results,
                r.get::<_, bool>(8)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (kind, session, repo, ts, query, path, done_ts, results, is_error) = row?;
            let Some(kind) = Kind::parse(&kind) else {
                continue;
            };
            out.push(CallRow {
                session,
                repo,
                kind,
                ts,
                query,
                path,
                done_ts,
                results: results
                    .map(|s: String| s.lines().map(String::from).collect())
                    .unwrap_or_default(),
                is_error,
            });
        }
        Ok(out)
    }
}

/// The paths of a lodestone result: lines shaped `path:line:column  …`.
fn result_paths(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|l| {
            let loc = l.split_whitespace().next()?;
            let mut parts = loc.rsplitn(3, ':');
            let (col, line, path) = (parts.next()?, parts.next()?, parts.next()?);
            let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
            (digits(col) && digits(line) && !path.is_empty()).then(|| path.to_string())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn result_paths_from_lodestone_text() {
        let text = "a/b.rs:25:12  lines 25-29  pub fn x()\n# note: something\nc.clar:3:1  lines 3-4  (define-public (y))\n";
        assert_eq!(result_paths(text), ["a/b.rs", "c.clar"]);
    }
}
