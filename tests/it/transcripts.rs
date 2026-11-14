//! `lode transcript-stats`: Claude Code and Codex transcripts read incrementally into statistics.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use lodestone::agents::Harness;
use lodestone::config::Config;
use lodestone::testutil::tempdir;
use lodestone::transcripts::{self, Filter, Report};
use serde_json::{Value, json};

const T0: &str = "2026-11-11T10:00:0";

fn ts(s: u32) -> String {
    format!("{T0}{s}.000Z")
}

fn ms(s: u32) -> i64 {
    transcripts::parse_date("2026-11-11").unwrap() + 10 * 3_600_000 + i64::from(s) * 1000
}

struct Setup {
    config: Config,
    claude: PathBuf,
    codex: PathBuf,
    repo: PathBuf,
    other: PathBuf,
}

impl Setup {
    fn new() -> Self {
        let base = tempdir().canonicalize().unwrap();
        let repo = base.join("repo");
        let other = base.join("other");
        fs::create_dir_all(&repo).unwrap();
        fs::create_dir_all(&other).unwrap();
        let s = Setup {
            config: Config {
                data_dir: Some(base.join("home")),
                ..Config::default()
            },
            claude: base.join("claude"),
            codex: base.join("codex"),
            repo,
            other,
        };
        s.register(&[&s.repo]);
        s
    }

    fn register(&self, roots: &[&Path]) {
        let repos: Vec<Value> = roots
            .iter()
            .map(|r| json!({ "root": r, "name": r.file_name().unwrap().to_string_lossy() }))
            .collect();
        fs::create_dir_all(self.config.data_dir()).unwrap();
        fs::write(
            self.config.data_dir().join("registry.json"),
            json!({ "repos": repos }).to_string(),
        )
        .unwrap();
    }

    fn claude_file(&self, cwd: &Path, rel: &str) -> PathBuf {
        let project: String = cwd
            .to_string_lossy()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        self.claude.join(project).join(rel)
    }

    fn scan(&self) -> transcripts::ScanSummary {
        transcripts::scan(
            &self.config,
            &[
                (Harness::Claude, self.claude.clone()),
                (Harness::Codex, self.codex.clone()),
            ],
        )
        .unwrap()
    }

    fn report(&self) -> Report {
        let filter = Filter {
            harnesses: vec!["claude".into(), "codex".into()],
            ..Filter::default()
        };
        transcripts::report(&self.config, &filter).unwrap()
    }
}

fn append(path: &Path, lines: &[Value]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap();
    for l in lines {
        writeln!(f, "{l}").unwrap();
    }
}

fn claude_line(session: &str, cwd: &Path, s: u32, role: &str, content: Value) -> Value {
    json!({ "type": role, "sessionId": session, "cwd": cwd, "timestamp": ts(s),
            "message": { "role": role, "content": content } })
}

fn tool_use(id: &str, name: &str, input: Value) -> Value {
    json!([{ "type": "tool_use", "id": id, "name": name, "input": input }])
}

fn tool_result(id: &str, text: &str) -> Value {
    json!([{ "type": "tool_result", "tool_use_id": id, "content": [{ "type": "text", "text": text }] }])
}

fn codex_meta(id: &str, cwd: &Path) -> Value {
    json!({ "type": "session_meta", "timestamp": ts(0), "payload": { "id": id, "cwd": cwd, "timestamp": ts(0) } })
}

fn codex_item(start: u32, end: u32, item: Value) -> Value {
    json!({ "type": "event_msg", "timestamp": ts(end), "payload": { "type": "item_completed",
            "started_at_ms": ms(start), "completed_at_ms": ms(end), "item": item } })
}

fn group<'a>(r: &'a Report, harness: &str) -> &'a transcripts::Group {
    r.groups.iter().find(|g| g.harness == harness).unwrap()
}

