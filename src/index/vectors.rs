//! usearch HNSW index keyed by item id (cosine). Snapshots share an index read-only; writers
//! duplicate it, mutate the copy and publish the copy in the next snapshot.

use std::path::Path;

use anyhow::anyhow;
use usearch::{Index, IndexOptions, MetricKind, ScalarKind};

use crate::config::Precision;

pub struct VectorIndex {
    index: Index,
    dim: usize,
    precision: Precision,
}

fn options(dim: usize, precision: Precision) -> IndexOptions {
    IndexOptions {
        dimensions: dim,
        metric: MetricKind::Cos,
        quantization: match precision {
            Precision::F32 => ScalarKind::F32,
            Precision::F16 => ScalarKind::F16,
        },
        connectivity: 16,
        expansion_add: 128,
        expansion_search: 128,
        multi: false,
    }
}

impl VectorIndex {
    pub fn new(dim: usize, precision: Precision) -> anyhow::Result<Self> {
        let index = Index::new(&options(dim, precision)).map_err(|e| anyhow!("usearch: {e}"))?;
        Ok(Self {
            index,
            dim,
            precision,
        })
    }

    /// Loads `path`, or returns `None` if it is missing or was built with other parameters.
    pub fn load(path: &Path, dim: usize, precision: Precision) -> anyhow::Result<Option<Self>> {
        if !path.exists() {
            return Ok(None);
        }
        let v = Self::new(dim, precision)?;
        let p = path.to_string_lossy();
        if v.index.load(&p).is_err() || v.index.dimensions() != dim {
            return Ok(None);
        }
        Ok(Some(v))
    }

    pub fn dim(&self) -> usize {
        self.dim
    }

    pub fn len(&self) -> usize {
        self.index.size()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn contains(&self, id: u64) -> bool {
        self.index.contains(id)
    }

    /// Deep copy for copy-on-write publication.
    pub fn duplicate(&self) -> anyhow::Result<Self> {
        let copy = Self::new(self.dim, self.precision)?;
        if self.is_empty() {
            return Ok(copy);
        }
        let mut buf = vec![0u8; self.index.serialized_length()];
        self.index
            .save_to_buffer(&mut buf)
            .map_err(|e| anyhow!("usearch save: {e}"))?;
        copy.index
            .load_from_buffer(&buf)
            .map_err(|e| anyhow!("usearch load: {e}"))?;
        Ok(copy)
    }

    /// Inserts sequentially: concurrent `add` calls beyond usearch's thread-slot pool were
    /// observed to spin indefinitely, and embedding dominates indexing time anyway.
    pub fn add_many(&mut self, entries: &[(u64, &[f32])]) -> anyhow::Result<()> {
        if entries.is_empty() {
            return Ok(());
        }
        self.index
            .reserve(self.len() + entries.len())
            .map_err(|e| anyhow!("usearch reserve: {e}"))?;
        for (id, v) in entries {
            if v.len() != self.dim {
                return Err(anyhow!("vector dim {} != {}", v.len(), self.dim));
            }
            // Non-finite vectors make HNSW insertion loop forever.
            if !v.iter().all(|x| x.is_finite()) {
                return Err(anyhow!("refusing non-finite vector for item {id}"));
            }
            self.index
                .add(*id, v)
                .map_err(|e| anyhow!("usearch add: {e}"))?;
        }
        Ok(())
    }

    pub fn remove_many(&mut self, ids: &[u64]) {
        for id in ids {
            let _ = self.index.remove(*id);
        }
    }

    /// Nearest `k` as (id, cosine similarity), best first.
    pub fn search(&self, query: &[f32], k: usize) -> anyhow::Result<Vec<(u64, f32)>> {
        if self.is_empty() || !query.iter().all(|x| x.is_finite()) {
            return Ok(Vec::new());
        }
        let m = self
            .index
            .search(query, k)
            .map_err(|e| anyhow!("usearch search: {e}"))?;
        Ok(m.keys
            .into_iter()
            .zip(m.distances)
            .map(|(k, d)| (k, 1.0 - d))
            .collect())
    }

    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        let tmp = path.with_extension("usearch.tmp");
        self.index
            .save(&tmp.to_string_lossy())
            .map_err(|e| anyhow!("usearch save: {e}"))?;
        std::fs::rename(tmp, path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_non_finite_vectors() {
        let mut v = VectorIndex::new(4, Precision::F32).unwrap();
        assert!(v.add_many(&[(1, &[f32::NAN, 0.0, 0.0, 0.0])]).is_err());
        v.add_many(&[(2, &[1.0, 0.0, 0.0, 0.0])]).unwrap();
        assert_eq!(v.search(&[1.0, 0.0, 0.0, 0.0], 1).unwrap()[0].0, 2);
        assert!(v.search(&[f32::NAN; 4], 1).unwrap().is_empty());
    }
}
