//! Portable deterministic shard plans (#663).
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// One indivisible selected unit, assigned to a one-based shard index.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShardUnit {
    /// Manifest-relative leaf path; `None` denotes manifest-native work.
    pub verification: Option<String>,
    pub kind: String,
    pub id: String,
    pub shard: u32,
}

/// The complete selection and partition, repeated in every shard's evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShardPlan {
    pub parent_run_id: String,
    pub index: u32,
    pub total: u32,
    pub source: String,
    pub manifest_digest: String,
    pub plan_digest: String,
    pub units: Vec<ShardUnit>,
}

/// Stable SHA-256 identity for canonical plan bytes.
pub fn plan_digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
