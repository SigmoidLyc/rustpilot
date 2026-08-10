use serde_json::Value;

use crate::{
    mark_task_revision, new_id, now, queue_current_plan, string_argument, truncate_output,
    AgentPlan, AgentPlanStep, AppState, PlanStepStatus,
};

pub(crate) fn format_plan(plan: &AgentPlan) -> String {
    let completed = plan
        .steps
        .iter()
        .filter(|step| step.status == PlanStepStatus::Completed)
        .count();
    let in_progress = plan
        .steps
        .iter()
        .filter(|step| step.status == PlanStepStatus::InProgress)
        .count();
    let blocked = plan
        .steps
        .iter()
        .filter(|step| step.status == PlanStepStatus::Blocked)
        .count();
    let mut output = format!(
        "Plan: {} (ID: {})\nProgress: {}/{} completed\nStatus: {} completed, {} in progress, {} blocked\nSteps:\n",
        plan.title,
        plan.id,
        completed,
        plan.steps.len(),
        completed,
        in_progress,
        blocked
    );
    for (index, step) in plan.steps.iter().enumerate() {
        let symbol = match step.status {
            PlanStepStatus::NotStarted => "[ ]",
            PlanStepStatus::InProgress => "[>]",
            PlanStepStatus::Completed => "[x]",
            PlanStepStatus::Blocked => "[!]",
        };
        output.push_str(&format!("{index}. {symbol} {}\n", step.title));
        if !step.description.is_empty() && step.description != step.title {
            output.push_str(&format!("   detail: {}\n", step.description));
        }
        if !step.notes.is_empty() {
            output.push_str(&format!("   notes: {}\n", step.notes));
        }
    }
    output
}

fn parse_plan_status(value: &str) -> Result<PlanStepStatus, String> {
    match value {
        "not_started" => Ok(PlanStepStatus::NotStarted),
        "in_progress" => Ok(PlanStepStatus::InProgress),
        "completed" => Ok(PlanStepStatus::Completed),
        "blocked" => Ok(PlanStepStatus::Blocked),
        _ => Err(format!("Invalid plan step status: {value}")),
    }
}

