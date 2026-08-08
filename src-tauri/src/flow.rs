//! Flow orchestration primitives for planning and execution.

use std::{collections::BTreeMap, error::Error, fmt::Display, future::Future, pin::Pin};

use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::{
    agent::AgentKind,
    agents::{AgentInstance, AgentSpec},
    AgentPlan, AgentPlanStep, PlanStepStatus,
};

pub type FlowFuture<'a> = Pin<Box<dyn Future<Output = Result<String, FlowError>> + Send + 'a>>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FlowError {
    NoPrimaryAgent,
    NoExecutor(String),
    Cancelled,
    Agent(String),
    InvalidPlan(String),
}

impl Display for FlowError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoPrimaryAgent => formatter.write_str("No primary agent available."),
            Self::NoExecutor(step) => write!(formatter, "No executor available for step: {step}"),
            Self::Cancelled => formatter.write_str("Flow execution cancelled."),
            Self::Agent(error) => formatter.write_str(error),
            Self::InvalidPlan(error) => formatter.write_str(error),
        }
    }
}

impl Error for FlowError {}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BaseFlow {
    pub agents: BTreeMap<String, AgentSpec>,
    pub primary_agent_key: String,
}

impl BaseFlow {
    pub fn new(agents: impl IntoIterator<Item = (String, AgentSpec)>) -> Self {
        let agents = agents.into_iter().collect::<BTreeMap<_, _>>();
        let primary_agent_key = agents.keys().next().cloned().unwrap_or_default();
        Self {
            agents,
            primary_agent_key,
        }
    }

    pub fn from_specs(specs: impl IntoIterator<Item = AgentSpec>) -> Self {
        Self::new(specs.into_iter().map(|spec| (spec.key.clone(), spec)))
    }

    pub fn from_instance(instance: &AgentInstance) -> Self {
        Self::from_specs([instance.spec().clone()])
    }

    pub fn primary_agent(&self) -> Option<&AgentSpec> {
        self.agents.get(&self.primary_agent_key)
    }

    pub fn get_agent(&self, key: &str) -> Option<&AgentSpec> {
        self.agents.get(key)
    }

    pub fn add_agent(&mut self, key: impl Into<String>, agent: AgentSpec) {
        let key = key.into();
        if self.primary_agent_key.is_empty() {
            self.primary_agent_key = key.clone();
        }
        self.agents.insert(key, agent);
    }

    pub fn set_primary_agent(&mut self, key: impl Into<String>) -> Result<(), FlowError> {
        let key = key.into();
        if !self.agents.contains_key(&key) {
            return Err(FlowError::NoExecutor(key));
        }
        self.primary_agent_key = key;
        Ok(())
    }
}

