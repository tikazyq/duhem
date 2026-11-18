use std::env;
use std::path::Path;

use duhem_evidence::{RunLineage, RunOrigin, Sha256Hex, ShardAssignment, ShardPlan, ShardUnit};
use duhem_runtime::CheckFilter;
use duhem_schema::{NativeRun, RootManifest};

use crate::filter::{CliCheckFilter, native_selection};
use crate::run_cmd::ResolvedLeaf;

pub(crate) struct ShardSettings {
    index: u32,
    total: u32,
    source: &'static str,
    parent: Option<String>,
    ci_identity: Option<(String, String)>,
}

impl ShardSettings {
    pub(crate) fn resolve(
        flag: Option<&str>,
        run_id: Option<&str>,
    ) -> Result<Option<Self>, String> {
        if env::var_os(duhem_runtime::PARENT_RUN_ID_ENV).is_some_and(|id| !id.is_empty()) {
            return if flag.is_some() {
                Err("--shard cannot be used by a nested Duhem invocation".into())
            } else {
                Ok(None)
            };
        }
        let mut selected = if let Some(raw) = flag {
            Self::parse(raw, "--shard")?
        } else if let Some(raw) = read_env("DUHEM_SHARD")? {
            Self::parse(&raw, "DUHEM_SHARD")?
        } else if let Some(settings) = detect_ci()? {
            settings
        } else {
            return Ok(None);
        };
        selected.parent = match run_id {
            Some(id) => Some(id.to_owned()),
            None => read_env("DUHEM_RUN_ID")?,
        };
        if selected.parent.as_deref() == Some("") {
            return Err("shard parent run ID must not be empty".into());
        }
        Ok(Some(selected))
    }

    fn parse(raw: &str, source: &'static str) -> Result<Self, String> {
        let (index, total) = raw.split_once('/').ok_or_else(|| invalid(source))?;
        Self::from_pair(index, total, source, false)
    }

    fn from_pair(
        index: &str,
        total: &str,
        source: &'static str,
        zero_based: bool,
    ) -> Result<Self, String> {
        let parse = |raw: &str| {
            if !raw.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err(invalid(source));
            }
            raw.parse::<u32>().map_err(|_| invalid(source))
        };
        let index = parse(index)?;
        let total = parse(total)?;
        let index = if zero_based {
            index.checked_add(1).ok_or_else(|| invalid(source))?
        } else {
            index
        };
        if index == 0 || index > total {
            return Err(invalid(source));
        }
        Ok(Self {
            index,
            total,
            source,
            parent: None,
            ci_identity: None,
        })
    }
}

fn invalid(source: &str) -> String {
    format!("{source}: invalid shard; expected k/n with 1 <= k <= n (CI indexes may be zero-based)")
}

fn read_env(name: &str) -> Result<Option<String>, String> {
    match env::var(name) {
        Ok(value) => Ok(Some(value)),
        Err(env::VarError::NotPresent) => Ok(None),
        Err(env::VarError::NotUnicode(_)) => Err(format!("{name} must be valid UTF-8")),
    }
}

fn detect_ci() -> Result<Option<ShardSettings>, String> {
    for (source, index_var, total_var, zero_based, run_var, job_var) in [
        (
            "gitlab",
            "CI_NODE_INDEX",
            "CI_NODE_TOTAL",
            false,
            "CI_PIPELINE_ID",
            "CI_JOB_NAME",
        ),
        (
            "buildkite",
            "BUILDKITE_PARALLEL_JOB",
            "BUILDKITE_PARALLEL_JOB_COUNT",
            true,
            "BUILDKITE_BUILD_ID",
            "BUILDKITE_STEP_ID",
        ),
        (
            "circleci",
            "CIRCLE_NODE_INDEX",
            "CIRCLE_NODE_TOTAL",
            true,
            "CIRCLE_WORKFLOW_ID",
            "CIRCLE_JOB",
        ),
    ] {
        let index = read_env(index_var)?;
        let total = read_env(total_var)?;
        let (index, total) = match (index.as_deref(), total.as_deref()) {
            (None, None) => continue,
            (None, Some("1")) if source == "gitlab" => continue,
            (Some(index), Some(total)) => (index, total),
            _ => {
                return Err(format!(
                    "{source}: both {index_var} and {total_var} are required for sharding"
                ));
            }
        };
        let mut settings = ShardSettings::from_pair(index, total, source, zero_based)?;
        if let (Some(run), Some(mut job)) = (read_env(run_var)?, read_env(job_var)?) {
            if source == "gitlab" {
                let suffix = format!(" {index}/{total}");
                if job.ends_with(&suffix) {
                    job.truncate(job.len() - suffix.len());
                }
            }
            settings.ci_identity = Some((run, job));
        }
        return Ok(Some(settings));
    }
    Ok(None)
}

