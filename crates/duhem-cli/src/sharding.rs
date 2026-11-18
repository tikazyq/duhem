//! CI-neutral shard discovery and deterministic selected-work planning (#663).
use std::path::Path;

use duhem_evidence::{ShardPlan, ShardUnit, plan_digest};
use duhem_runtime::CheckFilter;

use crate::filter::{CliCheckFilter, native_selection};
use crate::run_cmd::ResolvedLeaf;
use crate::run_scope::Scope;

pub(crate) struct ShardSettings {
    index: u32,
    total: u32,
    source: String,
    run_id: Option<String>,
    ci_identity: Option<String>,
}

fn env(key: &str) -> Option<String> {
    std::env::var(key).ok()
}

fn number(raw: &str, name: &str) -> Result<u32, String> {
    if raw.is_empty() || !raw.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!("{name}: expected an integer, got `{raw}`"));
    }
    raw.parse()
        .map_err(|_| format!("{name}: integer out of range `{raw}`"))
}

impl ShardSettings {
    /// Higher-precedence inputs shield lower-precedence providers entirely.
    pub(crate) fn resolve(
        flag: Option<String>,
        run_id: Option<String>,
    ) -> Result<Option<Self>, String> {
        let run_id = run_id.or_else(|| env("DUHEM_RUN_ID"));
        let explicit = flag.is_some();
        let (index, total, source, ci_identity) =
            if let Some(raw) = flag.or_else(|| env("DUHEM_SHARD")) {
                let source = if explicit { "flag" } else { "env" };
                let (index, total) = raw
                    .split_once('/')
                    .ok_or_else(|| format!("--shard / DUHEM_SHARD: expected k/n, got `{raw}`"))?;
                (
                    number(index, "shard index")?,
                    number(total, "shard total")?,
                    source,
                    None,
                )
            } else {
                let providers = [
                    (
                        "gitlab",
                        "CI_NODE_INDEX",
                        "CI_NODE_TOTAL",
                        "CI_PIPELINE_ID",
                        "CI_JOB_NAME",
                        false,
                    ),
                    (
                        "buildkite",
                        "BUILDKITE_PARALLEL_JOB",
                        "BUILDKITE_PARALLEL_JOB_COUNT",
                        "BUILDKITE_BUILD_ID",
                        "BUILDKITE_STEP_ID",
                        true,
                    ),
                    (
                        "circleci",
                        "CIRCLE_NODE_INDEX",
                        "CIRCLE_NODE_TOTAL",
                        "CIRCLE_WORKFLOW_ID",
                        "CIRCLE_JOB",
                        true,
                    ),
                ];
                let mut detected = None;
                for (source, index_key, total_key, run_key, job_key, zero_based) in providers {
                    let (index, total) = (env(index_key), env(total_key));
                    if index.is_none() && total.is_none() {
                        continue;
                    }
                    let index = number(
                        &index.ok_or_else(|| format!("missing {index_key}"))?,
                        index_key,
                    )?;
                    let total = number(
                        &total.ok_or_else(|| format!("missing {total_key}"))?,
                        total_key,
                    )?;
                    let index = if zero_based {
                        index.checked_add(1).ok_or("shard index overflow")?
                    } else {
                        index
                    };
                    let job = env(job_key).unwrap_or_default();
                    let job = if source == "gitlab" {
                        job.strip_suffix(&format!(" {index}/{total}"))
                            .unwrap_or(&job)
                            .to_string()
                    } else {
                        job
                    };
                    let identity = env(run_key).map(|run| format!("{source}:{run}:{job}"));
                    detected = Some((index, total, source, identity));
                    break;
                }
                let Some(detected) = detected else {
                    return Ok(None);
                };
                detected
            };
        if total == 0 || index == 0 || index > total {
            return Err(format!(
                "invalid shard {index}/{total}: require 1 <= index <= total and total > 0"
            ));
        }
        if run_id.as_ref().is_some_and(|id| id.is_empty()) {
            return Err("sharded --run-id / DUHEM_RUN_ID must not be empty".into());
        }
        // Explicit shard coordinates still belong to the current CI job unless
        // the operator supplies an explicit logical parent identity as well.
        let ci_identity = ci_identity.or_else(|| {
            for (provider, run_key, job_key) in [
                ("gitlab", "CI_PIPELINE_ID", "CI_JOB_NAME"),
                ("buildkite", "BUILDKITE_BUILD_ID", "BUILDKITE_STEP_ID"),
                ("circleci", "CIRCLE_WORKFLOW_ID", "CIRCLE_JOB"),
            ] {
                if let (Some(run), Some(job)) = (env(run_key), env(job_key)) {
                    let job = if provider == "gitlab" {
                        match (env("CI_NODE_INDEX"), env("CI_NODE_TOTAL")) {
                            (Some(k), Some(n)) => job
                                .strip_suffix(&format!(" {k}/{n}"))
                                .unwrap_or(&job)
                                .to_owned(),
                            _ => job,
                        }
                    } else {
                        job
                    };
                    return Some(format!("{provider}:{run}:{job}"));
                }
            }
            None
        });
        Ok(Some(Self {
            index,
            total,
            source: source.into(),
            run_id,
            ci_identity,
        }))
    }