pub trait Flow: Send {
    fn base(&self) -> &BaseFlow;
    fn base_mut(&mut self) -> &mut BaseFlow;
    fn execute<'a>(&'a mut self, input: &'a str, cancel: &'a CancellationToken) -> FlowFuture<'a>;

    fn primary_agent(&self) -> Option<&AgentSpec> {
        self.base().primary_agent()
    }

    fn get_agent(&self, key: &str) -> Option<&AgentSpec> {
        self.base().get_agent(key)
    }

    fn add_agent(&mut self, key: impl Into<String>, agent: AgentSpec) {
        self.base_mut().add_agent(key, agent);
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FlowType {
    Planning,
}

pub fn all_plan_statuses() -> Vec<String> {
    [
        PlanStepStatus::NotStarted,
        PlanStepStatus::InProgress,
        PlanStepStatus::Completed,
        PlanStepStatus::Blocked,
    ]
    .into_iter()
    .map(|status| {
        serde_json::to_string(&status)
            .unwrap_or_default()
            .trim_matches('"')
            .to_string()
    })
    .collect()
}

pub fn active_plan_statuses() -> Vec<String> {
    vec!["not_started".to_string(), "in_progress".to_string()]
}

pub fn plan_status_marks() -> BTreeMap<String, String> {
    BTreeMap::from([
        ("completed".to_string(), "[x]".to_string()),
        ("in_progress".to_string(), "[>]".to_string()),
        ("blocked".to_string(), "[!]".to_string()),
        ("not_started".to_string(), "[ ]".to_string()),
    ])
}

#[derive(Debug)]
pub struct PlanningFlow {
    pub base: BaseFlow,
    pub executor_keys: Vec<String>,
    pub active_plan_id: String,
    pub current_step_index: Option<usize>,
    pub active_plan: Option<AgentPlan>,
}

impl PlanningFlow {
    pub fn new(base: BaseFlow) -> Self {
        let executor_keys = base.agents.keys().cloned().collect();
        Self {
            base,
            executor_keys,
            active_plan_id: format!("plan_{}", Uuid::new_v4()),
            current_step_index: None,
            active_plan: None,
        }
    }

    pub fn with_agents(specs: impl IntoIterator<Item = AgentSpec>) -> Self {
        Self::new(BaseFlow::from_specs(specs))
    }

    pub fn get_executor(&self, step_type: Option<&str>) -> Option<&AgentSpec> {
        if let Some(step_type) = step_type.filter(|value| !value.is_empty()) {
            if let Some(agent) = self.base.get_agent(step_type) {
                return Some(agent);
            }
        }
        self.executor_keys
            .iter()
            .find_map(|key| self.base.get_agent(key))
            .or_else(|| self.base.primary_agent())
    }

    pub fn create_initial_plan(&mut self, request: &str) -> Result<&AgentPlan, FlowError> {
        if request.trim().is_empty() {
            return Err(FlowError::InvalidPlan(
                "Request cannot be empty.".to_string(),
            ));
        }
        let title = if request.chars().count() > 50 {
            format!("{}...", request.chars().take(50).collect::<String>())
        } else {
            request.to_string()
        };
        let steps = vec![
            AgentPlanStep {
                id: format!("{}_step_0", self.active_plan_id),
                title: "Analyze request".to_string(),
                description: "Understand constraints and choose the smallest useful path."
                    .to_string(),
                status: PlanStepStatus::NotStarted,
                notes: String::new(),
            },
            AgentPlanStep {
                id: format!("{}_step_1", self.active_plan_id),
                title: "Execute task".to_string(),
                description: "Use the selected agent and rust_ tools to produce evidence."
                    .to_string(),
                status: PlanStepStatus::NotStarted,
                notes: String::new(),
            },
            AgentPlanStep {
                id: format!("{}_step_2", self.active_plan_id),
                title: "Verify results".to_string(),
                description: "Check returned evidence and report limitations.".to_string(),
                status: PlanStepStatus::NotStarted,
                notes: String::new(),
            },
        ];
        self.active_plan = Some(AgentPlan {
            id: self.active_plan_id.clone(),
            title: format!("Plan for: {title}"),
            steps,
            created_at: now_millis(),
            updated_at: now_millis(),
        });
        Ok(self.active_plan.as_ref().expect("plan was just created"))
    }

    pub fn set_plan(&mut self, plan: AgentPlan) -> Result<(), FlowError> {
        if plan.id.trim().is_empty() || plan.steps.is_empty() {
            return Err(FlowError::InvalidPlan(
                "A plan requires an id and at least one step.".to_string(),
            ));
        }
        self.active_plan_id = plan.id.clone();
        self.active_plan = Some(plan);
        self.current_step_index = None;
        Ok(())
    }

    pub fn current_step_info(&mut self) -> Option<(usize, AgentPlanStep)> {
        let plan = self.active_plan.as_mut()?;
        let index = plan.steps.iter().position(|step| {
            matches!(
                step.status,
                PlanStepStatus::NotStarted | PlanStepStatus::InProgress
            )
        })?;
        plan.steps[index].status = PlanStepStatus::InProgress;
        plan.updated_at = now_millis();
        self.current_step_index = Some(index);
        Some((index, plan.steps[index].clone()))
    }

    pub fn mark_step_completed(&mut self, note: impl Into<String>) -> Result<(), FlowError> {
        self.mark_current_step(PlanStepStatus::Completed, note)
    }

    pub fn mark_step_blocked(&mut self, note: impl Into<String>) -> Result<(), FlowError> {
        self.mark_current_step(PlanStepStatus::Blocked, note)
    }

    pub fn mark_current_step(
        &mut self,
        status: PlanStepStatus,
        note: impl Into<String>,
    ) -> Result<(), FlowError> {
        let index = self
            .current_step_index
            .ok_or_else(|| FlowError::InvalidPlan("No current plan step.".to_string()))?;
        let plan = self
            .active_plan
            .as_mut()
            .ok_or_else(|| FlowError::InvalidPlan("No active plan.".to_string()))?;
        let step = plan
            .steps
            .get_mut(index)
            .ok_or_else(|| FlowError::InvalidPlan(format!("Invalid plan step index: {index}")))?;
        step.status = status;
        step.notes = note.into();
        plan.updated_at = now_millis();
        Ok(())
    }

    pub fn plan_text(&self) -> String {
        let Some(plan) = &self.active_plan else {
            return "No active plan.".to_string();
        };
        let completed = plan
            .steps
            .iter()
            .filter(|step| step.status == PlanStepStatus::Completed)
            .count();
        let marks = plan_status_marks();
        let mut text = format!(
            "Plan: {} (ID: {})\nProgress: {}/{} completed\nSteps:\n",
            plan.title,
            plan.id,
            completed,
            plan.steps.len()
        );
        for (index, step) in plan.steps.iter().enumerate() {
            let status = serde_json::to_string(&step.status)
                .unwrap_or_default()
                .trim_matches('"')
                .to_string();
            text.push_str(&format!(
                "{index}. {} {}\n",
                marks.get(&status).map(String::as_str).unwrap_or("[ ]"),
                step.title
            ));
            if !step.notes.is_empty() {
                text.push_str(&format!("   Notes: {}\n", step.notes));
            }
        }
        text
    }

    pub async fn execute_with<F, Fut>(
        &mut self,
        input: &str,
        mut runner: F,
        cancel: &CancellationToken,
    ) -> Result<String, FlowError>
    where
        F: FnMut(AgentSpec, String) -> Fut,
        Fut: Future<Output = Result<String, String>>,
    {
        if self.base.primary_agent().is_none() {
            return Err(FlowError::NoPrimaryAgent);
        }
        if self.active_plan.is_none() {
            self.create_initial_plan(input)?;
        }
        let mut outputs = Vec::new();
        while let Some((index, step)) = self.current_step_info() {
            if cancel.is_cancelled() {
                self.mark_step_blocked("Flow cancelled.")?;
                return Err(FlowError::Cancelled);
            }
            let executor = self
                .get_executor(None)
                .cloned()
                .ok_or_else(|| FlowError::NoExecutor(step.title.clone()))?;
            let prompt = format!(
                "CURRENT PLAN STATUS:\n{}\n\nYOUR CURRENT TASK:\nStep {index}: {}\n\nExecute only this step and summarize verified evidence.",
                self.plan_text(),
                step.title
            );
            match runner(executor, prompt).await {
                Ok(output) => {
                    self.mark_step_completed("Step completed by the selected agent.")?;
                    outputs.push(output);
                }
                Err(error) => {
                    self.mark_step_blocked(error.clone())?;
                    return Err(FlowError::Agent(error));
                }
            }
        }
        Ok(format!(
            "Plan completed.\n{}\n{}",
            self.plan_text(),
            outputs.join("\n\n")
        ))
    }
}

impl Flow for PlanningFlow {
    fn base(&self) -> &BaseFlow {
        &self.base
    }

    fn base_mut(&mut self) -> &mut BaseFlow {
        &mut self.base
    }

    fn execute<'a>(&'a mut self, input: &'a str, cancel: &'a CancellationToken) -> FlowFuture<'a> {
        Box::pin(async move {
            self.execute_with(
                input,
                |_agent, _prompt| async {
                    Err("PlanningFlow requires an agent runner.".to_string())
                },
                cancel,
            )
            .await
        })
    }
}

