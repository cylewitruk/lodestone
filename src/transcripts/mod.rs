//! `lode transcript-stats`: how agents use lodestone, from Claude Code and Codex transcripts.
//!
//! Only sessions in registered repositories are read. Reading is incremental: `transcripts.sqlite`
//! in the data directory keeps what was extracted and how far each transcript was read (byte
//! offset of the last complete line, size, mtime and a hash of its first bytes), so a later run
//! skips unchanged files, reads only new lines of grown ones, and re-reads rewritten ones.

mod claude;
mod codex;
mod report;
mod store;
mod time;

use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::agents::Harness;
use crate::config::Config;
use crate::index::mtime_nanos;
use crate::registry;
pub use report::{Filter, Group, Report, Search};
use store::{FileState, Store};
pub use time::parse_date;

/// Bytes hashed from the start of a transcript to notice it being rewritten.
const HEAD_BYTES: u64 = 4096;

/// What a tool call did, as far as the statistics care.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// `semantic_code_search`.
    Semantic,
    /// `similar_code_search`.
    Similar,
    /// `search`, the single tool before the semantic/similar split.
    Search,
    Grep,
    Glob,
    Read,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Kind::Semantic => "semantic",
            Kind::Similar => "similar",
            Kind::Search => "search",
            Kind::Grep => "grep",
            Kind::Glob => "glob",
            Kind::Read => "read",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        [
            Kind::Semantic,
            Kind::Similar,
            Kind::Search,
            Kind::Grep,
            Kind::Glob,
            Kind::Read,
        ]
        .into_iter()
        .find(|k| k.as_str() == s)
    }

    /// A lodestone MCP tool, by its name.
    fn lodestone(tool: &str) -> Option<Self> {
        match tool {
            "semantic_code_search" => Some(Kind::Semantic),
            "similar_code_search" => Some(Kind::Similar),
            "search" => Some(Kind::Search),
            _ => None,
        }
    }

    fn is_lodestone(self) -> bool {
        matches!(self, Kind::Semantic | Kind::Similar | Kind::Search)
    }
}

/// A tool call: `done` is its finish time, result text and error flag, when the harness logs the
/// call and its result together.
#[derive(Clone, Debug, PartialEq)]
pub struct Call {
    id: String,
    session: String,
    kind: Kind,
    ts: i64,
    query: Option<String>,
    path: Option<String>,
    done: Option<(i64, String, bool)>,
}

/// What a parser extracts from one transcript line.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// The session `id` (a subagent's parent session) runs in `cwd`.
    Session {
        id: String,
        cwd: PathBuf,
        ts: i64,
    },
    Call(Call),
    /// The result of call `id` arrived.
    Done {
        id: String,
        ts: i64,
        text: String,
        is_error: bool,
    },
}

