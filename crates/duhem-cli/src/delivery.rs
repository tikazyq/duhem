//! Best-effort replication after the enclosing invocation has finished.

use std::{sync::Arc, time::Duration};

use duhem_evidence::{EventPayload, RunBundle, RunOrigin, RunStatus, SqliteStore, Store};

use crate::export_cmd::{HUB_TOKEN_ENV, HUB_URL_ENV, ship_bundle, ship_client};

pub struct Delivery {
    url: String,
    token: Option<String>,
    pub store: Option<Arc<SqliteStore>>,
    pub root: Option<String>,
    pub shard_parent: Option<String>,
}

impl Delivery {
    pub fn configured() -> Option<Self> {
        let url = std::env::var(HUB_URL_ENV).ok().filter(|s| !s.is_empty())?;
        Some(Self {
            url,
            token: std::env::var(HUB_TOKEN_ENV)
                .ok()
                .filter(|s| !s.trim().is_empty()),
            store: None,
            root: None,
            shard_parent: None,
        })
    }

    pub async fn send(self) {
        let (Some(store), Some(root)) = (self.store.as_deref(), self.root.as_deref()) else {
            return;
        };
        let result = tokio::time::timeout(Duration::from_secs(30), async {
            let client = ship_client(Duration::from_secs(10))?;
            self.send_tree(store, &client, root).await
        })
        .await;
        match result {
            Ok(Ok(())) => {}
            Ok(Err(error)) => eprintln!("warning: automatic evidence delivery failed: {error}"),
            Err(_) => eprintln!("warning: automatic evidence delivery exceeded 30 seconds"),
        }
    }

    async fn send_tree(
        &self,
        store: &SqliteStore,
        client: &reqwest::Client,
        root: &str,
    ) -> Result<(), String> {
        let mut pending = vec![(root.to_owned(), true)];
        if let Some(parent) = &self.shard_parent {
            // Do not traverse older siblings if this DB already ran another shard.
            pending.push((parent.clone(), false));
        }
        let mut failure = None;
        while let Some((id, descend)) = pending.pop() {
            let Some(record) = store.get_run(&id).await.map_err(|e| e.to_string())? else {
                continue;
            };
            let children = if descend {
                store.child_run_ids(&id).await.map_err(|e| e.to_string())?
            } else {
                Vec::new()
            };
            if descend && !matches!(record.status, RunStatus::Finished | RunStatus::Aborted) {
                // Nested explicit shards have their own logical container. An
                // unsealed execution is not an envelope we can deliver.
                if record.lineage.origin != Some(RunOrigin::Invocation) || children.is_empty() {
                    continue;
                }
                let events = store.run_events(&id).await.map_err(|e| e.to_string())?;
                if !matches!(events.as_slice(), [event] if matches!(&event.payload, EventPayload::RunStarted { definition: None, .. }))
                {
                    continue;
                }
            }
            let result = async {
                let bundle = RunBundle::from_store(store, &id)
                    .await
                    .map_err(|e| e.to_string())?;
                ship_bundle(client, &bundle, &self.url, self.token.as_deref()).await
            }
            .await;
            if let Err(error) = result {
                failure.get_or_insert_with(|| format!("run {id}: {error}"));
                if !descend {
                    // This is the shared parent of the only subtree in this invocation.
                    break;
                }
                continue;
            }
            pending.extend(children.into_iter().rev().map(|child| (child, true)));
        }
        failure.map_or(Ok(()), Err)
    }
}
