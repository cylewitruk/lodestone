//! Agent sessions as leases. A session (the `lode mcp` relay, renewing every 30 s) or a request
//! that names a repository holds a lease on a worktree root; a session also keeps every worktree
//! its tool calls named. An on-demand daemon watches only leased roots and exits once no lease has
//! been live for its idle timeout.

use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;
use std::time::{Duration, Instant};

struct Lease {
    root: Option<PathBuf>,
    /// Other worktrees held while the lease lives.
    held: BTreeSet<PathBuf>,
    expires: Instant,
}

pub struct Leases {
    leases: HashMap<String, Lease>,
    /// When the last lease ended (or the daemon started).
    idle_since: Instant,
}

impl Leases {
    pub fn new(now: Instant) -> Self {
        Self {
            leases: HashMap::new(),
            idle_since: now,
        }
    }

    /// Takes or renews lease `id` on `root` for `ttl`.
    pub fn renew(&mut self, id: &str, root: Option<PathBuf>, ttl: Duration, now: Instant) {
        let lease = self.leases.entry(id.to_string()).or_insert_with(|| Lease {
            root: None,
            held: BTreeSet::new(),
            expires: now,
        });
        lease.root = root;
        lease.expires = now + ttl;
    }

    /// Keeps `root` leased for as long as lease `id` lives (nothing, if there is no such lease).
    pub fn hold(&mut self, id: &str, root: PathBuf) {
        if let Some(l) = self.leases.get_mut(id) {
            l.held.insert(root);
        }
    }

    pub fn release(&mut self, id: &str, now: Instant) {
        if self.leases.remove(id).is_some() && self.leases.is_empty() {
            self.idle_since = now;
        }
    }

    /// The root lease `id` holds.
    pub fn root(&self, id: &str) -> Option<PathBuf> {
        self.leases.get(id).and_then(|l| l.root.clone())
    }

    /// Drops expired leases and returns the roots still leased.
    pub fn sweep(&mut self, now: Instant) -> BTreeSet<PathBuf> {
        let mut last_end = None;
        self.leases.retain(|_, l| {
            let live = l.expires > now;
            if !live {
                last_end = last_end.max(Some(l.expires));
            }
            live
        });
        if self.leases.is_empty()
            && let Some(end) = last_end
        {
            self.idle_since = self.idle_since.max(end);
        }
        self.leases
            .values()
            .flat_map(|l| l.root.iter().chain(&l.held).cloned())
            .collect()
    }

    /// How long no lease has been live; `None` while one is.
    pub fn idle_for(&self, now: Instant) -> Option<Duration> {
        self.leases
            .is_empty()
            .then(|| now.saturating_duration_since(self.idle_since))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN: Duration = Duration::from_secs(60);

    #[test]
    fn leases_expire_and_idle_time_counts_from_the_last_end() {
        let t0 = Instant::now();
        let mut l = Leases::new(t0);
        assert_eq!(l.idle_for(t0 + MIN), Some(MIN));

        l.renew("relay-1", Some("/a".into()), MIN, t0);
        l.renew("request:/b", Some("/b".into()), 2 * MIN, t0);
        assert_eq!(l.idle_for(t0), None);
        assert_eq!(l.root("relay-1"), Some(PathBuf::from("/a")));

        let live = l.sweep(t0 + MIN + Duration::from_secs(1));
        assert_eq!(live, BTreeSet::from([PathBuf::from("/b")]));
        assert_eq!(l.idle_for(t0 + MIN), None);

        // The last lease ended at t0 + 2 min, however late the sweep notices.
        assert!(l.sweep(t0 + 5 * MIN).is_empty());
        assert_eq!(l.idle_for(t0 + 5 * MIN), Some(3 * MIN));
    }

    #[test]
    fn a_lease_keeps_what_it_holds_across_renewals() {
        let t0 = Instant::now();
        let mut l = Leases::new(t0);
        l.renew("relay-1", None, MIN, t0);
        l.hold("relay-1", "/a".into());
        l.hold("unknown", "/b".into());
        l.renew("relay-1", None, MIN, t0 + MIN / 2);
        assert_eq!(l.sweep(t0 + MIN), BTreeSet::from([PathBuf::from("/a")]));
        assert!(l.sweep(t0 + 2 * MIN).is_empty());
    }

    #[test]
    fn release_ends_a_lease_now() {
        let t0 = Instant::now();
        let mut l = Leases::new(t0);
        l.renew("relay-1", None, MIN, t0);
        l.release("relay-1", t0 + Duration::from_secs(5));
        assert_eq!(
            l.idle_for(t0 + Duration::from_secs(65)),
            Some(Duration::from_secs(60))
        );
    }
}
