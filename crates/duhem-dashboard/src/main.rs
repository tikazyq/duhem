//! `duhem-dashboard` binary: serve mode (default) and `export`.
//!
//! Kept as a separate binary from `duhem` (the #53 alignment
//! decision) so the web server + SPA toolchain stays out of the core
//! CLI; `duhem dashboard` shells out to this (#87).
//!
//! The dashboard opens the evidence store **read-only** (#189): by
//! default the working copy's project DB
//! (`$XDG_STATE_HOME/duhem/projects/<slug>/duhem.db`, `DUHEM_HOME`
//! honored), or an explicit `--db` path.

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use clap::{Parser, Subcommand};
use duhem_dashboard::{DEFAULT_PORT, EvidenceReader, export, router};
use duhem_evidence::SqliteStore;

/// Read-only web dashboard over the Duhem evidence store.
#[derive(Debug, Parser)]
#[command(name = "duhem-dashboard", version)]
struct Cli {
    /// Evidence store (SQLite DB) to read runs from. Defaults to the
    /// working copy's project store under the duhem state dir.
    #[arg(long = "db", value_name = "PATH", global = true)]
    db: Option<PathBuf>,

    /// Listen port (serve mode).
    #[arg(long = "port", value_name = "PORT", default_value_t = DEFAULT_PORT)]
    port: u16,

    /// Listen host (serve mode). Loopback by default: the MVP has no
    /// auth, exposure is the operator's deliberate choice.
    #[arg(long = "host", value_name = "HOST", default_value = "127.0.0.1")]
    host: String,

    /// Accept run bundles at POST /api/ingest. DUHEM_HUB_TOKEN protects writes.
    #[arg(long)]
    ingest: bool,

    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// Render a self-contained static site (SPA bundle + JSON
    /// snapshots + artifacts) for upload to a file host.
    Export {
        /// Output directory (created if missing).
        #[arg(long = "out", value_name = "DIR")]
        out: PathBuf,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    rt.block_on(run(cli))
}

async fn run(cli: Cli) -> ExitCode {
    let db_path = match &cli.db {
        Some(p) => p.clone(),
        None => match std::env::current_dir()
            .map_err(duhem_evidence::StoreError::Io)
            .and_then(|cwd| duhem_evidence::project_db_path(&cwd))
        {
            Ok(p) => p,
            Err(e) => {
                eprintln!("resolve store: {e}");
                return ExitCode::FAILURE;
            }
        },
    };

    let token = std::env::var("DUHEM_HUB_TOKEN")
        .ok()
        .filter(|value| !value.is_empty());
    let writer = if cli.ingest && cli.cmd.is_none() {
        match SqliteStore::open(&db_path).await {
            Ok(store) => Some(Arc::new(store)),
            Err(error) => {
                eprintln!("open ingest store: {error}");
                return ExitCode::FAILURE;
            }
        }
    } else {
        None
    };
    // Every browsing route still uses SQLite mode=ro.
    let store = match SqliteStore::open_read_only(&db_path).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("open store: {e}");
            return ExitCode::FAILURE;
        }
    };
    let reader = EvidenceReader::new(Arc::new(store));

    match cli.cmd {
        Some(Cmd::Export { out }) => match export(&reader, &out).await {
            Ok(stats) => {
                println!(
                    "exported {} run(s), {} check page(s), {} artifact(s), {} SPA file(s) to {}",
                    stats.runs,
                    stats.checks,
                    stats.artifacts,
                    stats.spa_files,
                    out.display()
                );
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("export: {e:#}");
                ExitCode::FAILURE
            }
        },
        None => serve(reader, &db_path, &cli.host, cli.port, writer, token).await,
    }
}

async fn serve(
    reader: EvidenceReader,
    db_path: &std::path::Path,
    host: &str,
    port: u16,
    writer: Option<Arc<SqliteStore>>,
    token: Option<String>,
) -> ExitCode {
    let listener = match tokio::net::TcpListener::bind((host, port)).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("bind {host}:{port}: {e}");
            return ExitCode::FAILURE;
        }
    };
    if writer.is_some()
        && token.is_none()
        && !listener
            .local_addr()
            .is_ok_and(|address| address.ip().is_loopback())
    {
        eprintln!("ingest on a non-loopback address requires a non-empty DUHEM_HUB_TOKEN");
        return ExitCode::FAILURE;
    }
    let mut app = router(reader);
    if let Some(store) = writer {
        app = app.merge(duhem_dashboard::ingest::router(store, token));
    }
    let addr = listener
        .local_addr()
        .map(|a| a.to_string())
        .unwrap_or_default();
    println!(
        "duhem dashboard listening on http://{addr}/ (store: {})",
        db_path.display()
    );

    // Advertise the base URL next to the store (#298): `duhem run`
    // against the same DB reads this to print a clickable live-run
    // URL. Best-effort — a dashboard that can't write the file still
    // serves; a stale file after a crash is defused by the CLI's
    // connect probe.
    let addr_file = duhem_evidence::dashboard_addr_path(db_path);
    if !addr.is_empty()
        && let Err(e) = std::fs::write(&addr_file, format!("http://{addr}"))
    {
        eprintln!("advertise {}: {e}", addr_file.display());
    }

    let result = axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await;
    // Withdraw the advertisement on the way out (best-effort).
    let _ = std::fs::remove_file(&addr_file);
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("serve: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Resolve on Ctrl-C or SIGTERM so serve() unwinds normally and the
/// `dashboard.addr` advertisement is removed instead of left stale.
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }
}
