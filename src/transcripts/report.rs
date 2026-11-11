//! Statistics over what was recorded from transcripts.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;

use serde::Serialize;

use super::store::{CallRow, SessionRow};
use super::{Kind, time};
use crate::text::table;

#[derive(Clone, Debug, Default)]
pub struct Filter {
    /// Harnesses to include (`claude`, `codex`).
    pub harnesses: Vec<String>,
    /// A registered repository, by root path or directory name.
    pub repo: Option<String>,
    /// Calls at or after this time, and sessions started then (ms since the epoch).
    pub since: Option<i64>,
}

impl Filter {
    fn harness(&self, s: &SessionRow) -> bool {
        self.harnesses.contains(&s.harness)
    }

    fn repo(&self, repo: &str) -> bool {
        self.repo.as_deref().is_none_or(|r| {
            let root = Path::new(repo);
            let wanted = Path::new(r);
            root == wanted
                || wanted.canonicalize().is_ok_and(|w| w == root)
                || root.file_name().is_some_and(|n| n == r)
        })
    }

    fn since(&self, ts: i64) -> bool {
        self.since.is_none_or(|t| ts >= t)
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Report {
    /// One per repository and harness.
    pub groups: Vec<Group>,
    pub searches: Vec<Search>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Group {
    pub repo: String,
    pub harness: String,
    pub sessions: usize,
    pub sessions_searched: usize,
    /// Sessions whose first lodestone search came before their first text search, if any.
    pub searched_before_grep: usize,
    pub semantic: usize,
    pub similar: usize,
    /// The `search` tool, before it was split into semantic and similar.
    pub search: usize,
    /// Searches with a result path read later in the same session.
    pub searches_read: usize,
    /// Searches that completed with no results.
    pub searches_empty: usize,
    /// Searches that failed: an error, or denied by the harness.
    pub searches_failed: usize,
    pub greps: usize,
    pub globs: usize,
    pub reads: usize,
    pub latency_ms_median: Option<i64>,
    pub latency_ms_p90: Option<i64>,
}

impl Group {
    fn searches(&self) -> usize {
        self.semantic + self.similar + self.search
    }
}

/// One lodestone search.
#[derive(Clone, Debug, Serialize)]
pub struct Search {
    pub ts: i64,
    pub harness: String,
    pub repo: String,
    pub kind: &'static str,
    pub query: String,
    pub results: usize,
    pub read: bool,
}

/// Each call counts towards its repository. A lodestone search that names none counts towards the
/// session's working directory's, else the one repository the session's other calls worked in.
/// A session counts for every repository it worked in (or started in, within `since`).
pub fn build(sessions: &[SessionRow], calls: &[CallRow], f: &Filter) -> anyhow::Result<Report> {
    let mut by_session: HashMap<&str, Vec<&CallRow>> = HashMap::new();
    for c in calls.iter().filter(|c| f.since(c.ts)) {
        by_session.entry(&c.session).or_default().push(c);
    }
    let mut groups: BTreeMap<(String, &str), (Group, Vec<i64>)> = BTreeMap::new();
    let mut searches = Vec::new();
    for s in sessions.iter().filter(|s| f.harness(s)) {
        let calls = by_session.get(s.id.as_str()).map_or(&[][..], Vec::as_slice);
        let touched: BTreeSet<&str> = calls.iter().filter_map(|c| c.repo.as_deref()).collect();
        let sole = (touched.len() == 1)
            .then(|| touched.iter().next().copied())
            .flatten();
        let mut per_repo: BTreeMap<&str, Vec<&CallRow>> = BTreeMap::new();
        for c in calls {
            if let Some(repo) = c.repo.as_deref().or(s.repo.as_deref()).or(sole) {
                per_repo.entry(repo).or_default().push(c);
            }
        }
        if let Some(repo) = s.repo.as_deref().filter(|_| f.since(s.started)) {
            per_repo.entry(repo).or_default();
        }
        for (repo, calls) in per_repo.into_iter().filter(|(r, _)| f.repo(r)) {
            let (g, latencies) = groups
                .entry((repo.to_string(), s.harness.as_str()))
                .or_insert_with(|| {
                    let g = Group {
                        repo: repo.to_string(),
                        harness: s.harness.clone(),
                        ..Group::default()
                    };
                    (g, Vec::new())
                });
            tally(g, latencies, &mut searches, s, repo, &calls);
        }
    }
    let groups = groups
        .into_values()
        .map(|(mut g, mut lat)| {
            lat.sort_unstable();
            // Nearest rank: the smallest value with at least p% of values at or below it.
            let pct = |p: usize| {
                (!lat.is_empty()).then(|| lat[(lat.len() * p).div_ceil(100).saturating_sub(1)])
            };
            g.latency_ms_median = pct(50);
            g.latency_ms_p90 = pct(90);
            g
        })
        .collect();
    searches.sort_by_key(|s| s.ts);
    Ok(Report { groups, searches })
}

/// Adds one session's calls in one repository to its group.
fn tally(
    g: &mut Group,
    latencies: &mut Vec<i64>,
    searches: &mut Vec<Search>,
    s: &SessionRow,
    repo: &str,
    calls: &[&CallRow],
) {
    g.sessions += 1;
    let first = |p: &dyn Fn(Kind) -> bool| calls.iter().find(|c| p(c.kind)).map(|c| c.ts);
    if let Some(search) = first(&Kind::is_lodestone) {
        g.sessions_searched += 1;
        if first(&|k| k == Kind::Grep).is_none_or(|grep| search <= grep) {
            g.searched_before_grep += 1;
        }
    }
    for c in calls {
        match c.kind {
            Kind::Semantic => g.semantic += 1,
            Kind::Similar => g.similar += 1,
            Kind::Search => g.search += 1,
            Kind::Grep => g.greps += 1,
            Kind::Glob => g.globs += 1,
            Kind::Read => g.reads += 1,
        }
        if !c.kind.is_lodestone() {
            continue;
        }
        if c.is_error {
            g.searches_failed += 1;
        } else if let Some(done) = c.done_ts {
            latencies.push(done - c.ts);
            if c.results.is_empty() {
                g.searches_empty += 1;
            }
        }
        let read = c.results.iter().any(|r| {
            calls.iter().any(|o| {
                o.kind == Kind::Read
                    && o.ts >= c.ts
                    && o.path
                        .as_deref()
                        .is_some_and(|p| p == r || p.ends_with(&format!("/{r}")))
            })
        });
        g.searches_read += usize::from(read);
        searches.push(Search {
            ts: c.ts,
            harness: s.harness.clone(),
            repo: repo.to_string(),
            kind: c.kind.as_str(),
            query: c.query.clone().unwrap_or_default(),
            results: c.results.len(),
            read,
        });
    }
}

impl Report {
    /// One table per repository with a column per harness; with `queries`, every search too.
    pub fn to_text(&self, queries: bool) -> String {
        if self.groups.is_empty() {
            return "no sessions recorded in registered repositories".into();
        }
        let mut out = Vec::new();
        let mut repos: Vec<&str> = self.groups.iter().map(|g| g.repo.as_str()).collect();
        repos.dedup();
        for repo in repos {
            let cols: Vec<&Group> = self.groups.iter().filter(|g| g.repo == repo).collect();
            let row = |label: &str, cell: &dyn Fn(&Group) -> String| {
                (
                    label.to_string(),
                    cols.iter().map(|g| cell(g)).collect::<Vec<_>>(),
                )
            };
            let searched = |g: &Group| g.sessions_searched;
            let rows = vec![
                row("", &|g| g.harness.clone()),
                row("sessions", &|g| g.sessions.to_string()),
                row("sessions that searched", &|g| {
                    share(searched(g), g.sessions)
                }),
                row("searched before first grep", &|g| {
                    ratio(g.searched_before_grep, searched(g))
                }),
                row("lodestone searches", &|g| g.searches().to_string()),
                row("  semantic / similar / search", &|g| {
                    format!("{} / {} / {}", g.semantic, g.similar, g.search)
                }),
                row("  with a result read", &|g| {
                    share(g.searches_read, g.searches())
                }),
                row("  with no results", &|g| {
                    share(g.searches_empty, g.searches())
                }),
                row("  failed", &|g| share(g.searches_failed, g.searches())),
                row(
                    "  latency median / p90",
                    &|g| match (g.latency_ms_median, g.latency_ms_p90) {
                        (Some(m), Some(p)) => format!("{} / {}", secs(m), secs(p)),
                        _ => "-".into(),
                    },
                ),
                row("grep / glob / read calls", &|g| {
                    format!("{} / {} / {}", g.greps, g.globs, g.reads)
                }),
            ];
            out.push(repo.to_string());
            out.push(table(&rows));
        }
        if queries && !self.searches.is_empty() {
            out.push("searches:".into());
            for s in &self.searches {
                out.push(format!(
                    "  {}  {:<6} {:<12} {:<8} {:>2} results{}  {}",
                    time::format(s.ts),
                    s.harness,
                    Path::new(&s.repo)
                        .file_name()
                        .map_or(s.repo.clone(), |n| n.to_string_lossy().into_owned()),
                    s.kind,
                    s.results,
                    if s.read { ", read" } else { "      " },
                    s.query
                ));
            }
        }
        out.join("\n")
    }
}

fn share(n: usize, of: usize) -> String {
    if of == 0 {
        return "-".into();
    }
    format!("{n} ({:.0}%)", 100.0 * n as f64 / of as f64)
}

fn ratio(n: usize, of: usize) -> String {
    if of == 0 {
        "-".into()
    } else {
        format!("{n}/{of}")
    }
}

fn secs(ms: i64) -> String {
    format!("{:.1}s", ms as f64 / 1000.0)
}
