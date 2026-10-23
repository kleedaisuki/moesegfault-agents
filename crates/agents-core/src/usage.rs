use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageKind {
    ModelInputTokens,
    ModelCachedInputTokens,
    ModelOutputTokens,
    ContainerVcpuMilliseconds,
    ContainerMemoryMibMilliseconds,
    ContainerDiskMibMilliseconds,
    R2StoredByteHours,
    R2ClassAOperations,
    R2ClassBOperations,
    D1RowsRead,
    D1RowsWritten,
    DurableObjectRequests,
    DurableObjectGbMilliseconds,
}

/// Append once by id. The price revision makes past invoices reproducible.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageEvent {
    pub id: String,
    pub owner_id: String,
    pub session_id: Option<String>,
    pub run_id: Option<String>,
    pub kind: UsageKind,
    pub quantity: u64,
    pub cost_micro_usd: u64,
    pub price_revision: String,
    pub occurred_at_ms: u64,
    pub byok: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub output_tokens: u64,
}

impl TokenUsage {
    pub fn uncached_input_tokens(self) -> u64 {
        self.input_tokens.saturating_sub(self.cached_input_tokens)
    }
}
