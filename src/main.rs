use std::path::PathBuf;

use anyhow::Context;
use clap::{Parser, Subcommand};

use lodestone::agents::{self, Harness, Scope, Target};
use lodestone::api::types::{DEFAULT_TOP_K, IndexRequest, RegisterRequest, SearchRequest};
use lodestone::bench::{self, BenchOptions};
use lodestone::client::{self, Client};
use lodestone::config::{Backend, Config};
use lodestone::embed;
use lodestone::index::IndexOptions;
use lodestone::registry::Engine;
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
    /// Register a worktree: canonicalize, add to the registry, watch, index if needed.
    Register { path: PathBuf },
    /// Index a worktree (incremental unless --force).
    Index {
        path: PathBuf,
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
    /// Index freshness for a worktree.
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
    /// Install the skill, MCP server and SessionStart reminder into Claude Code and/or Codex.
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
}

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
            rt.block_on(lodestone::api::serve(config, on_demand))
        }
        Cmd::Mcp => {
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(agents::run_relay(config, cli.config))
        }
        Cmd::Register { path } => {
            let path = std::path::absolute(&path)?;
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
            let path = std::path::absolute(&path)?;
            if let Some(c) = daemon(true)? {
                let resp = c.index(&IndexRequest {
                    repo: Some(path.to_string_lossy().to_string()),
                    force,
                })?;
                println!("{}: {}", resp.repo, describe(&resp.report));
                return Ok(());
            }
            let engine = Engine::from_env(config)?;
            let w = engine.register(&path)?;
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
                repo,
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
        Cmd::Hook(HookCmd::SessionStart) => {
            let dir = session_dir()?;
            if let Some(out) = agents::session_start(&dir, &config, cli.config.as_deref()) {
                println!("{out}");
            }
            Ok(())
        }
        Cmd::Status { repo, json } => {
            let st = match daemon(false)? {
                Some(c) => c.status(repo.as_deref())?,
                None => Engine::from_env(config)?.status(repo.as_deref())?,
            };
            if json {
                println!("{}", serde_json::to_string_pretty(&st)?);
            } else {
                println!("{}", st.to_text());
            }
            Ok(())
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
fn session_dir() -> anyhow::Result<PathBuf> {
    use std::io::{IsTerminal, Read};
    let mut input = String::new();
    if !std::io::stdin().is_terminal() {
        std::io::stdin().read_to_string(&mut input)?;
    }
    let cwd = serde_json::from_str::<serde_json::Value>(&input)
        .ok()
        .and_then(|v| v["cwd"].as_str().map(PathBuf::from));
    Ok(
        match cwd.or_else(|| std::env::var_os("CLAUDE_PROJECT_DIR").map(PathBuf::from)) {
            Some(dir) => dir,
            None => std::env::current_dir()?,
        },
    )
}