/// Where a harness keeps its transcripts by default.
pub fn default_dir(h: Harness) -> PathBuf {
    let home = dirs::home_dir().unwrap_or_default();
    let config = |var: &str, dir: &str| {
        std::env::var_os(var)
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(dir))
    };
    match h {
        Harness::Claude => config("CLAUDE_CONFIG_DIR", ".claude").join("projects"),
        Harness::Codex => config("CODEX_HOME", ".codex").join("sessions"),
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ScanSummary {
    pub files_read: usize,
    pub lines: usize,
    pub unchanged: usize,
}

/// Reads what is new in the transcripts under each source directory.
pub fn scan(config: &Config, sources: &[(Harness, PathBuf)]) -> anyhow::Result<ScanSummary> {
    fs::create_dir_all(config.data_dir())?;
    let store = Store::open(&config.data_dir().join("transcripts.sqlite"))?;
    let roots = registry::registered_roots(config);
    let mut summary = ScanSummary::default();
    for (harness, dir) in sources {
        let files = match harness {
            Harness::Claude => claude::files(dir, &roots),
            Harness::Codex => jsonl_files(dir),
        };
        for path in files {
            read_file(&store, *harness, &path, &roots, &mut summary)?;
        }
    }
    Ok(summary)
}

/// Loads the statistics recorded so far.
pub fn report(config: &Config, filter: &Filter) -> anyhow::Result<Report> {
    let store = Store::open(&config.data_dir().join("transcripts.sqlite"))?;
    report::build(&store.sessions()?, &store.calls()?, filter)
}

fn read_file(
    store: &Store,
    harness: Harness,
    path: &Path,
    roots: &[PathBuf],
    summary: &mut ScanSummary,
) -> anyhow::Result<()> {
    let meta = fs::metadata(path)?;
    let key = path.to_string_lossy().into_owned();
    let saved = store.file(&key)?;
    let registered = |cwd: &str| registry::root_of(roots, Path::new(cwd));
    if let Some(s) = &saved {
        let skip = match s.cwd.as_deref() {
            // Stopped at a session outside every registered repository: wait for one.
            Some(cwd) if s.offset == 0 => registered(cwd).is_none(),
            _ => s.size == meta.len() && s.mtime == mtime_nanos(&meta),
        };
        if skip {
            summary.unchanged += 1;
            return Ok(());
        }
    }
    let mut file = File::open(path)?;
    let mut state = match saved {
        Some(s) if s.offset <= meta.len() && head(&mut file, s.head_len)? == s.head => s,
        Some(_) => {
            store.forget_calls(&key)?;
            FileState::default()
        }
        None => FileState::default(),
    };
    state.size = meta.len();
    state.mtime = mtime_nanos(&meta);
    let mut session = state
        .cwd
        .as_deref()
        .and_then(registered)
        .and(state.session.clone());
    let mut codex = codex::Context {
        session: state.session.clone(),
    };

    file.seek(SeekFrom::Start(state.offset))?;
    let mut reader = BufReader::new(file);
    let mut line = Vec::new();
    store.begin()?;
    'lines: loop {
        line.clear();
        let n = reader.read_until(b'\n', &mut line)?;
        // A missing newline is a line still being written; it is read next time.
        if n == 0 || line.last() != Some(&b'\n') {
            break;
        }
        let text = String::from_utf8_lossy(&line);
        let events = match harness {
            Harness::Claude => claude::parse(&text),
            Harness::Codex => codex::parse(&text, &mut codex),
        };
        for event in events {
            match event {
                Event::Session { id, cwd, ts } => {
                    let cwd = cwd.to_string_lossy().into_owned();
                    let Some(root) = registered(&cwd) else {
                        // Not ours: remember where it runs, read nothing further.
                        state = FileState {
                            offset: 0,
                            cwd: Some(cwd),
                            ..state
                        };
                        break 'lines;
                    };
                    let id = format!("{}:{id}", harness.program());
                    if session.as_deref() != Some(&id) {
                        store.add_session(&id, harness.program(), &root.to_string_lossy(), ts)?;
                    }
                    state.cwd = Some(cwd);
                    state.session = Some(id.clone());
                    session = Some(id);
                }
                Event::Call(c) => {
                    if let Some(s) = &session {
                        store.add_call(&key, s, &c)?;
                    }
                }
                Event::Done {
                    id,
                    ts,
                    text,
                    is_error,
                } => store.finish_call(&key, &id, ts, &text, is_error)?,
            }
        }
        state.offset += n as u64;
        summary.lines += 1;
    }
    if state.head_len < HEAD_BYTES.min(state.offset) {
        state.head_len = HEAD_BYTES.min(state.offset);
        state.head = head(reader.get_mut(), state.head_len)?;
    }
    store.save_file(&key, &state)?;
    store.commit()?;
    summary.files_read += 1;
    Ok(())
}

/// Hash of the first `len` bytes.
fn head(file: &mut File, len: u64) -> anyhow::Result<Vec<u8>> {
    let mut buf = Vec::new();
    file.seek(SeekFrom::Start(0))?;
    file.take(len).read_to_end(&mut buf)?;
    Ok(blake3::hash(&buf).as_bytes().to_vec())
}

/// Every `*.jsonl` file under `dir`.
fn jsonl_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(&d).into_iter().flatten().flatten() {
            let p = e.path();
            match e.file_type() {
                Ok(t) if t.is_dir() => stack.push(p),
                Ok(t) if t.is_file() && p.extension().is_some_and(|x| x == "jsonl") => out.push(p),
                _ => {}
            }
        }
    }
    out.sort();
    out
}

/// A tool result's text: a string, or the text blocks of a content list.
fn text_of(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|b| b["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// What a lodestone search was asked: the sentence and guessed names, or the start of the code.
fn query_of(kind: Kind, input: &Value) -> String {
    match kind {
        Kind::Similar => {
            let code = input["code"].as_str().unwrap_or_default();
            let first = code
                .lines()
                .find(|l| !l.trim().is_empty())
                .unwrap_or_default();
            first.trim().chars().take(200).collect()
        }
        _ => {
            let query = input["query"].as_str().unwrap_or_default();
            let ids: Vec<&str> = input["example_identifiers"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect();
            if ids.is_empty() {
                query.to_string()
            } else {
                format!("{query} [{}]", ids.join(", "))
            }
        }
    }
}