    /// Filter first, then assign stable sorted units round-robin. Unit counts
    /// differ by at most one; a criterion's selected checks never separate.
    pub(crate) fn partition(
        self,
        target: &Path,
        leaves: &mut Vec<ResolvedLeaf>,
        scope: &mut Scope,
        filter: Option<&CliCheckFilter>,
        filters: &[String],
        profile: Option<&str>,
    ) -> Result<ShardPlan, String> {
        let canonical_target = target
            .canonicalize()
            .map_err(|e| format!("shard plan {}: {e}", target.display()))?;
        let root = canonical_target.parent().unwrap_or(Path::new("."));
        let mut units = Vec::new();
        let mut definitions = Vec::new();
        for (name, path, def, _, _) in leaves.iter_mut() {
            let relative = path
                .canonicalize()
                .map_err(|e| format!("shard plan {}: {e}", path.display()))?
                .strip_prefix(root)
                .map_err(|e| format!("shard path: {e}"))?
                .to_string_lossy()
                .replace('\\', "/");
            let narrowed = filter.and_then(|f| f.for_verification(name));
            for criterion in &mut def.criteria {
                criterion.checks.retain(|check| match filter {
                    None => true,
                    Some(_) => narrowed
                        .as_ref()
                        .is_some_and(|f| f.matches(&criterion.id, &check.id)),
                });
            }
            def.criteria
                .retain(|criterion| !criterion.checks.is_empty());
            def.native = native_selection(filter, &def.native).0;
            definitions.push(serde_json::json!([relative, def]));
            for criterion in &def.criteria {
                units.push(ShardUnit {
                    verification: Some(relative.clone()),
                    kind: "criterion".into(),
                    id: criterion.id.clone(),
                    shard: 0,
                });
            }
            for native in &def.native {
                units.push(ShardUnit {
                    verification: Some(relative.clone()),
                    kind: "native".into(),
                    id: native.id.clone(),
                    shard: 0,
                });
            }
        }
        if let Scope::Manifest {
            native,
            provision,
            defaults,
            project,
            inputs,
            ..
        } = scope
        {
            *native = native_selection(filter, native).0;
            definitions.push(serde_json::json!({"manifest_native": native,
                "provision": provision, "defaults": defaults, "project": project, "inputs": inputs}));
            for run in native.iter() {
                units.push(ShardUnit {
                    verification: None,
                    kind: "native".into(),
                    id: run.id.clone(),
                    shard: 0,
                });
            }
        }
        units.sort_by(|a, b| {
            (&a.verification, &a.kind, &a.id).cmp(&(&b.verification, &b.kind, &b.id))
        });
        for (ordinal, unit) in units.iter_mut().enumerate() {
            unit.shard = (ordinal % self.total as usize) as u32 + 1;
        }
        let manifest =
            std::fs::read(target).map_err(|e| format!("shard plan {}: {e}", target.display()))?;
        let manifest_digest = plan_digest(&manifest);
        definitions.sort_by_key(|definition| definition.to_string());
        let canonical = serde_json::to_vec(&serde_json::json!({
            "manifest_digest": manifest_digest, "definitions": definitions,
            "filters": filters, "profile": profile, "total": self.total, "units": units,
        }))
        .map_err(|e| format!("shard plan: {e}"))?;
        let digest = plan_digest(&canonical);
        let parent_run_id = self.run_id.unwrap_or_else(|| {
            let identity = format!("{}:{digest}", self.ci_identity.unwrap_or_default());
            format!("shard-{}", plan_digest(identity.as_bytes()))
        });
        let owns = |verification: Option<&str>, kind: &str, id: &str| {
            units.iter().any(|unit| {
                unit.shard == self.index
                    && unit.verification.as_deref() == verification
                    && unit.kind == kind
                    && unit.id == id
            })
        };
        for (_, path, def, _, _) in leaves.iter_mut() {
            let relative = path
                .canonicalize()
                .map_err(|e| format!("shard plan {}: {e}", path.display()))?
                .strip_prefix(root)
                .map_err(|e| format!("shard path: {e}"))?
                .to_string_lossy()
                .replace('\\', "/");
            def.criteria
                .retain(|c| owns(Some(&relative), "criterion", &c.id));
            def.native
                .retain(|run| owns(Some(&relative), "native", &run.id));
        }
        leaves.retain(|(_, _, def, _, _)| !def.criteria.is_empty() || !def.native.is_empty());
        if let Scope::Manifest { native, .. } = scope {
            native.retain(|run| owns(None, "native", &run.id));
        }
        Ok(ShardPlan {
            parent_run_id,
            index: self.index,
            total: self.total,
            source: self.source,
            manifest_digest,
            plan_digest: digest,
            units,
        })
    }
}
