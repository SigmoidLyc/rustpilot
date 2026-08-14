use std::future::Future;

use serde_json::Value;

use crate::AgentMemoryEntry;

use super::{context::ContextBudget, surface};

#[derive(Debug)]
pub(crate) enum SummaryFailure {
    Cancelled,
    Unavailable(String),
}

#[derive(Debug, Clone)]
pub(crate) struct PreparedSurface {
    pub(crate) entries: Vec<AgentMemoryEntry>,
    pub(crate) before_tokens: usize,
    pub(crate) after_tokens: usize,
    pub(crate) changed_ids: Vec<String>,
    pub(crate) summary: Option<String>,
    pub(crate) summary_fallback: bool,
}

pub(crate) fn prepare_prune(
    entries: &[AgentMemoryEntry],
    budget: ContextBudget,
) -> Option<PreparedSurface> {
    let before_tokens = surface::estimate_entries(entries);
    if before_tokens <= budget.pressure_limit() {
        return None;
    }
    let pruned = surface::prune_tool_results(entries, budget.pressure_limit());
    if pruned.changed_ids.is_empty() || pruned.after_tokens > budget.pressure_limit() {
        return None;
    }
    Some(PreparedSurface {
        entries: pruned.entries,
        before_tokens,
        after_tokens: pruned.after_tokens,
        changed_ids: pruned.changed_ids,
        summary: None,
        summary_fallback: false,
    })
}

pub(crate) fn build_plan(
    entries: &[AgentMemoryEntry],
    budget: ContextBudget,
) -> Option<surface::CompactionPlan> {
    surface::build_compaction_plan(entries, budget)
}

pub(crate) async fn finish_plan<F, Fut>(
    plan: surface::CompactionPlan,
    system_messages: &[Value],
    budget: ContextBudget,
    summarize: F,
) -> Result<PreparedSurface, SummaryFailure>
where
    F: FnOnce(Vec<Value>) -> Fut,
    Fut: Future<Output = Result<String, SummaryFailure>>,
{
    let summary_input = crate::agent_loop::summary::request_messages(
        system_messages,
        &plan.summary_entries,
        budget,
    );
    let (summary, summary_fallback) = match summarize(summary_input).await {
        Ok(summary) => (summary, false),
        Err(SummaryFailure::Cancelled) => return Err(SummaryFailure::Cancelled),
        Err(SummaryFailure::Unavailable(reason)) => {
            tracing::warn!(error = %reason, "Context summary provider unavailable; using deterministic checkpoint");
            (
                crate::agent_loop::summary::fallback_summary(
                    &plan.source_entries,
                    plan.shadowed_tokens,
                ),
                true,
            )
        }
    };
    let mut summary = crate::agent_loop::summary::normalize(&summary, plan.shadowed_tokens);
    let mut summary_fallback = summary_fallback;
    let (mut entries, mut changed_ids) = surface::finalize_compaction(&plan, &summary, budget);
    if let Err(error) = surface::validate_compaction_result(&plan, &entries, budget) {
        let fallback = crate::agent_loop::summary::normalize(
            &crate::agent_loop::summary::fallback_summary(
                &plan.source_entries,
                plan.shadowed_tokens,
            ),
            plan.shadowed_tokens,
        );
        summary = fallback.clone();
        summary_fallback = true;
        (entries, changed_ids) = surface::finalize_compaction(&plan, &fallback, budget);
        if let Err(fallback_error) = surface::validate_compaction_result(&plan, &entries, budget) {
            return Err(SummaryFailure::Unavailable(format!(
                "Context compaction invariant failed ({error}); fallback failed ({fallback_error})."
            )));
        }
    }
    let after_tokens = surface::estimate_entries(&entries);
    Ok(PreparedSurface {
        entries,
        before_tokens: plan.before_tokens,
        after_tokens,
        changed_ids,
        summary: Some(summary),
        summary_fallback,
    })
}

pub(crate) fn fallback_surface(
    entries: &[AgentMemoryEntry],
    budget: ContextBudget,
) -> PreparedSurface {
    let before_tokens = surface::estimate_entries(entries);
    let pruned = surface::prune_tool_results(entries, budget.input_limit());
    PreparedSurface {
        entries: pruned.entries,
        before_tokens,
        after_tokens: pruned.after_tokens,
        changed_ids: pruned.changed_ids,
        summary: None,
        summary_fallback: false,
    }
}