#[test]
fn reads_sessions_in_registered_repositories_incrementally() {
    let s = Setup::new();
    let main = s.claude_file(&s.repo, "s1.jsonl");
    append(
        &main,
        &[
            json!({ "type": "queue-operation", "sessionId": "s1", "timestamp": ts(0) }),
            claude_line(
                "s1",
                &s.repo,
                1,
                "assistant",
                tool_use("g1", "Grep", json!({ "pattern": "x" })),
            ),
            claude_line(
                "s1",
                &s.repo,
                2,
                "assistant",
                tool_use(
                    "q1",
                    "mcp__plugin_lodestone_lodestone__semantic_code_search",
                    json!({ "query": "parse a duration", "example_identifiers": ["parse_duration"] }),
                ),
            ),
            claude_line(
                "s1",
                &s.repo,
                4,
                "user",
                tool_result("q1", "core/src/a.rs:3:8  lines 3-5  pub fn a()"),
            ),
            claude_line(
                "s1",
                &s.repo,
                5,
                "assistant",
                tool_use(
                    "r1",
                    "Read",
                    json!({ "file_path": s.repo.join("core/src/a.rs") }),
                ),
            ),
        ],
    );
    // A subagent of s1, in its own file.
    append(
        &s.claude_file(&s.repo, "s1/subagents/agent-1.jsonl"),
        &[claude_line(
            "s1",
            &s.repo,
            3,
            "assistant",
            tool_use("x1", "Glob", json!({ "pattern": "*" })),
        )],
    );
    // A project outside every registered repository.
    append(
        &s.claude_file(&s.other, "s2.jsonl"),
        &[claude_line(
            "s2",
            &s.other,
            1,
            "assistant",
            tool_use("g2", "Grep", json!({})),
        )],
    );
    let codex_main = s.codex.join("2026/11/11/rollout-a.jsonl");
    append(
        &codex_main,
        &[
            codex_meta("c1", &s.repo),
            codex_item(
                1,
                3,
                json!({ "type": "McpToolCall", "id": "m1", "server": "lodestone",
                "tool": "similar_code_search", "arguments": { "code": "\nfn a() {}\n" },
                "status": "completed",
                "result": { "content": [{ "type": "text", "text": "core/src/b.rs:1:4  lines 1-2  fn b()" }], "isError": false } }),
            ),
            codex_item(
                4,
                4,
                json!({ "type": "CommandExecution", "id": "e1", "cwd": s.repo,
                "parsed_cmd": [{ "type": "read", "cmd": "cat core/src/b.rs", "name": "b.rs", "path": "core/src/b.rs" },
                               { "type": "search", "cmd": "rg x", "query": "x", "path": null }] }),
            ),
        ],
    );
    let codex_other = s.codex.join("2026/11/11/rollout-b.jsonl");
    append(
        &codex_other,
        &[
            codex_meta("c2", &s.other),
            codex_item(
                1,
                2,
                json!({ "type": "CommandExecution", "id": "e2", "cwd": s.other,
                "parsed_cmd": [{ "type": "search", "cmd": "rg y", "query": "y", "path": null }] }),
            ),
        ],
    );

    let first = s.scan();
    // Every transcript is read; only calls in registered repositories are kept.
    assert_eq!(first.files_read, 5, "{first:?}");
    let r = s.report();
    assert_eq!(r.groups.len(), 2, "{:#?}", r.groups);
    let c = group(&r, "claude");
    assert_eq!(c.repo, s.repo.to_string_lossy());
    assert_eq!(
        (c.sessions, c.sessions_searched, c.searched_before_grep),
        (1, 1, 0)
    );
    assert_eq!((c.semantic, c.similar, c.search), (1, 0, 0));
    assert_eq!((c.greps, c.globs, c.reads), (1, 1, 1));
    assert_eq!((c.searches_read, c.searches_empty), (1, 0));
    assert_eq!(c.latency_ms_median, Some(2000));
    let x = group(&r, "codex");
    assert_eq!(
        (x.sessions, x.sessions_searched, x.searched_before_grep),
        (1, 1, 1)
    );
    assert_eq!((x.similar, x.greps, x.reads, x.searches_read), (1, 1, 1, 1));
    assert_eq!(x.latency_ms_median, Some(2000));
    let queries: Vec<&str> = r.searches.iter().map(|q| q.query.as_str()).collect();
    assert_eq!(queries, ["fn a() {}", "parse a duration [parse_duration]"]);

    // Nothing changed: nothing is read.
    let again = s.scan();
    assert_eq!((again.files_read, again.lines), (0, 0), "{again:?}");

    // New lines, the last one still being written: only complete lines are read.
    append(
        &main,
        &[claude_line(
            "s1",
            &s.repo,
            6,
            "assistant",
            tool_use(
                "q2",
                "mcp__lodestone__semantic_code_search",
                json!({ "query": "retry" }),
            ),
        )],
    );
    let partial = claude_line(
        "s1",
        &s.repo,
        7,
        "user",
        tool_result("q2", "core/src/c.rs:1:1  x"),
    )
    .to_string();
    let (head, tail) = partial.split_at(20);
    OpenOptions::new()
        .append(true)
        .open(&main)
        .unwrap()
        .write_all(head.as_bytes())
        .unwrap();
    assert_eq!(s.scan().lines, 1);
    assert_eq!(group(&s.report(), "claude").semantic, 2);
    assert_eq!(group(&s.report(), "claude").latency_ms_median, Some(2000));
    writeln!(
        OpenOptions::new().append(true).open(&main).unwrap(),
        "{tail}"
    )
    .unwrap();
    assert_eq!(s.scan().lines, 1);
    let c = group(&s.report(), "claude").clone();
    assert_eq!((c.semantic, c.searches_read), (2, 1));
    assert_eq!(c.latency_ms_p90, Some(2000));

    // Rewritten from scratch: its calls are read again, not added to.
    fs::write(
        &main,
        format!(
            "{}\n",
            claude_line(
                "s1",
                &s.repo,
                1,
                "assistant",
                tool_use("g9", "Grep", json!({}))
            )
        ),
    )
    .unwrap();
    s.scan();
    let c = group(&s.report(), "claude").clone();
    assert_eq!((c.semantic, c.greps, c.globs, c.reads), (0, 1, 1, 0));

    // Registering the other repository reads everything again, picking up its sessions.
    s.register(&[&s.repo, &s.other]);
    let later = s.scan();
    assert_eq!(later.files_read, 5, "{later:?}");
    let r = s.report();
    assert_eq!(
        r.groups
            .iter()
            .filter(|g| g.repo == s.other.to_string_lossy())
            .count(),
        2
    );
}

