use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context;
use clap::{Parser, Subcommand};

use lodestone::agents::{self, Harness, HookEvent, Scope, Target};
use lodestone::api::types::{
    DEFAULT_TOP_K, IndexRequest, RegisterRequest, SearchRequest, StatusOverview, StatusReport,
};
use lodestone::bench::{self, BenchOptions};
use lodestone::client::{self, Client};
use lodestone::config::{Backend, Config};
use lodestone::embed;
use lodestone::index::IndexOptions;
use lodestone::registry::{self, Engine};
use lodestone::transcripts;

#[derive(Parser)]
#[command(
    name = "lode",
    version,
    about = "Local semantic code search for coding agents"
)]
struct Cli {
    /// Config file (default: ~/.lodestone/config.toml).
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    /// Run in-process: neither use nor start a daemon.
    #[arg(long, global = true)]
    local: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the daemon (REST + MCP on 127.0.0.1).
    Serve {
        #[arg(long)]
        port: Option<u16>,
        /// Watch only worktrees agent sessions use, and exit `idle_timeout_secs` after the last
        /// session ends (how `lode mcp` and the session hook start the daemon).
        #[arg(long)]
        on_demand: bool,
    },
    /// MCP server over stdio for agent harnesses: relays to the daemon, starting it if needed.
    Mcp,
    /// Stop the running daemon: it finishes in-flight requests, then exits.
    Stop,
    /// Register a worktree: canonicalize, add to the registry, watch, index if needed. Without a
    /// path, the git checkout of the current directory.
    Register { path: Option<PathBuf> },
    /// Index a worktree (incremental unless --force): the registered worktree containing `path`,
    /// by default the current directory.
    Index {
        path: Option<PathBuf>,
        #[arg(long)]
        force: bool,
    },
    /// Search a registered worktree.
    Search {
        query: String,
        #[arg(long)]
        repo: Option<String>,
        #[arg(long)]
        from_unit: Option<String>,
        #[arg(long)]
        language: Option<String>,
        #[arg(long, default_value_t = DEFAULT_TOP_K)]
        top_k: usize,
        #[arg(long)]
        include_tests: bool,
        #[arg(long)]
        path_prefix: Option<String>,
        #[arg(long)]
        with_body: Option<usize>,
        #[arg(long)]
        json: bool,
    },
    /// Index freshness for a worktree: the one named by --repo, else the current one, else a
    /// summary of every registered repository.
    Status {
        #[arg(long)]
        repo: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// rank@k on a question file, index time and query latency.
    Bench {
        eval_dir: PathBuf,
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        backend: Option<Backend>,
        /// Corpus to index (default: <eval-dir>/corpus).
        #[arg(long)]
        corpus: Option<PathBuf>,
        /// Question file (default: <eval-dir>/questions.json).
        #[arg(long)]
        questions: Option<PathBuf>,
        /// Comma-separated question ids that count as hard cases.
        #[arg(long, value_delimiter = ',')]
        hard: Vec<String>,
        #[arg(long)]
        skip_index: bool,
        #[arg(long)]
        force: bool,
        /// Enable the reranker (off by default unless the config enables it).
        #[arg(long, conflicts_with = "no_rerank")]
        rerank: bool,
        /// Disable the reranker for this run.
        #[arg(long)]
        no_rerank: bool,
        /// Cross-encoder to rerank with (Hugging Face repo); implies `--rerank`.
        #[arg(long, conflicts_with = "no_rerank")]
        reranker: Option<String>,
        /// Candidates passed to the reranker (overrides `rerank.candidates`); implies `--rerank`.
        #[arg(long, conflicts_with = "no_rerank")]
        rerank_candidates: Option<usize>,
        /// Include test items (down-weighted by `search.test_weight`).
        #[arg(long)]
        include_tests: bool,
        #[arg(long)]
        json: bool,
    },
    /// Configured embedding models and available backends.
    Models,
    /// Built-in grammars, grammars found via tree-sitter's config, and the extension map.
    Languages,
    /// Install the skill, MCP server and reminder hooks into Claude Code and/or Codex.
    Install(HarnessArgs),
    /// Remove what `install` added.
    Uninstall(HarnessArgs),
    /// Hooks that agent harnesses run.
    #[command(subcommand)]
    Hook(HookCmd),
    /// How agents use lodestone, from Claude Code and Codex transcripts. Reads only sessions in
    /// registered repositories, and records what it read so later runs read only new lines.
    #[command(group(clap::ArgGroup::new("source").required(true).multiple(true)))]
    TranscriptStats {
        /// Read Claude Code transcripts [default dir: ~/.claude/projects]
        #[arg(long, value_name = "DIR", group = "source")]
        claude: Option<Option<PathBuf>>,
        /// Read Codex transcripts [default dir: ~/.codex/sessions]
        #[arg(long, value_name = "DIR", group = "source")]
        codex: Option<Option<PathBuf>>,
        /// Only this repository (root path or directory name).
        #[arg(long)]
        repo: Option<String>,
        /// Only sessions started on or after this date (YYYY-MM-DD, UTC).
        #[arg(long, value_name = "DATE")]
        since: Option<String>,
        /// Also list every search with its query.
        #[arg(long)]
        queries: bool,
        #[arg(long)]
        json: bool,
    },
}

#[derive(clap::Args)]
struct HarnessArgs {
    /// Harness to (un)install for; repeatable (default: every harness whose CLI is on PATH).
    #[arg(long, value_enum)]
    client: Vec<Harness>,
    /// `user` (every session) or `project` (files to commit in a repository).
    #[arg(long, value_enum, default_value = "user")]
    scope: Scope,
    /// Repository for `--scope project` (default: the enclosing git repository).
    #[arg(long)]
    project: Option<PathBuf>,
    /// Print the steps without applying them.
    #[arg(long)]
    dry_run: bool,
}

#[derive(Subcommand)]
enum HookCmd {
    /// Print the lodestone reminder in a registered repository; start the daemon if needed.
    SessionStart,
    /// The same, on a session's first prompt only, unless a session-start hook already ran.
    PromptSubmit,
}

/// How long `lode stop` waits for the daemon to exit.
const STOP_WAIT: Duration = Duration::from_secs(30);

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("LODESTONE_LOG")
                .unwrap_or_else(|_| "warn".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    let cli = Cli::parse();
    let mut config = Config::load(cli.config.as_deref())?;
    // `search`, `register` and `index` start an on-demand daemon when none runs, like `lode mcp`;
    // `status` only uses one that is already running.
    let daemon = |start: bool| -> anyhow::Result<Option<Client>> {
        Ok(match (cli.local, start) {
            (true, _) => None,
            (false, true) => Some(client::connect_or_start(
                &config,
                cli.config.as_deref(),
                client::START_WAIT,
            )?),
            (false, false) => Client::connect(&config.endpoint()),
        })
    };
    match cli.cmd {
        Cmd::Serve { port, on_demand } => {
            if let Some(p) = port {
                config.port = Some(p);
            }
            let rt = tokio::runtime::Runtime::new()?;
            let served = rt.block_on(lodestone::api::serve(config, on_demand));
            // Don't wait for an indexing pass still running: the index is consistent after every
            // batch, and the next pass picks up where this one stopped.
            rt.shutdown_background();
            served
        }
        Cmd::Stop => {
            let Some(c) = Client::connect(&config.endpoint()) else {
                println!("no lodestone daemon running");
                return Ok(());
            };
            c.stop()?;
            client::wait_until_stopped(&config, STOP_WAIT)?;
            println!("stopped");
            Ok(())
        }
        Cmd::Mcp => {
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(agents::run_relay(config, cli.config))
        }
        Cmd::Register { path } => {
            let path = match path {
                Some(p) => std::path::absolute(&p)?,
                None => {
                    let cwd = std::env::current_dir()?;
                    registry::checkout_top(&cwd).unwrap_or(cwd)
                }
            };
            if let Some(c) = daemon(true)? {
                let info = c.register(&RegisterRequest {
                    path: path.to_string_lossy().to_string(),
                })?;
                println!(
                    "registered {} ({}); indexing in the daemon (`lode status` shows progress)",
                    info.root, info.name
                );
                return Ok(());
            }
            let engine = Engine::from_env(config)?;
            let w = engine.register(&path)?;
            let report = engine.index_blocking(&w, IndexOptions::default())?;
            println!("registered {}; {}", w.root.display(), describe(&report));
            Ok(())
        }
        Cmd::Index { path, force } => {
            let path = match path {
                Some(p) => std::path::absolute(&p)?,
                None => indexed_here(&config)?,
            };
            if let Some(c) = daemon(true)? {
                let resp = c.index(&IndexRequest {
                    repo: Some(path.to_string_lossy().to_string()),
                    force,
                })?;
                println!("{}: {}", resp.repo, describe(&resp.report));
                return Ok(());
            }
            let engine = Engine::from_env(config)?;
            let w = engine.index_target(&path)?;
            let progress = spawn_progress(w.clone());
            let report = engine.index_blocking(&w, IndexOptions { force, paths: None });
            progress.store(true, std::sync::atomic::Ordering::SeqCst);
            println!("{}: {}", w.root.display(), describe(&report?));
            Ok(())
        }
        Cmd::Search {
            query,
            repo,
            from_unit,
            language,
            top_k,
            include_tests,
            path_prefix,
            with_body,
            json,
        } => {
            let req = SearchRequest {
                query,
                repo: repo.or_else(|| here(&config)),
                from_unit,
                language,
                top_k,
                include_tests,
                path_prefix,
                kinds: Vec::new(),
                with_body,
            };
            let resp = match daemon(true)? {
                Some(c) => c.search(&req)?,
                None => Engine::from_env(config)?.search(&req)?,
            };
            if json {
                println!("{}", serde_json::to_string_pretty(&resp)?);
            } else {
                println!("{}", resp.to_text());
            }
            Ok(())
        }
        Cmd::Install(args) => harness_command(&config, args, true),
        Cmd::Uninstall(args) => harness_command(&config, args, false),
        Cmd::TranscriptStats {
            claude,
            codex,
            repo,
            since,
            queries,
            json,
        } => {
            let sources: Vec<(Harness, PathBuf)> =
                [(Harness::Claude, claude), (Harness::Codex, codex)]
                    .into_iter()
                    .filter_map(|(h, dir)| {
                        Some((h, dir?.unwrap_or_else(|| transcripts::default_dir(h))))
                    })
                    .collect();
            let since = since
                .map(|s| transcripts::parse_date(&s).context("--since takes a date: YYYY-MM-DD"))
                .transpose()?;
            let read = transcripts::scan(&config, &sources)?;
            eprintln!(
                "read {} new lines from {} transcripts ({} unchanged)",
                read.lines, read.files_read, read.unchanged
            );
            let filter = transcripts::Filter {
                harnesses: sources
                    .iter()
                    .map(|(h, _)| h.program().to_string())
                    .collect(),
                repo,
                since,
            };
            let report = transcripts::report(&config, &filter)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                println!("{}", report.to_text(queries));
            }
            Ok(())
        }
        Cmd::Hook(cmd) => {
            let event = match cmd {
                HookCmd::SessionStart => HookEvent::SessionStart,
                HookCmd::PromptSubmit => HookEvent::UserPromptSubmit,
            };
            let input = HookInput::read()?;
            let out = agents::hook(
                event,
                &input.dir,
                input.session.as_deref(),
                &config,
                cli.config.as_deref(),
            );
            if let Some(out) = out {
                println!("{out}");
            }
            Ok(())
        }
        Cmd::Status { repo, json } => {
            let view = match (repo.or_else(|| here(&config)), daemon(false)?) {
                (Some(r), Some(c)) => StatusView::Repo(Box::new(c.status(Some(&r))?)),
                (Some(r), None) => {
                    StatusView::Repo(Box::new(Engine::from_env(config)?.status(Some(&r))?))
                }
                (None, Some(c)) => c.status_all()?.into(),
                (None, None) => Engine::from_env(config)?.status_all().into(),
            };
            view.print(json)
        }
        Cmd::Bench {
            eval_dir,
            model,
            backend,
            corpus,
            questions,
            hard,
            skip_index,
            force,
            rerank,
            no_rerank,
            reranker,
            rerank_candidates,
            include_tests,
            json,
        } => {
            if let Some(m) = model {
                config.model = m;
            }
            if let Some(b) = backend {
                config.backend = b;
            }
            let rerank = rerank || reranker.is_some() || rerank_candidates.is_some();
            if let Some(repo) = reranker {
                config.rerank.hf_repo = repo;
            }
            if let Some(n) = rerank_candidates {
                config.rerank.candidates = n;
            }
            config.rerank.enabled = (config.rerank.enabled || rerank) && !no_rerank;
            let (default_corpus, default_questions) = bench::default_paths(&eval_dir);
            let opts = BenchOptions {
                corpus: corpus.unwrap_or(default_corpus),
                questions: questions.unwrap_or(default_questions),
                hard,
                skip_index,
                force,
                include_tests,
            };
            anyhow::ensure!(
                opts.corpus.is_dir(),
                "corpus {} not found (see eval/README.md)",
                opts.corpus.display()
            );
            let engine = Engine::from_env(config)?;
            let report = bench::run(&engine, &opts).context("bench")?;
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                println!("{}", report.to_text());
            }
            Ok(())
        }
        Cmd::Models => {
            println!("configured: {} on {}", config.model, config.backend);
            println!(
                "backends: {}",
                embed::available_backends()
                    .iter()
                    .map(|b| b.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            for (alias, m) in &config.models {
                println!(
                    "{alias:<16} {:<40} dim {:<4} max_tokens {:<5} {:?}/{:?} fastembed:{} license:{}",
                    m.hf_repo,
                    m.dim,
                    m.max_tokens,
                    m.arch,
                    m.pooling,
                    m.fastembed.as_deref().unwrap_or("-"),
                    m.license
                );
            }
            Ok(())
        }
        Cmd::Languages => {
            let reg =
                lodestone::chunk::LanguageRegistry::load(&config.languages, &config.data_dir());
            for l in reg.info() {
                println!(
                    "{:<12} {:<9} .{}{}",
                    l.name,
                    if l.origin == "builtin" {
                        "builtin"
                    } else {
                        "external"
                    },
                    l.extensions.join(" ."),
                    if l.origin == "builtin" {
                        String::new()
                    } else {
                        format!("  ({})", l.origin)
                    }
                );
            }
            println!("text: .{}", reg.text_extensions().join(" ."));
            for issue in &reg.issues {
                println!("issue: {issue}");
            }
            Ok(())
        }
    }
}

/// What `lode status` prints: one repository in detail, or a summary of all of them.
enum StatusView {
    Repo(Box<StatusReport>),
    All(StatusOverview),
}

impl From<StatusOverview> for StatusView {
    /// A lone registered repository is shown in detail, as if it were named.
    fn from(mut overview: StatusOverview) -> Self {
        if overview.repos.len() == 1 && overview.failed.is_empty() {
            Self::Repo(Box::new(overview.repos.remove(0)))
        } else {
            Self::All(overview)
        }
    }
}

impl StatusView {
    fn print(&self, json: bool) -> anyhow::Result<()> {
        let out = match (self, json) {
            (Self::Repo(st), true) => serde_json::to_string_pretty(st)?,
            (Self::All(overview), true) => serde_json::to_string_pretty(overview)?,
            (Self::Repo(st), false) => st.to_text(),
            (Self::All(overview), false) => overview.to_text(),
        };
        println!("{out}");
        Ok(())
    }
}

fn describe(r: &lodestone::index::IndexReport) -> String {
    format!(
        "{} files scanned, {} changed, {} removed, {} items, {} embedded, {} cached, {:.1}s",
        r.files_scanned,
        r.files_changed,
        r.files_removed,
        r.items,
        r.chunks_embedded,
        r.chunks_cached,
        r.secs
    )
}

/// Prints indexing progress to stderr every few seconds until the returned flag is set.
fn spawn_progress(
    w: std::sync::Arc<lodestone::index::WorktreeIndex>,
) -> std::sync::Arc<std::sync::atomic::AtomicBool> {
    let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = done.clone();
    std::thread::spawn(move || {
        while !flag.load(std::sync::atomic::Ordering::SeqCst) {
            std::thread::sleep(std::time::Duration::from_secs(5));
            let p = w.progress();
            if p.running && !flag.load(std::sync::atomic::Ordering::SeqCst) {
                eprintln!(
                    "files {}/{}  embedded {}  cached {}  eta {}s",
                    p.files_done,
                    p.files_total,
                    p.chunks_embedded,
                    p.chunks_cached,
                    p.eta_secs.unwrap_or(0)
                );
            }
        }
    });
    done
}

fn harness_command(config: &Config, args: HarnessArgs, add: bool) -> anyhow::Result<()> {
    let mut clients = args.client;
    if clients.is_empty() {
        clients = Harness::ALL
            .into_iter()
            .filter(|h| on_path(h.program()))
            .collect();
        anyhow::ensure!(
            !clients.is_empty(),
            "neither claude nor codex is on PATH; pass --client"
        );
    }
    let cwd = std::env::current_dir()?;
    let project = match args.project {
        Some(p) => std::path::absolute(p)?,
        None => cwd
            .ancestors()
            .find(|d| d.join(".git").exists())
            .unwrap_or(&cwd)
            .to_path_buf(),
    };
    let target = Target {
        scope: args.scope,
        home: dirs::home_dir().context("no home directory")?,
        project,
        exe: std::env::current_exe()?,
    };
    let steps = if add {
        agents::install(&clients, &target)
    } else {
        agents::uninstall(&clients, &target)
    };
    for step in &steps {
        println!("{step}");
        if !args.dry_run {
            agents::apply(step)?;
        }
    }
    if add && !args.dry_run {
        print_next_steps(config, &clients, args.scope);
    }
    Ok(())
}

fn print_next_steps(config: &Config, clients: &[Harness], scope: Scope) {
    println!(
        "
Next: register repositories (`lode register <path>`). Agent sessions start the daemon \
         on demand; it exits {} min after the last session ends.",
        config.idle_timeout_secs / 60
    );
    if clients.contains(&Harness::Codex) {
        println!("Codex: run /hooks once to trust the lodestone SessionStart hook.");
        if scope == Scope::Project {
            println!("Codex: the project must be trusted for .codex/ to load.");
        }
    }
    if clients.contains(&Harness::Claude) && scope == Scope::Project {
        println!("Claude Code: approve the `lodestone` server from .mcp.json on first use.");
    }
}

fn on_path(program: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(program).is_file()))
}

