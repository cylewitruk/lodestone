//! Global content-addressed embedding cache (`cache.sqlite`), shared by every worktree.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Context;
use rusqlite::{Connection, OptionalExtension, params};

pub type CacheKey = [u8; 32];

/// `blake3(model_id ‖ chunk_text)`. Nothing else may go into the key: embeddings are a pure
/// function of (model, text).
pub fn cache_key(model_id: &str, text: &str) -> CacheKey {
    let mut h = blake3::Hasher::new();
    h.update(model_id.as_bytes());
    h.update(text.as_bytes());
    *h.finalize().as_bytes()
}

pub struct EmbeddingCache {
    conn: Mutex<Connection>,
}

impl EmbeddingCache {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        conn.busy_timeout(std::time::Duration::from_secs(30))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS embeddings (
               key BLOB PRIMARY KEY, model_id TEXT NOT NULL, dim INTEGER NOT NULL,
               vector BLOB NOT NULL, created INTEGER NOT NULL);
             CREATE INDEX IF NOT EXISTS embeddings_created ON embeddings(created);
             CREATE TABLE IF NOT EXISTS models (model_id TEXT PRIMARY KEY, dim INTEGER NOT NULL, revision TEXT);",
        )?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn register_model(
        &self,
        model_id: &str,
        dim: usize,
        revision: Option<&str>,
    ) -> anyhow::Result<()> {
        self.conn().execute(
            "INSERT OR REPLACE INTO models (model_id, dim, revision) VALUES (?1, ?2, ?3)",
            params![model_id, dim as i64, revision],
        )?;
        Ok(())
    }

    /// Returns the cached vectors for whichever `keys` are present.
    pub fn get_many(&self, keys: &[CacheKey]) -> anyhow::Result<HashMap<CacheKey, Vec<f32>>> {
        let conn = self.conn();
        let mut stmt = conn.prepare_cached("SELECT vector FROM embeddings WHERE key = ?1")?;
        let mut out = HashMap::with_capacity(keys.len());
        for key in keys {
            if out.contains_key(key) {
                continue;
            }
            let blob: Option<Vec<u8>> = stmt.query_row([&key[..]], |r| r.get(0)).optional()?;
            if let Some(b) = blob {
                out.insert(*key, decode(&b));
            }
        }
        Ok(out)
    }

    pub fn put_many(&self, model_id: &str, entries: &[(CacheKey, &[f32])]) -> anyhow::Result<()> {
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64;
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT OR IGNORE INTO embeddings (key, model_id, dim, vector, created) VALUES (?1, ?2, ?3, ?4, ?5)",
            )?;
            for (key, v) in entries {
                stmt.execute(params![&key[..], model_id, v.len() as i64, encode(v), now])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// (entries, bytes of vector data).
    pub fn stats(&self) -> anyhow::Result<(u64, u64)> {
        Ok(self.conn().query_row(
            "SELECT COUNT(*), COALESCE(SUM(LENGTH(vector)), 0) FROM embeddings",
            [],
            |r| Ok((r.get::<_, i64>(0)? as u64, r.get::<_, i64>(1)? as u64)),
        )?)
    }

    /// Deletes oldest entries until vector data fits in `max_bytes`. Returns rows deleted.
    pub fn prune(&self, max_bytes: u64) -> anyhow::Result<u64> {
        let (_, bytes) = self.stats()?;
        if bytes <= max_bytes {
            return Ok(0);
        }
        let conn = self.conn();
        let mut excess = (bytes - max_bytes) as i64;
        let mut deleted = 0u64;
        let mut stmt =
            conn.prepare("SELECT key, LENGTH(vector) FROM embeddings ORDER BY created ASC")?;
        let rows: Vec<(Vec<u8>, i64)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<_, _>>()?;
        for (key, len) in rows {
            if excess <= 0 {
                break;
            }
            conn.execute("DELETE FROM embeddings WHERE key = ?1", [&key])?;
            excess -= len;
            deleted += 1;
        }
        Ok(deleted)
    }
}

fn encode(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_le_bytes()).collect()
}

fn decode(b: &[u8]) -> Vec<f32> {
    b.as_chunks::<4>()
        .0
        .iter()
        .map(|c| f32::from_le_bytes(*c))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_depends_only_on_model_and_text() {
        assert_eq!(cache_key("m", "fn a() {}"), cache_key("m", "fn a() {}"));
        assert_ne!(cache_key("m", "fn a() {}"), cache_key("m2", "fn a() {}"));
        assert_ne!(cache_key("m", "fn a() {}"), cache_key("m", "fn b() {}"));
        assert_eq!(
            cache_key("model", "text"),
            *blake3::hash(b"modeltext").as_bytes()
        );
    }

    #[test]
    fn roundtrip_and_prune() {
        let dir = crate::testutil::tempdir();
        let cache = EmbeddingCache::open(&dir.join("cache.sqlite")).unwrap();
        let k1 = cache_key("m", "a");
        let k2 = cache_key("m", "b");
        cache
            .put_many("m", &[(k1, &[1.0, 2.0]), (k2, &[3.0, 4.0])])
            .unwrap();
        let got = cache.get_many(&[k1, k2, cache_key("m", "c")]).unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[&k1], vec![1.0, 2.0]);
        assert_eq!(cache.stats().unwrap(), (2, 16));
        assert_eq!(cache.prune(8).unwrap(), 1);
        assert_eq!(cache.stats().unwrap().0, 1);
    }
}
