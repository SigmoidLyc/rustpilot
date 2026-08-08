//! ReAct and tool-call execution primitives.
//!
//! The execution loop separates the decision phase (`think`) from the action
//! phase (`act`). This module keeps that contract reusable outside the Tauri
//! task loop while allowing the desktop runtime to provide its approval-aware
//! tools through the same `ToolCollection` interface.

use std::{error::Error, fmt::Display, sync::Arc};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::{
    agent::{AgentState, Message, MessageToolCall, ToolCallAgentRuntime, ToolChoice},
    llm::{LlmError, OpenAiCompatibleClient},
    tool::{ToolCollection, ToolResult},
};

pub const TOOL_CALL_REQUIRED: &str = "Tool calls required but none provided";

#[derive(Debug)]
pub enum ReActError {
    Llm(LlmError),
    Tool(String),
    Cancelled,
    MaxSteps(u32),
}

impl Display for ReActError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Llm(error) => Display::fmt(error, formatter),
            Self::Tool(error) => formatter.write_str(error),
            Self::Cancelled => formatter.write_str("Agent execution cancelled."),
            Self::MaxSteps(limit) => write!(formatter, "Reached maximum agent steps ({limit})."),
        }
    }
}

impl Error for ReActError {}

impl From<LlmError> for ReActError {
    fn from(error: LlmError) -> Self {
        match error {
            LlmError::Cancelled => Self::Cancelled,
            other => Self::Llm(other),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentRunResult {
    pub answer: String,
    pub observations: Vec<String>,
    pub steps: u32,
    pub state: AgentState,
}

#[derive(Clone)]
pub struct ReActAgentRuntime {
    pub tool_agent: ToolCallAgentRuntime,
    pub available_tools: ToolCollection,
    pub current_base64_image: Option<String>,
}

impl std::fmt::Debug for ReActAgentRuntime {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ReActAgentRuntime")
            .field("tool_agent", &self.tool_agent)
            .field("available_tools", &self.available_tools.names())
            .finish()
    }
}

impl ReActAgentRuntime {
    pub fn new(
        name: impl Into<String>,
        system_prompt: impl Into<String>,
        tools: ToolCollection,
    ) -> Self {
        Self {
            tool_agent: ToolCallAgentRuntime::new(name, system_prompt),
            available_tools: tools,
            current_base64_image: None,
        }
    }

    pub fn state(&self) -> AgentState {
        self.tool_agent.base.state
    }

    pub fn set_tool_choice(&mut self, choice: ToolChoice) {
        self.tool_agent.tool_choice = choice;
    }

    pub async fn think(
        &mut self,
        llm: &OpenAiCompatibleClient,
        cancel: &CancellationToken,
    ) -> Result<bool, ReActError> {
        if cancel.is_cancelled() {
            return Err(ReActError::Cancelled);
        }
        if !self.tool_agent.base.next_step_prompt.is_empty() {
            self.tool_agent
                .base
                .memory
                .add_message(Message::user_message(
                    self.tool_agent.base.next_step_prompt.clone(),
                ));
        }

        let messages = self.tool_agent.base.memory.get_recent_messages(100);
        let system_messages = if self.tool_agent.base.system_prompt.is_empty() {
            Vec::new()
        } else {
            vec![Message::system_message(
                self.tool_agent.base.system_prompt.clone(),
            )]
        };
        let response = llm
            .ask_tool(
                &messages,
                &system_messages,
                &self.available_tools.to_params(),
                self.tool_agent.tool_choice,
                None,
                cancel,
            )
            .await?;
        let calls = response.tool_calls.clone();
        let content = (!response.content.is_empty()).then_some(response.content);
        Ok(self.tool_agent.set_response(content, calls))
    }

    pub async fn act(&mut self, cancel: &CancellationToken) -> Result<String, ReActError> {
        if cancel.is_cancelled() {
            return Err(ReActError::Cancelled);
        }
        if self.tool_agent.tool_calls.is_empty() {
            if self.tool_agent.tool_choice == ToolChoice::Required {
                return Err(ReActError::Tool(TOOL_CALL_REQUIRED.to_string()));
            }
            return Ok(self
                .tool_agent
                .base
                .memory
                .messages
                .back()
                .and_then(|message| message.content.clone())
                .unwrap_or_else(|| "No content or commands to execute".to_string()));
        }

        let calls = self.tool_agent.tool_calls.clone();
        let mut results = Vec::with_capacity(calls.len());
        for command in calls {
            if cancel.is_cancelled() {
                return Err(ReActError::Cancelled);
            }
            let result = self.execute_tool(&command, cancel).await?;
            results.push(result);
        }
        Ok(results.join("\n\n"))
    }

    pub async fn execute_tool(
        &mut self,
        command: &MessageToolCall,
        cancel: &CancellationToken,
    ) -> Result<String, ReActError> {
        if command.id.is_empty() || command.function.name.is_empty() {
            return Ok("Error: Invalid command format".to_string());
        }
        if !self.available_tools.contains(&command.function.name) {
            return Ok(format!("Error: Unknown tool '{}'", command.function.name));
        }
        let arguments = match serde_json::from_str::<Value>(&command.function.arguments) {
            Ok(arguments) => arguments,
            Err(_) => {
                return Ok(format!(
                    "Error parsing arguments for {}: Invalid JSON format",
                    command.function.name
                ));
            }
        };
        let result = tokio::select! {
            _ = cancel.cancelled() => return Err(ReActError::Cancelled),
            result = self.available_tools.execute(&command.function.name, arguments) => result,
        };
        if self.tool_agent.is_special_tool(&command.function.name) && result.is_success() {
            self.tool_agent.base.finish();
        }
        if result.base64_image.is_some() {
            self.current_base64_image = result.base64_image.clone();
        }
        let observation = format!(
            "Observed output of cmd `{}` executed:\n{}",
            command.function.name,
            self.tool_agent.observed(&result.text())
        );
        self.tool_agent
            .base
            .memory
            .add_message(Message::tool_message(
                self.tool_agent.observed(&result.text()),
                command.function.name.clone(),
                command.id.clone(),
            ));
        Ok(observation)
    }

    pub async fn step(
        &mut self,
        llm: &OpenAiCompatibleClient,
        cancel: &CancellationToken,
    ) -> Result<String, ReActError> {
        let should_act = self.think(llm, cancel).await?;
        if !should_act {
            return Ok("Thinking complete - no action needed".to_string());
        }
        self.act(cancel).await
    }

    pub async fn run(
        &mut self,
        llm: &OpenAiCompatibleClient,
        request: Option<&str>,
        cancel: &CancellationToken,
    ) -> Result<AgentRunResult, ReActError> {
        if self.state() != AgentState::Idle {
            return Err(ReActError::Tool(format!(
                "Cannot run agent from state: {:?}",
                self.state()
            )));
        }
        if let Some(request) = request.filter(|value| !value.trim().is_empty()) {
            self.tool_agent
                .base
                .memory
                .add_message(Message::user(request));
        }
        self.tool_agent.base.begin().map_err(ReActError::Tool)?;
        let mut observations = Vec::new();
        let mut answer = String::new();
        while self.tool_agent.base.current_step < self.tool_agent.base.max_steps
            && self.state() == AgentState::Running
        {
            if cancel.is_cancelled() {
                self.tool_agent.base.state = AgentState::Error;
                return Err(ReActError::Cancelled);
            }
            self.tool_agent
                .base
                .next_step()
                .map_err(|_| ReActError::MaxSteps(self.tool_agent.base.max_steps))?;
            let observation = self.step(llm, cancel).await?;
            if !observation.is_empty() {
                answer = observation.clone();
                observations.push(format!(
                    "Step {}: {observation}",
                    self.tool_agent.base.current_step
                ));
            }
            if self.tool_agent.base.is_stuck() {
                self.tool_agent.base.handle_stuck_state();
            }
            if self.state() == AgentState::Finished {
                break;
            }
        }
        if self.state() == AgentState::Running {
            self.tool_agent.base.fail();
            return Err(ReActError::MaxSteps(self.tool_agent.base.max_steps));
        }
        Ok(AgentRunResult {
            answer,
            observations,
            steps: self.tool_agent.base.current_step,
            state: self.state(),
        })
    }

    pub fn tool_result_observation(&self, result: &ToolResult) -> String {
        self.tool_agent.observed(&result.text())
    }

    pub fn add_tool(&mut self, tool: Arc<dyn crate::tool::BaseTool>) -> bool {
        self.available_tools.add(tool)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::{FunctionTool, ToolDefinition};
    use serde_json::json;

    #[tokio::test]
    async fn act_formats_observations_and_special_tools_finish() {
        let echo: Arc<dyn crate::tool::BaseTool> = Arc::new(FunctionTool::new(
            ToolDefinition::new("rust_echo", "Echo", json!({"type": "object"})),
            |arguments| async move { Ok(ToolResult::success(arguments)) },
        ));
        let terminate: Arc<dyn crate::tool::BaseTool> = Arc::new(FunctionTool::new(
            ToolDefinition::new("terminate", "Finish", json!({"type": "object"})),
            |_| async { Ok(ToolResult::success("done")) },
        ));
        let mut runtime =
            ReActAgentRuntime::new("test", "system", ToolCollection::new(vec![echo, terminate]));
        runtime.tool_agent.base.begin().unwrap();
        runtime.tool_agent.tool_calls = vec![MessageToolCall {
            id: "call-1".to_string(),
            call_type: "function".to_string(),
            function: crate::agent::FunctionCall {
                name: "rust_echo".to_string(),
                arguments: r#"{"value":1}"#.to_string(),
            },
        }];
        let result = runtime.act(&CancellationToken::new()).await.unwrap();
        assert!(result.contains("rust_echo"));
        assert!(result.contains("value"));
    }
}
