//! usearch HNSW index keyed by item id (cosine). Snapshots share an index read-only; writers
//! duplicate it, mutate the copy and publish the copy in the next snapshot.
//!
//! usearch fails a search that finds all of an index's thread slots taken ("Reserve capacity
//! ahead of searches!"), so each index reserves [`search_slots`] and searches beyond that wait.

use std::path::Path;
use std::sync::{Condvar, Mutex, MutexGuard};

use anyhow::anyhow;
use usearch::{Index, IndexOptions, MetricKind, ScalarKind};

use crate::config::Precision;

pub struct VectorIndex {
    index: Index,
    dim: usize,
    precision: Precision,
    slots: Slots,
}

/// Searches one index runs at once: one per core, usearch's own default.
fn search_slots() -> usize {
    std::thread::available_parallelism().map_or(4, |n| n.get())
}

/// A counting semaphore over an index's search slots.
struct Slots {
    free: Mutex<usize>,
    freed: Condvar,
}

impl Slots {
    fn new(n: usize) -> Self {
        Self {
            free: Mutex::new(n),
            freed: Condvar::new(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, usize> {
        self.free.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn take(&self) -> Slot<'_> {
        let mut free = self.lock();
        while *free == 0 {
            free = self.freed.wait(free).unwrap_or_else(|e| e.into_inner());
        }
        *free -= 1;
        Slot(self)
    }
}

struct Slot<'a>(&'a Slots);

impl Drop for Slot<'_> {
    fn drop(&mut self) {
        *self.0.lock() += 1;
        self.0.freed.notify_one();
    }
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
        let v = Self {
            index,
            dim,
            precision,
            slots: Slots::new(search_slots()),
        };
        v.reserve(0)?;
        Ok(v)
    }

    /// Room for `capacity` vectors and [`search_slots`] concurrent searches.
    fn reserve(&self, capacity: usize) -> anyhow::Result<()> {
        self.index
            .reserve_capacity_and_threads(capacity.max(self.len()), search_slots())
            .map_err(|e| anyhow!("usearch reserve: {e}"))
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
        v.reserve(0)?;
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
        copy.reserve(0)?;
        Ok(copy)
    }

    /// Inserts sequentially: concurrent `add` calls beyond usearch's thread-slot pool were
    /// observed to spin indefinitely, and embedding dominates indexing time anyway.
    pub fn add_many(&mut self, entries: &[(u64, &[f32])]) -> anyhow::Result<()> {
        if entries.is_empty() {
            return Ok(());
        }
        self.reserve(self.len() + entries.len())?;
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
    /// The `k` nearest keys that `admit` accepts, as (key, cosine similarity). The graph walk
    /// continues past rejected keys until `k` are found or the graph is exhausted.
    pub fn search(
        &self,
        query: &[f32],
        k: usize,
        admit: impl Fn(u64) -> bool,
    ) -> anyhow::Result<Vec<(u64, f32)>> {
        if self.is_empty() || !query.iter().all(|x| x.is_finite()) {
            return Ok(Vec::new());
        }
        let m = {
            let _slot = self.slots.take();
            self.index
                .filtered_search(query, k, admit)
                .map_err(|e| anyhow!("usearch search: {e}"))?
        };
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
        assert_eq!(
            v.search(&[1.0, 0.0, 0.0, 0.0], 1, |_| true).unwrap()[0].0,
            2
        );
        assert!(v.search(&[f32::NAN; 4], 1, |_| true).unwrap().is_empty());
    }

    #[test]
    fn filtered_search_finds_k_admitted_keys_past_rejected_ones() {
        let mut v = VectorIndex::new(8, Precision::F32).unwrap();
        let vecs: Vec<Vec<f32>> = (0..300)
            .map(|i| (0..8).map(|j| ((i * 7 + j) % 13) as f32 + 0.5).collect())
            .collect();
        let entries: Vec<(u64, &[f32])> = vecs
            .iter()
            .enumerate()
            .map(|(i, x)| (i as u64, x.as_slice()))
            .collect();
        v.add_many(&entries).unwrap();
        let hits = v.search(&vecs[1], 5, |k| k % 50 == 0).unwrap();
        assert_eq!(hits.len(), 5);
        assert!(hits.iter().all(|(k, _)| k % 50 == 0), "{hits:?}");
    }

    #[test]
    fn more_concurrent_searches_than_slots_all_succeed() {
        let mut v = VectorIndex::new(8, Precision::F32).unwrap();
        let vecs: Vec<Vec<f32>> = (0..300)
            .map(|i| (0..8).map(|j| ((i * 7 + j) % 13) as f32 + 0.5).collect())
            .collect();
        let entries: Vec<(u64, &[f32])> = vecs
            .iter()
            .enumerate()
            .map(|(i, x)| (i as u64, x.as_slice()))
            .collect();
        v.add_many(&entries).unwrap();
        // Published indexes are duplicates; they need their slots too.
        let v = v.duplicate().unwrap();
        let failed = std::sync::atomic::AtomicUsize::new(0);
        std::thread::scope(|s| {
            for t in 0..search_slots() * 4 {
                let (v, failed) = (&v, &failed);
                s.spawn(move || {
                    for i in 0..500 {
                        if v.search(&[(t + i) as f32 % 5.0 + 1.0; 8], 10, |_| true)
                            .is_err()
                        {
                            failed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        }
                    }
                });
            }
        });
        assert_eq!(failed.into_inner(), 0);
    }
}
