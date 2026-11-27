//! `lode transcript-stats`: Claude Code, Codex and Antigravity transcripts read incrementally into
//! statistics.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use lodestone::config::Config;
use lodestone::testutil::tempdir;
use lodestone::transcripts::{self, Filter, Format, Report};
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
    antigravity: PathBuf,
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
            antigravity: base.join("antigravity"),
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

    fn agy_transcript(&self, conversation: &str) -> PathBuf {
        self.antigravity
            .join("brain")
            .join(conversation)
            .join(".system_generated/logs/transcript_full.jsonl")
    }

    /// Records `dir` as a conversation's workspace, as Antigravity's app does.
    fn agy_workspace(&self, conversation: &str, dir: &Path) {
        fs::create_dir_all(&self.antigravity).unwrap();
        let db =
            rusqlite::Connection::open(self.antigravity.join("conversation_summaries.db")).unwrap();
        db.execute_batch(
            "CREATE TABLE IF NOT EXISTS conversation_summaries
                 (conversation_id TEXT, workspace_uris TEXT NOT NULL)",
        )
        .unwrap();
        db.execute(
            "INSERT INTO conversation_summaries VALUES (?1, ?2)",
            [conversation.to_string(), json!([file_url(dir)]).to_string()],
        )
        .unwrap();
    }

    fn scan(&self) -> transcripts::ScanSummary {
        transcripts::scan(
            &self.config,
            &[
                (Format::Claude, self.claude.clone()),
                (Format::Codex, self.codex.clone()),
                (Format::Antigravity, self.antigravity.clone()),
            ],
        )
        .unwrap()
    }

    fn report(&self) -> Report {
        let filter = Filter {
            harnesses: vec!["claude".into(), "codex".into(), "antigravity".into()],
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
            // Listed as deferred, then loaded: the session had the tools.
            json!({ "type": "attachment", "sessionId": "s1", "cwd": s.repo, "timestamp": ts(6),
                "attachment": { "type": "deferred_tools_delta",
                    "addedNames": ["mcp__plugin_lodestone_lodestone__semantic_code_search"] } }),
            json!({ "type": "attachment", "sessionId": "s1", "cwd": s.repo, "timestamp": ts(6),
                "attachment": { "type": "prompt_snapshot",
                    "tools": [{ "name": "mcp__plugin_lodestone_lodestone__semantic_code_search" }] } }),
            // A filter on another command's output is not a search of the code.
            claude_line(
                "s1",
                &s.repo,
                7,
                "assistant",
                tool_use(
                    "b9",
                    "Bash",
                    json!({ "command": "cargo test 2>&1 | grep FAILED" }),
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
    assert_eq!(
        (
            c.sessions_tools_known,
            c.sessions_tools_loaded,
            c.loaded_searched
        ),
        (1, 1, 1)
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
    // Codex transcripts do not record the tools.
    assert_eq!((x.sessions_tools_known, x.sessions_tools_loaded), (0, 0));
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

    // What was read from a transcript deleted since stays when the rest is read again.
    fs::remove_file(&main).unwrap();
    s.register(&[&s.repo]);
    s.scan();
    let r = s.report();
    let kept = r
        .groups
        .iter()
        .find(|g| g.harness == "claude" && g.repo == s.repo.to_string_lossy())
        .unwrap();
    assert_eq!((kept.greps, kept.globs), (1, 1));

    // A scan of one harness's transcripts keeps what was read from the others' until they are
    // read again.
    let calls = |r: &Report, harness: &str| -> Vec<_> {
        r.groups
            .iter()
            .filter(|g| g.harness == harness)
            .map(|g| {
                (
                    g.repo.clone(),
                    g.semantic,
                    g.similar,
                    g.greps,
                    g.globs,
                    g.reads,
                )
            })
            .collect()
    };
    let on_disk = s.claude_file(&s.repo, "s7.jsonl");
    append(
        &on_disk,
        &[claude_line(
            "s7",
            &s.repo,
            1,
            "assistant",
            tool_use("g7", "Grep", json!({})),
        )],
    );
    s.scan();
    let before = calls(&s.report(), "claude");
    assert!(before.iter().any(|c| c.3 == 2), "{before:?}");
    s.register(&[&s.repo, &s.other]);
    transcripts::scan(&s.config, &[(Format::Codex, s.codex.clone())]).unwrap();
    assert_eq!(calls(&s.report(), "claude"), before);
}

/// `dir` as a `file://` URL, as Codex records command directories.
fn file_url(dir: &Path) -> String {
    url::Url::from_file_path(dir).unwrap().to_string()
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
                "cwd": file_url(&s.repo),
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
                "cwd": file_url(&s.other),
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

#[test]
fn shell_commands_count_as_the_searches_and_reads_they_make() {
    let s = Setup::new();
    // A linked worktree registered under its repository's name, since removed.
    let worktree = s.other.join("wt");
    fs::write(
        s.config.data_dir().join("registry.json"),
        json!({ "repos": [{ "root": s.repo, "name": "repo" },
                          { "root": worktree, "name": "repo:wt" }] })
        .to_string(),
    )
    .unwrap();
    let bash = |n: u32, id: &str, command: String| {
        let input = json!({ "command": command });
        claude_line("s1", &s.repo, n, "assistant", tool_use(id, "Bash", input))
    };
    append(
        &s.claude_file(&s.repo, "s1.jsonl"),
        &[
            claude_line(
                "s1",
                &s.repo,
                1,
                "assistant",
                tool_use(
                    "q1",
                    "mcp__lodestone__semantic_code_search",
                    json!({ "query": "a", "repo": "repo:wt" }),
                ),
            ),
            claude_line(
                "s1",
                &s.repo,
                2,
                "user",
                tool_result("q1", "core/src/a.rs:3:8  lines 3-5  pub fn a()"),
            ),
            // Reads a result and searches the repository.
            bash(
                3,
                "b1",
                "sed -n 1,40p core/src/a.rs; grep -rn foo core | head".into(),
            ),
            // Elsewhere: neither the search nor the read is the repository's. (A relative `cd`,
            // since Bash reads a Windows path's backslashes as escapes.)
            bash(
                4,
                "b2",
                "cd ../other && grep -c foo notes.json; cat notes.json".into(),
            ),
        ],
    );
    // A file that exists, so `../other` resolves out of the repository.
    fs::write(s.other.join("notes.json"), "{}").unwrap();
    s.scan();
    let r = s.report();
    assert_eq!(r.groups.len(), 1, "{:#?}", r.groups);
    let c = group(&r, "claude");
    assert_eq!(c.repo, s.repo.to_string_lossy());
    assert_eq!((c.semantic, c.searches_read), (1, 1));
    assert_eq!((c.greps, c.reads), (1, 1));
}

#[test]
fn counts_nudges_where_the_nudged_search_counts() {
    let s = Setup::new();
    // The session starts above the repository and searches it after `cd`.
    let top = s.repo.parent().unwrap();
    let nudge = |call: &str, timestamp: String, text: &str| {
        json!({ "type": "attachment", "sessionId": "s1", "cwd": top, "timestamp": timestamp,
            "attachment": { "type": "hook_additional_context", "hookEvent": "PreToolUse",
                "hookName": "PreToolUse:Bash", "toolUseID": call, "content": [text] } })
    };
    let grep = |n: u32, id: &str, dir: &str| {
        let input = json!({ "command": format!("cd {dir} && grep -rn 'a\\|b\\|c' .") });
        claude_line("s1", top, n, "assistant", tool_use(id, "Bash", input))
    };
    append(
        &s.claude_file(top, "s1.jsonl"),
        &[
            grep(1, "b1", "repo"),
            nudge("b1", ts(1), "This search tries 3 names. lodestone's …"),
            claude_line(
                "s1",
                top,
                3,
                "assistant",
                tool_use(
                    "q1",
                    "mcp__lodestone__semantic_code_search",
                    json!({ "query": "a" }),
                ),
            ),
            // Another hook's context is not a nudge.
            nudge("b1", ts(1), "Some other hook's context"),
            // Nudged later, with no search after it.
            grep(5, "b2", "repo"),
            nudge(
                "b2",
                "2026-11-11T10:20:00.000Z".into(),
                "This search tries 4 names.",
            ),
            // A search in no registered repository, like its nudge, is not kept.
            grep(6, "b3", "other"),
            nudge("b3", ts(6), "This search tries 3 names."),
        ],
    );
    s.scan();
    let r = s.report();
    assert_eq!(r.groups.len(), 1, "{:#?}", r.groups);
    let c = group(&r, "claude");
    assert_eq!(c.repo, s.repo.to_string_lossy());
    assert_eq!((c.nudges, c.nudges_searched), (2, 1));
    assert_eq!((c.greps, c.semantic), (2, 1));
}

#[test]
fn a_read_sent_alongside_a_search_is_not_a_read_of_its_results() {
    let s = Setup::new();
    let read = |n: u32, id: &str| {
        let input = json!({ "file_path": s.repo.join("core/src/a.rs") });
        claude_line("s1", &s.repo, n, "assistant", tool_use(id, "Read", input))
    };
    append(
        &s.claude_file(&s.repo, "s1.jsonl"),
        &[
            claude_line(
                "s1",
                &s.repo,
                1,
                "assistant",
                tool_use(
                    "q1",
                    "mcp__lodestone__semantic_code_search",
                    json!({ "query": "a" }),
                ),
            ),
            // Parallel tool calls: the read goes out before the search answers.
            read(2, "r1"),
            claude_line(
                "s1",
                &s.repo,
                3,
                "user",
                tool_result("q1", "core/src/a.rs:3:8  lines 3-5  pub fn a()"),
            ),
        ],
    );
    s.scan();
    let c = group(&s.report(), "claude").clone();
    assert_eq!((c.semantic, c.reads, c.searches_read), (1, 1, 0));
}

/// An Antigravity step: `content` for a result, `calls` (name, args) for a model step.
fn agy_step(index: u32, kind: &str, content: &str, calls: &[(&str, Value)]) -> Value {
    let calls: Vec<Value> = calls
        .iter()
        .map(|(name, args)| json!({ "name": name, "args": args }))
        .collect();
    json!({ "step_index": index, "type": kind, "status": "DONE", "created_at": ts(index),
            "content": content, "tool_calls": calls })
}

#[test]
fn reads_antigravity_conversations_and_their_workspaces() {
    let s = Setup::new();
    s.agy_workspace("c1", &s.repo);
    let file = s.repo.join("core/src/a.rs").to_string_lossy().into_owned();
    let search = json!({ "ServerName": "lodestone_lodestone", "ToolName": "semantic_code_search",
                         "Arguments": { "query": "a" } });
    append(
        &s.agy_transcript("c1"),
        &[
            agy_step(0, "USER_INPUT", "find a", &[]),
            agy_step(1, "PLANNER_RESPONSE", "", &[("call_mcp_tool", search)]),
        ],
    );
    // No workspace recorded: only its calls on the repository's files count.
    append(
        &s.agy_transcript("c2"),
        &[
            agy_step(0, "USER_INPUT", "read a", &[]),
            agy_step(
                1,
                "PLANNER_RESPONSE",
                "",
                &[("read_file", json!({ "Path": file }))],
            ),
        ],
    );
    // `transcript.jsonl` truncates results and is not read.
    append(
        &s.agy_transcript("c1").with_file_name("transcript.jsonl"),
        &[agy_step(0, "USER_INPUT", "find a", &[])],
    );
    assert_eq!(s.scan().files_read, 2);
    // The search's result arrives after the scan that read the search.
    append(
        &s.agy_transcript("c1"),
        &[
            agy_step(
                2,
                "GENERIC",
                &format!(
                    "Completed At: {}\ncore/src/a.rs:3:8  lines 3-5  pub fn a()",
                    ts(3)
                ),
                &[],
            ),
            agy_step(
                4,
                "PLANNER_RESPONSE",
                "",
                &[
                    ("view_file", json!({ "AbsolutePath": file })),
                    (
                        "run_command",
                        json!({ "CommandLine": "grep -rn foo core", "Cwd": s.repo }),
                    ),
                    // No directory of its own: runs in the workspace.
                    ("run_command", json!({ "CommandLine": "rg bar src" })),
                ],
            ),
        ],
    );
    assert_eq!(s.scan().files_read, 1);
    let r = s.report();
    assert_eq!(r.groups.len(), 1, "{:#?}", r.groups);
    let g = group(&r, "antigravity");
    assert_eq!(g.repo, s.repo.to_string_lossy());
    assert_eq!((g.sessions, g.sessions_searched), (2, 1));
    assert_eq!((g.semantic, g.searches_read, g.searches_empty), (1, 1, 0));
    assert_eq!((g.greps, g.reads), (2, 2));
    assert_eq!(g.latency_ms_median, Some(2000));
}

#[test]
fn rereads_an_antigravity_conversation_once_its_workspace_is_recorded() {
    let s = Setup::new();
    let grep = json!({ "CommandLine": "rg foo src" });
    append(
        &s.agy_transcript("c1"),
        &[
            agy_step(0, "USER_INPUT", "find foo", &[]),
            agy_step(1, "PLANNER_RESPONSE", "", &[("run_command", grep)]),
        ],
    );
    s.scan();
    assert!(s.report().groups.is_empty());
    s.agy_workspace("c1", &s.repo);
    assert_eq!(s.scan().files_read, 1);
    let r = s.report();
    let g = group(&r, "antigravity");
    assert_eq!((g.sessions, g.greps), (1, 1));
    assert_eq!(s.scan().unchanged, 1);
}

#[cfg(unix)]
#[test]
fn places_a_search_in_a_missing_directory_of_a_workspace_named_by_another_path() {
    // As Windows names a canonical path `\\?\C:\…` and a workspace URL's `C:\…`.
    let s = Setup::new();
    let link = s.other.join("link");
    std::os::unix::fs::symlink(&s.repo, &link).unwrap();
    s.agy_workspace("c1", &link);
    let grep = json!({ "CommandLine": "rg foo missing" });
    append(
        &s.agy_transcript("c1"),
        &[
            agy_step(0, "USER_INPUT", "find foo", &[]),
            agy_step(1, "PLANNER_RESPONSE", "", &[("run_command", grep)]),
        ],
    );
    s.scan();
    let r = s.report();
    let g = group(&r, "antigravity");
    assert_eq!((g.sessions, g.greps), (1, 1));
}