pub struct FlowFactory;

impl FlowFactory {
    pub fn create_flow(
        flow_type: FlowType,
        agents: impl IntoIterator<Item = AgentSpec>,
    ) -> PlanningFlow {
        match flow_type {
            FlowType::Planning => PlanningFlow::with_agents(agents),
        }
    }

    pub fn planning_for_kind(kind: AgentKind, workspace: &str) -> PlanningFlow {
        Self::create_flow(FlowType::Planning, [AgentSpec::for_kind(kind, workspace)])
    }
}

fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn planning_flow_executes_each_step_and_formats_progress() {
        let specs = AgentSpec::all(".");
        let mut flow = PlanningFlow::with_agents(specs);
        let output = flow
            .execute_with(
                "collect evidence",
                |agent, prompt| async move {
                    assert!(agent
                        .tool_names
                        .iter()
                        .all(|name| name.starts_with("rust_")));
                    assert!(prompt.contains("CURRENT PLAN STATUS"));
                    Ok(format!("{} completed", agent.name))
                },
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(output.contains("Plan completed"));
        assert!(output.contains("3/3 completed"));
        assert!(flow.current_step_index.is_some());
    }

    #[test]
    fn flow_factory_preserves_primary_agent() {
        let flow = FlowFactory::planning_for_kind(AgentKind::Browser, ".");
        assert_eq!(flow.primary_agent().unwrap().kind, AgentKind::Browser);
        assert_eq!(flow.get_executor(Some("browser")).unwrap().key, "browser");
        assert_eq!(active_plan_statuses(), vec!["not_started", "in_progress"]);
        assert_eq!(all_plan_statuses().len(), 4);
    }
}