#[test]
fn counts_calls_towards_the_repository_they_worked_in() {
    // Sessions that start outside every registered repository (a scratch directory, a parent
    // folder) but search, read and run commands in one.
    let s = Setup::new();
    let a = s.repo.join("core/a.rs");
    fs::create_dir_all(a.parent().unwrap()).unwrap();
    fs::write(&a, "fn a() {}\n").unwrap();
    append(
        &s.claude_file(&s.other, "s1.jsonl"),
        &[
            claude_line(
                "s1",
                &s.other,
                1,
                "assistant",
                tool_use("g1", "Grep", json!({ "pattern": "x" })),
            ),
            claude_line(
                "s1",
                &s.other,
                2,
                "assistant",
                tool_use(
                    "q1",
                    "mcp__lodestone__semantic_code_search",
                    json!({ "query": "a", "repo": s.repo.to_string_lossy() }),
                ),
            ),
            claude_line(
                "s1",
                &s.other,
                3,
                "user",
                tool_result("q1", "core/a.rs:1:4  lines 1-1  fn a()"),
            ),
            claude_line(
                "s1",
                &s.other,
                4,
                "assistant",
                tool_use("r1", "Read", json!({ "file_path": a })),
            ),
            // No `repo`: the session's other calls all worked in `repo`.
            claude_line(
                "s1",
                &s.other,
                5,
                "assistant",
                tool_use(
                    "q2",
                    "mcp__lodestone__similar_code_search",
                    json!({ "code": "fn a() {}" }),
                ),
            ),
        ],
    );
    // A long-running Codex thread: started the day before, searching today, by repository name;
    // command directories as `file://` URLs.
    let yesterday = json!({ "type": "session_meta", "timestamp": "2026-11-10T09:00:00.000Z",
        "payload": { "id": "c1", "cwd": s.other, "timestamp": "2026-11-10T09:00:00.000Z" } });
    append(
        &s.codex.join("rollout-a.jsonl"),
        &[
            yesterday,
            codex_item(
                1,
                2,
                json!({ "type": "McpToolCall", "id": "m1", "server": "lodestone",
                "tool": "semantic_code_search", "arguments": { "query": "a", "repo": "repo" },
                "status": "completed",
                "result": { "content": [{ "type": "text", "text": "core/a.rs:1:4  lines 1-1  fn a()" }] } }),
            ),
            codex_item(
                3,
                3,
                json!({ "type": "CommandExecution", "id": "e1",
                "cwd": format!("file://{}", s.repo.display()),
                "parsed_cmd": [{ "type": "read", "cmd": "cat core/a.rs", "name": "a.rs", "path": "core/a.rs" }] }),
            ),
            codex_item(
                5,
                5,
                json!({ "type": "McpToolCall", "id": "m2", "server": "lodestone",
                "tool": "semantic_code_search", "arguments": { "query": "b", "repo": "repo" },
                "status": "failed", "result": null }),
            ),
            codex_item(
                4,
                4,
                json!({ "type": "CommandExecution", "id": "e2",
                "cwd": format!("file://{}", s.other.display()),
                "parsed_cmd": [{ "type": "search", "cmd": "rg x", "query": "x", "path": null }] }),
            ),
        ],
    );
    s.scan();
    let r = s.report();
    assert_eq!(r.groups.len(), 2, "{:#?}", r.groups);
    assert!(r.groups.iter().all(|g| g.repo == s.repo.to_string_lossy()));
    let c = group(&r, "claude");
    assert_eq!((c.sessions, c.semantic, c.similar), (1, 1, 1));
    // The grep ran in the scratch directory: not counted.
    assert_eq!((c.greps, c.reads, c.searches_read), (0, 1, 1));
    let x = group(&r, "codex");
    assert_eq!(
        (x.sessions, x.semantic, x.reads, x.greps, x.searches_read),
        (1, 2, 1, 0, 1)
    );
    // A failed (or denied) search is not an empty one, and has no latency.
    assert_eq!((x.searches_failed, x.searches_empty), (1, 0));
    assert_eq!(x.latency_ms_median, Some(1000));

    // `--since` applies to calls: today's searches in yesterday's thread count.
    let today = transcripts::report(
        &s.config,
        &Filter {
            harnesses: vec!["codex".into()],
            since: transcripts::parse_date("2026-11-11"),
            ..Filter::default()
        },
    )
    .unwrap();
    assert_eq!(group(&today, "codex").semantic, 2);
}

