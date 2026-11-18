//! `duhem export` (#189) and `duhem ship` (#194): two destinations
//! for one format — the [`duhem_evidence::RunBundle`].
//!
//! `export` writes the human-browsable directory layout; `ship`
//! POSTs the canonical JSON envelope to a hub's ingest endpoint,
//! keyed by the bundle's content hash so re-shipping the same run is
//! a server-side no-op. Both the opt-in OSS dashboard receiver and the
//! hosted hub use this contract (`tests/bundle_contract.rs` pins it).

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use duhem_evidence::{RunBundle, SqliteStore};

/// Environment variables the ship step reads. The URL names the hub
/// ingest endpoint; the token authenticates (the OAuth/session flow
/// that mints it is the hub's concern).
pub const HUB_URL_ENV: &str = "DUHEM_HUB_URL";
pub const HUB_TOKEN_ENV: &str = "DUHEM_HUB_TOKEN";

async fn open_and_bundle(run_id: &str, db: Option<&Path>) -> Result<RunBundle, String> {
    let db_path = match db {
        Some(p) => p.to_path_buf(),
        None => {
            let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
            duhem_evidence::project_db_path(&cwd).map_err(|e| e.to_string())?
        }
    };
    // Read-only: neither export nor ship may mutate the store.
    let store = SqliteStore::open_read_only(&db_path)
        .await
        .map_err(|e| e.to_string())?;
    RunBundle::from_store(&store, run_id)
        .await
        .map_err(|e| e.to_string())
}

pub async fn run_export(run_id: &str, db: Option<&Path>, out: Option<&Path>) -> ExitCode {
    let bundle = match open_and_bundle(run_id, db).await {
        Ok(b) => b,
        Err(e) => {
            eprintln!("export: {e}");
            return ExitCode::FAILURE;
        }
    };
    let out_dir = out
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from(format!("duhem-export-{run_id}")));
    match bundle.write_dir(&out_dir) {
        Ok(()) => {
            println!("exported run {run_id} to {}", out_dir.display());
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("export: {e}");
            ExitCode::FAILURE
        }
    }
}

/// `duhem ship <run-id>`: POST the bundle to the hub. Without a hub
/// URL configured this is a clean no-op success — the CI ship step
/// can run unconditionally and only bites when the operator opted in.
pub async fn run_ship(
    run_id: &str,
    db: Option<&Path>,
    hub_url: Option<&str>,
    quiet_unconfigured: bool,
) -> ExitCode {
    let url = hub_url
        .map(str::to_string)
        .or_else(|| std::env::var(HUB_URL_ENV).ok().filter(|v| !v.is_empty()));
    let Some(url) = url else {
        if quiet_unconfigured {
            println!("ship: no hub configured ({HUB_URL_ENV} unset); skipping");
            return ExitCode::SUCCESS;
        }
        eprintln!("ship: no hub URL — pass --hub-url or set {HUB_URL_ENV}");
        return ExitCode::FAILURE;
    };

    let bundle = match open_and_bundle(run_id, db).await {
        Ok(b) => b,
        Err(e) => {
            eprintln!("ship: {e}");
            return ExitCode::FAILURE;
        }
    };
    match ship_bundle(&bundle, &url, std::env::var(HUB_TOKEN_ENV).ok().as_deref()).await {
        Ok(hash) => {
            println!("shipped run {run_id} to {url} (content hash {hash})");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("ship: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Deliver only this invocation and its descendants, including logical ancestors.
/// Nested processes defer to their enclosing invocation. The deadline includes
/// reading/assembling bundles as well as transport; the verdict is already fixed.
pub async fn deliver_tree(store: &dyn duhem_evidence::Store, roots: &[String]) {
    let Ok(url) = std::env::var(HUB_URL_ENV) else {
        return;
    };
    if url.is_empty() || roots.is_empty() {
        return;
    }
    let token = std::env::var(HUB_TOKEN_ENV).ok();
    let delivery = async {
        let runs = store.list_runs().await.map_err(|e| e.to_string())?;
        let mut selected: std::collections::BTreeSet<String> = roots.iter().cloned().collect();
        // Descendants of invocation roots, and ancestors required by their FKs.
        loop {
            let before = selected.len();
            for run in &runs {
                if run
                    .lineage
                    .parent_run_id
                    .as_ref()
                    .is_some_and(|id| selected.contains(id))
                {
                    selected.insert(run.run_id.clone());
                }
            }
            if selected.len() == before {
                break;
            }
        }
        loop {
            let before = selected.len();
            for run in &runs {
                if selected.contains(&run.run_id)
                    && let Some(parent) = &run.lineage.parent_run_id
                {
                    selected.insert(parent.clone());
                }
            }
            if selected.len() == before {
                break;
            }
        }
        let mut sent = std::collections::BTreeSet::new();
        while sent.len() < selected.len() {
            let mut advanced = false;
            for run in &runs {
                if !selected.contains(&run.run_id)
                    || sent.contains(&run.run_id)
                    || run
                        .lineage
                        .parent_run_id
                        .as_ref()
                        .is_some_and(|id| !sent.contains(id))
                {
                    continue;
                }
                let bundle = RunBundle::from_store(store, &run.run_id)
                    .await
                    .map_err(|e| e.to_string())?;
                ship_bundle(&bundle, &url, token.as_deref()).await?;
                sent.insert(run.run_id.clone());
                advanced = true;
            }
            if !advanced {
                return Err("run lineage is incomplete or cyclic".to_string());
            }
        }
        Ok::<_, String>(())
    };
    let error = match tokio::time::timeout(std::time::Duration::from_secs(30), delivery).await {
        Ok(Ok(())) => return,
        Ok(Err(error)) => error,
        Err(_) => "30 s tree delivery deadline exceeded".into(),
    };
    eprintln!("warning: automatic run delivery failed: {error}");
}

/// The ingest client (#194): one POST of the canonical envelope.
/// Idempotency: the `X-Duhem-Content-Hash` header carries the
/// bundle's hash; a hub that has it already answers 2xx without
/// re-storing. Conflicting run identities are delivery failures.
pub async fn ship_bundle(
    bundle: &RunBundle,
    url: &str,
    token: Option<&str>,
) -> Result<String, String> {
    let body = bundle.wire_bytes().map_err(|e| e.to_string())?;
    let hash = bundle.content_hash().map_err(|e| e.to_string())?;
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .build()
        .map_err(|e| e.to_string())?;
    let mut req = client
        .post(url)
        .header("content-type", "application/json")
        .header("x-duhem-bundle-version", bundle.bundle_version.to_string())
        .header("x-duhem-content-hash", &hash)
        .body(body);
    if let Some(token) = token {
        req = req.bearer_auth(token);
    }
    let res = req.send().await.map_err(|e| e.to_string())?;
    if !res.status().is_success() {
        let status = res.status();
        let text = res.text().await.unwrap_or_default();
        return Err(format!("hub answered {status}: {}", text.trim()));
    }
    Ok(hash)
}
