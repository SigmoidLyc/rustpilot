use serde_json::Value;

/// A durable, append-only record for changes to the model-visible context.
///
/// The full transcript lives in the task snapshot. These records only describe
/// how a derived surface was changed, so they remain small enough to keep
/// forever and can be audited independently from UI event retention.
#[derive(Debug, Clone)]
pub(crate) struct ContextEventRecord {
    pub(crate) kind: String,
    pub(crate) compaction_id: String,
    pub(crate) generation: u64,
    pub(crate) source_start: Option<String>,
    pub(crate) source_end: Option<String>,
    pub(crate) source_hash: String,
    pub(crate) shadowed_tokens: usize,
    pub(crate) surface_tokens: usize,
    pub(crate) occurred_at: i64,
    pub(crate) payload: Value,
}

impl ContextEventRecord {
    pub(crate) fn new(
        kind: impl Into<String>,
        compaction_id: impl Into<String>,
        generation: u64,
        source_start: Option<String>,
        source_end: Option<String>,
        source_hash: impl Into<String>,
        shadowed_tokens: usize,
        surface_tokens: usize,
        occurred_at: i64,
        payload: Value,
    ) -> Self {
        Self {
            kind: kind.into(),
            compaction_id: compaction_id.into(),
            generation,
            source_start,
            source_end,
            source_hash: source_hash.into(),
            shadowed_tokens,
            surface_tokens,
            occurred_at,
            payload,
        }
    }
}