#[test]
fn report_filters_by_repo_harness_and_date() {
    let s = Setup::new();
    append(
        &s.claude_file(&s.repo, "s1.jsonl"),
        &[claude_line(
            "s1",
            &s.repo,
            1,
            "assistant",
            tool_use("g1", "Grep", json!({})),
        )],
    );
    append(
        &s.codex.join("rollout-a.jsonl"),
        &[codex_meta("c1", &s.repo)],
    );
    s.scan();
    let report = |f: Filter| transcripts::report(&s.config, &f).unwrap().groups.len();
    let both = || vec!["claude".to_string(), "codex".to_string()];
    assert_eq!(
        report(Filter {
            harnesses: both(),
            ..Filter::default()
        }),
        2
    );
    assert_eq!(
        report(Filter {
            harnesses: vec!["codex".into()],
            ..Filter::default()
        }),
        1
    );
    assert_eq!(
        report(Filter {
            harnesses: both(),
            repo: Some("repo".into()),
            ..Filter::default()
        }),
        2
    );
    assert_eq!(
        report(Filter {
            harnesses: both(),
            repo: Some("nope".into()),
            ..Filter::default()
        }),
        0
    );
    let tomorrow = transcripts::parse_date("2026-11-12");
    assert_eq!(
        report(Filter {
            harnesses: both(),
            since: tomorrow,
            ..Filter::default()
        }),
        0
    );
}
