//! `items.sqlite`: the join table between the vector and lexical indexes and the source of
//! output lines.

use std::path::Path;

use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};

use crate::cache::CacheKey;
use crate::chunk::Item;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ItemRow {
    pub id: u64,
    #[serde(flatten)]
    pub item: Item,
    pub unit: Option<String>,
    pub chunk_key: CacheKey,
}

pub struct ItemStore {
    conn: Connection,
}

const COLUMNS: &str = "id, rel_path, language, kind, name, qualified_name, line_start, line_end, signature, doc, unit, is_test, chunk_key, tagged, text, name_line, name_column";
/// Columns added after the first schema, with their definitions (added in place on open).
const ADDED_COLUMNS: [(&str, &str); 2] = [
    ("name_line", "INTEGER NOT NULL DEFAULT 0"),
    ("name_column", "INTEGER NOT NULL DEFAULT 0"),
];

impl ItemStore {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let conn = Connection::open(path)?;
        conn.busy_timeout(std::time::Duration::from_secs(30))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS items (
               id INTEGER PRIMARY KEY, rel_path TEXT NOT NULL, language TEXT NOT NULL, kind TEXT NOT NULL,
               name TEXT NOT NULL, qualified_name TEXT NOT NULL, line_start INTEGER NOT NULL,
               line_end INTEGER NOT NULL, signature TEXT NOT NULL, doc TEXT NOT NULL, unit TEXT,
               is_test INTEGER NOT NULL, chunk_key BLOB NOT NULL, tagged INTEGER NOT NULL, text TEXT NOT NULL);
             CREATE INDEX IF NOT EXISTS items_path ON items(rel_path);",
        )?;
        let existing: Vec<String> = conn
            .prepare("SELECT name FROM pragma_table_info('items')")?
            .query_map([], |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        for (name, def) in ADDED_COLUMNS {
            if !existing.iter().any(|c| c == name) {
                conn.execute_batch(&format!("ALTER TABLE items ADD COLUMN {name} {def}"))?;
            }
        }
        Ok(Self { conn })
    }

    pub fn load_all(&self) -> anyhow::Result<Vec<ItemRow>> {
        let mut stmt = self.conn.prepare(&format!("SELECT {COLUMNS} FROM items"))?;
        let rows = stmt
            .query_map([], |r| {
                let key: Vec<u8> = r.get(12)?;
                Ok(ItemRow {
                    id: r.get::<_, i64>(0)? as u64,
                    item: Item {
                        rel_path: r.get(1)?,
                        language: r.get(2)?,
                        kind: r.get(3)?,
                        name: r.get(4)?,
                        qualified_name: r.get(5)?,
                        line_start: r.get(6)?,
                        line_end: r.get(7)?,
                        signature: r.get(8)?,
                        doc: r.get(9)?,
                        is_test: r.get(11)?,
                        tagged: r.get(13)?,
                        text: r.get(14)?,
                        name_line: r.get(15)?,
                        name_column: r.get(16)?,
                    },
                    unit: r.get(10)?,
                    chunk_key: key.try_into().unwrap_or([0; 32]),
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn ids_for_paths(&self, paths: &[String]) -> anyhow::Result<Vec<u64>> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT id FROM items WHERE rel_path = ?1")?;
        let mut out = Vec::new();
        for p in paths {
            for id in stmt.query_map([p], |r| r.get::<_, i64>(0))? {
                out.push(id? as u64);
            }
        }
        Ok(out)
    }

    pub fn max_id(&self) -> anyhow::Result<u64> {
        Ok(self
            .conn
            .query_row("SELECT COALESCE(MAX(id), 0) FROM items", [], |r| {
                r.get::<_, i64>(0)
            })? as u64)
    }

    /// Deletes every item of `paths` and inserts `rows`, atomically.
    pub fn replace_files(&mut self, paths: &[String], rows: &[ItemRow]) -> anyhow::Result<()> {
        let tx = self.conn.transaction()?;
        {
            let mut del = tx.prepare_cached("DELETE FROM items WHERE rel_path = ?1")?;
            for p in paths {
                del.execute([p])?;
            }
            let mut ins = tx.prepare_cached(&format!(
                "INSERT INTO items ({COLUMNS}) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17)"
            ))?;
            for row in rows {
                let i = &row.item;
                ins.execute(params![
                    row.id as i64,
                    i.rel_path,
                    i.language,
                    i.kind,
                    i.name,
                    i.qualified_name,
                    i.line_start,
                    i.line_end,
                    i.signature,
                    i.doc,
                    row.unit,
                    i.is_test,
                    &row.chunk_key[..],
                    i.tagged,
                    i.text,
                    i.name_line,
                    i.name_column
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn clear(&mut self) -> anyhow::Result<()> {
        self.conn.execute("DELETE FROM items", [])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::tempdir;

    #[test]
    fn opening_an_old_schema_adds_columns() {
        let path = tempdir().join("items.sqlite");
        Connection::open(&path)
            .unwrap()
            .execute_batch(
                "CREATE TABLE items (
                   id INTEGER PRIMARY KEY, rel_path TEXT NOT NULL, language TEXT NOT NULL, kind TEXT NOT NULL,
                   name TEXT NOT NULL, qualified_name TEXT NOT NULL, line_start INTEGER NOT NULL,
                   line_end INTEGER NOT NULL, signature TEXT NOT NULL, doc TEXT NOT NULL, unit TEXT,
                   is_test INTEGER NOT NULL, chunk_key BLOB NOT NULL, tagged INTEGER NOT NULL, text TEXT NOT NULL);
                 INSERT INTO items VALUES (1, 'a.rs', 'rust', 'function', 'f', 'f', 3, 4, 'fn f()', '',
                   NULL, 0, zeroblob(32), 1, 'fn f() {}');",
            )
            .unwrap();
        let store = ItemStore::open(&path).unwrap();
        let rows = store.load_all().unwrap();
        assert_eq!((rows[0].item.name_line, rows[0].item.name_column), (0, 0));
        // Re-opening a migrated store is a no-op.
        drop(store);
        assert_eq!(ItemStore::open(&path).unwrap().load_all().unwrap().len(), 1);
    }
}
