use serde::{Deserialize, Serialize};

/// The complete partition computed before a shard executes any work.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShardPlan {
    pub index: u32,
    pub total: u32,
    pub source: String,
    pub manifest_digest: String,
    pub assignments: Vec<ShardAssignment>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShardAssignment {
    pub shard: u32,
    #[serde(flatten)]
    pub unit: ShardUnit,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ShardUnit {
    Criterion {
        verification: String,
        id: String,
        checks: Vec<String>,
    },
    Native {
        #[serde(skip_serializing_if = "Option::is_none")]
        verification: Option<String>,
        id: String,
    },
}