pub(crate) async fn run(
    state: &AppState,
    task_id: &str,
    arguments: &Value,
) -> Result<String, String> {
    let command = string_argument(arguments, "command")
        .ok_or_else(|| "rust_planning requires command".to_string())?;
    let output = {
        let mut tasks = state
            .tasks
            .write()
            .map_err(|_| "Task lock is poisoned".to_string())?;
        let task = tasks
            .get_mut(task_id)
            .ok_or_else(|| "Task not found".to_string())?;
        match command.as_str() {
            "create" => {
                let plan_id = string_argument(arguments, "plan_id")
                    .ok_or_else(|| "plan_id is required for create".to_string())?;
                if task.plans.iter().any(|plan| plan.id == plan_id) {
                    return Err(format!("A plan with ID '{plan_id}' already exists."));
                }
                let title = string_argument(arguments, "title")
                    .ok_or_else(|| "title is required for create".to_string())?;
                let steps = arguments
                    .get("steps")
                    .and_then(Value::as_array)
                    .ok_or_else(|| "steps must be a non-empty array for create".to_string())?;
                if steps.is_empty() {
                    return Err("steps must be a non-empty array for create".to_string());
                }
                let plan = AgentPlan {
                    id: plan_id,
                    title,
                    steps: steps
                        .iter()
                        .enumerate()
                        .map(|(index, value)| {
                            let title = value.as_str().unwrap_or_default().trim().to_string();
                            AgentPlanStep {
                                id: new_id(&format!("plan_step_{index}")),
                                description: title.clone(),
                                title,
                                status: PlanStepStatus::NotStarted,
                                notes: String::new(),
                            }
                        })
                        .collect(),
                    created_at: now(),
                    updated_at: now(),
                };
                if plan.steps.iter().any(|step| step.title.is_empty()) {
                    return Err("Every plan step must be a non-empty string.".to_string());
                }
                task.active_plan_id = Some(plan.id.clone());
                let result = format_plan(&plan);
                task.plans.push(plan);
                mark_task_revision(task);
                format!("Plan created successfully.\n{result}")
            }
            "update" => {
                let plan_id = string_argument(arguments, "plan_id")
                    .ok_or_else(|| "plan_id is required for update".to_string())?;
                let plan = task
                    .plans
                    .iter_mut()
                    .find(|plan| plan.id == plan_id)
                    .ok_or_else(|| format!("No plan found with ID: {plan_id}"))?;
                if let Some(title) = string_argument(arguments, "title") {
                    if !title.trim().is_empty() {
                        plan.title = title;
                    }
                }
                if let Some(steps) = arguments.get("steps").and_then(Value::as_array) {
                    let old_steps = plan.steps.clone();
                    let mut next_steps = Vec::new();
                    for (index, value) in steps.iter().enumerate() {
                        let title = value.as_str().unwrap_or_default().trim().to_string();
                        if title.is_empty() {
                            return Err("Every plan step must be a non-empty string.".to_string());
                        }
                        if let Some(old) = old_steps.get(index).filter(|old| old.title == title) {
                            next_steps.push(old.clone());
                        } else {
                            next_steps.push(AgentPlanStep {
                                id: new_id("plan_step"),
                                title: title.clone(),
                                description: title,
                                status: PlanStepStatus::NotStarted,
                                notes: String::new(),
                            });
                        }
                    }
                    plan.steps = next_steps;
                }
                plan.updated_at = now();
                let output = format!("Plan updated successfully.\n{}", format_plan(plan));
                mark_task_revision(task);
                output
            }
            "list" => {
                if task.plans.is_empty() {
                    "No plans available.".to_string()
                } else {
                    task.plans
                        .iter()
                        .map(|plan| {
                            let marker = if task.active_plan_id.as_deref() == Some(&plan.id) {
                                " (active)"
                            } else {
                                ""
                            };
                            let completed = plan
                                .steps
                                .iter()
                                .filter(|step| step.status == PlanStepStatus::Completed)
                                .count();
                            format!(
                                "- {}{}: {} ({}/{})",
                                plan.id,
                                marker,
                                plan.title,
                                completed,
                                plan.steps.len()
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                }
            }
            "get" => {
                let plan_id = string_argument(arguments, "plan_id")
                    .or_else(|| task.active_plan_id.clone())
                    .ok_or_else(|| "No active plan. Specify plan_id.".to_string())?;
                let plan = task
                    .plans
                    .iter()
                    .find(|plan| plan.id == plan_id)
                    .ok_or_else(|| format!("No plan found with ID: {plan_id}"))?;
                format_plan(plan)
            }
            "set_active" => {
                let plan_id = string_argument(arguments, "plan_id")
                    .ok_or_else(|| "plan_id is required for set_active".to_string())?;
                if !task.plans.iter().any(|plan| plan.id == plan_id) {
                    return Err(format!("No plan found with ID: {plan_id}"));
                }
                task.active_plan_id = Some(plan_id.clone());
                mark_task_revision(task);
                format!("Plan '{plan_id}' is now active.")
            }
            "mark_step" => {
                let plan_id = string_argument(arguments, "plan_id")
                    .or_else(|| task.active_plan_id.clone())
                    .ok_or_else(|| "No active plan. Specify plan_id.".to_string())?;
                let index = arguments
                    .get("step_index")
                    .and_then(Value::as_i64)
                    .ok_or_else(|| "step_index is required for mark_step".to_string())?;
                let status = string_argument(arguments, "step_status")
                    .map(|value| parse_plan_status(&value))
                    .transpose()?;
                let note = string_argument(arguments, "step_notes");
                let plan = task
                    .plans
                    .iter_mut()
                    .find(|plan| plan.id == plan_id)
                    .ok_or_else(|| format!("No plan found with ID: {plan_id}"))?;
                let step = plan
                    .steps
                    .get_mut(index.max(0) as usize)
                    .ok_or_else(|| format!("Invalid step_index: {index}"))?;
                if let Some(status) = status {
                    step.status = status;
                }
                if let Some(note) = note {
                    step.notes = note;
                }
                plan.updated_at = now();
                let output = format!("Step updated.\n{}", format_plan(plan));
                mark_task_revision(task);
                output
            }
            "delete" => {
                let plan_id = string_argument(arguments, "plan_id")
                    .ok_or_else(|| "plan_id is required for delete".to_string())?;
                let index = task
                    .plans
                    .iter()
                    .position(|plan| plan.id == plan_id)
                    .ok_or_else(|| format!("No plan found with ID: {plan_id}"))?;
                task.plans.remove(index);
                if task.active_plan_id.as_deref() == Some(&plan_id) {
                    task.active_plan_id = None;
                }
                mark_task_revision(task);
                format!("Plan '{plan_id}' deleted.")
            }
            _ => return Err(format!("Unsupported planning command: {command}")),
        }
    };
    state.persist_task(task_id)?;
    queue_current_plan(state, task_id)?;
    Ok(truncate_output(&output))
}