pub(crate) fn manifest_digest(manifest: &RootManifest) -> Result<String, serde_yml::Error> {
    let yaml = serde_yml::to_string(manifest)?;
    Ok(Sha256Hex::of(yaml.as_bytes()).0)
}

/// Partition the post-filter work in authored order. One ordinal covers both
/// criteria and native commands, so shard sizes differ by at most one unit.
pub(crate) fn partition(
    settings: ShardSettings,
    target: &Path,
    manifest: Option<(&str, &Path, &[NativeRun])>,
    filter: Option<&CliCheckFilter>,
    profile: Option<&str>,
    leaves: &mut Vec<ResolvedLeaf>,
) -> Result<(RunLineage, Vec<NativeRun>), duhem_schema::SchemaError> {
    let mut plan = ShardPlan {
        index: settings.index,
        total: settings.total,
        source: settings.source.into(),
        manifest_digest: match manifest {
            Some((digest, _, _)) => digest.to_owned(),
            None => Sha256Hex::of(leaves[0].2.to_yaml_string()?.as_bytes()).0,
        },
        assignments: Vec::new(),
    };
    let mut assign = |unit| {
        let shard = (plan.assignments.len() % settings.total as usize) as u32 + 1;
        plan.assignments.push(ShardAssignment { shard, unit });
        shard == settings.index
    };
    let mut suite_native = Vec::new();
    if let Some((_, _, native)) = manifest {
        suite_native = native_selection(filter, native).0;
        suite_native.retain(|run| {
            assign(ShardUnit::Native {
                verification: None,
                id: run.id.clone(),
            })
        });
    }
    let mut leaf_digests = Vec::new();
    for (name, path, def, _, _) in leaves.iter_mut() {
        let verification = match manifest {
            Some((_, root, _)) => path.strip_prefix(root).expect("manifest-relative leaf"),
            None => Path::new(target.file_name().expect("loaded a file")),
        }
        .iter()
        .map(|part| part.to_string_lossy())
        .collect::<Vec<_>>()
        .join("/");
        leaf_digests.push(Sha256Hex::of(def.to_yaml_string()?.as_bytes()).0);
        let criteria_filter = filter.and_then(|filter| filter.for_verification(name));
        def.criteria.retain_mut(|criterion| {
            if filter.is_some() {
                criterion.checks.retain(|check| {
                    criteria_filter
                        .as_ref()
                        .is_some_and(|filter| filter.matches(&criterion.id, &check.id))
                });
            }
            !criterion.checks.is_empty()
                && assign(ShardUnit::Criterion {
                    verification: verification.clone(),
                    id: criterion.id.clone(),
                    checks: criterion
                        .checks
                        .iter()
                        .map(|check| check.id.clone())
                        .collect(),
                })
        });
        def.native = native_selection(filter, &def.native).0;
        def.native.retain(|run| {
            assign(ShardUnit::Native {
                verification: Some(verification.clone()),
                id: run.id.clone(),
            })
        });
    }
    leaves.retain(|(_, _, def, _, _)| !def.criteria.is_empty() || !def.native.is_empty());
    let parent = settings.parent.unwrap_or_else(|| {
        let identity = serde_json::to_vec(&(
            "duhem-shard-v1",
            &plan.manifest_digest,
            leaf_digests,
            profile,
            plan.total,
            &plan.assignments,
            settings.ci_identity,
        ))
        .expect("shard plan is JSON serializable");
        format!("shard-{}", Sha256Hex::of(&identity).as_str())
    });
    Ok((
        RunLineage {
            parent_run_id: Some(parent),
            origin: Some(RunOrigin::Shard),
            shard: Some(plan),
        },
        suite_native,
    ))
}