/// The session's directory: `cwd` from the hook's stdin JSON (Claude Code and Codex both send
/// it), else `CLAUDE_PROJECT_DIR`, else the current directory.
/// The current directory, as the repository to use when none is named, if it is in a git
/// checkout or a registered directory: its own index, or an error, never another checkout's.
/// Elsewhere, `None` leaves the choice to the daemon (the only registered repository).
fn here(config: &Config) -> Option<String> {
    let cwd = std::env::current_dir().ok()?;
    let placement = registry::place(&registry::registered_roots(config), &cwd);
    (placement != registry::Placement::Outside).then(|| cwd.to_string_lossy().into_owned())
}

/// The current directory, for `lode index` without a path: it must be in a registered worktree
/// (or a linked worktree of a registered repository, which gets registered).
fn indexed_here(config: &Config) -> anyhow::Result<PathBuf> {
    let cwd = std::env::current_dir()?;
    match registry::place(&registry::registered_roots(config), &cwd) {
        registry::Placement::Registered(_) | registry::Placement::NewWorktree(_) => Ok(cwd),
        _ => anyhow::bail!(
            "{} is not in a registered worktree; run `lode register` here first, or pass a path",
            cwd.display()
        ),
    }
}

/// What a harness passes a hook on stdin.
struct HookInput {
    /// The session's directory.
    dir: PathBuf,
    /// The harness's session id.
    session: Option<String>,
}

impl HookInput {
    fn read() -> anyhow::Result<Self> {
        use std::io::{IsTerminal, Read};
        let mut input = String::new();
        if !std::io::stdin().is_terminal() {
            std::io::stdin().read_to_string(&mut input)?;
        }
        let json = serde_json::from_str::<serde_json::Value>(&input).unwrap_or_default();
        let cwd = json["cwd"].as_str().map(PathBuf::from);
        let dir = match cwd.or_else(|| std::env::var_os("CLAUDE_PROJECT_DIR").map(PathBuf::from)) {
            Some(dir) => dir,
            None => std::env::current_dir()?,
        };
        Ok(Self {
            dir,
            session: json["session_id"].as_str().map(String::from),
        })
    }
}
