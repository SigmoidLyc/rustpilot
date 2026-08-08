//! Reusable tool contracts.
//!
//! The desktop command dispatcher owns the concrete `rust_` implementations,
//! while this module provides a shared tool/result/collection boundary for
//! agent runtimes. Keeping the boundary independent makes agents, flows, MCP
//! adapters, and tests composable without coupling them to Tauri.

use std::{
    collections::BTreeMap,
    error::Error,
    fmt::{Display, Formatter},
    future::Future,
    pin::Pin,
    sync::Arc,
};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub type ToolFuture<'a> = Pin<Box<dyn Future<Output = Result<ToolResult, ToolError>> + Send + 'a>>;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct ToolResult {
    #[serde(default)]
    pub output: Option<Value>,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub base64_image: Option<String>,
    #[serde(default)]
    pub system: Option<String>,
}

impl ToolResult {
    pub fn success(output: impl Into<Value>) -> Self {
        Self {
            output: Some(output.into()),
            error: None,
            base64_image: None,
            system: None,
        }
    }

    pub fn failure(error: impl Into<String>) -> Self {
        Self {
            output: None,
            error: Some(error.into()),
            base64_image: None,
            system: None,
        }
    }

    pub fn is_success(&self) -> bool {
        self.error.is_none()
    }

    pub fn replace(mut self, output: Option<Value>, error: Option<String>) -> Self {
        if output.is_some() {
            self.output = output;
        }
        if error.is_some() {
            self.error = error;
        }
        self
    }

    pub fn text(&self) -> String {
        if let Some(error) = &self.error {
            return format!("Error: {error}");
        }
        match &self.output {
            Some(Value::String(value)) => value.clone(),
            Some(value) => {
                serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
            }
            None => String::new(),
        }
    }
}

impl Display for ToolResult {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.text())
    }
}

impl std::ops::Add for ToolResult {
    type Output = Result<Self, ToolError>;

    fn add(self, other: Self) -> Self::Output {
        let output = match (self.output, other.output) {
            (Some(Value::String(left)), Some(Value::String(right))) => {
                Some(Value::String(format!("{left}{right}")))
            }
            (Some(left), Some(right)) => Some(json!([left, right])),
            (left, right) => left.or(right),
        };
        let error = match (self.error, other.error) {
            (Some(left), Some(right)) => Some(format!("{left}; {right}")),
            (left, right) => left.or(right),
        };
        let base64_image = match (self.base64_image, other.base64_image) {
            (Some(_), Some(_)) => {
                return Err(ToolError::new("Cannot combine two image tool results."));
            }
            (left, right) => left.or(right),
        };
        Ok(Self {
            output,
            error,
            base64_image,
            system: self.system.or(other.system),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolError {
    pub message: String,
}

impl ToolError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl Display for ToolError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for ToolError {}

impl From<String> for ToolError {
    fn from(message: String) -> Self {
        Self::new(message)
    }
}

impl From<&str> for ToolError {
    fn from(message: &str) -> Self {
        Self::new(message)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

impl ToolDefinition {
    pub fn new(name: impl Into<String>, description: impl Into<String>, parameters: Value) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            parameters,
        }
    }

    pub fn to_param(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": self.name,
                "description": self.description,
                "parameters": self.parameters
            }
        })
    }
}

pub trait BaseTool: Send + Sync {
    fn definition(&self) -> &ToolDefinition;

    fn execute(&self, arguments: Value) -> ToolFuture<'_>;

    fn name(&self) -> &str {
        &self.definition().name
    }

    fn description(&self) -> &str {
        &self.definition().description
    }

    fn to_param(&self) -> Value {
        self.definition().to_param()
    }
}

pub type ToolExecutor = Arc<dyn Fn(Value) -> ToolFuture<'static> + Send + Sync>;

pub struct FunctionTool {
    definition: ToolDefinition,
    executor: ToolExecutor,
}

impl FunctionTool {
    pub fn new<F, Fut>(definition: ToolDefinition, executor: F) -> Self
    where
        F: Fn(Value) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<ToolResult, ToolError>> + Send + 'static,
    {
        let executor =
            Arc::new(move |arguments: Value| Box::pin(executor(arguments)) as ToolFuture<'static>);
        Self {
            definition,
            executor,
        }
    }
}

impl BaseTool for FunctionTool {
    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }

    fn execute(&self, arguments: Value) -> ToolFuture<'_> {
        (self.executor)(arguments)
    }
}

#[derive(Clone, Default)]
pub struct ToolCollection {
    tools: BTreeMap<String, Arc<dyn BaseTool>>,
}

impl ToolCollection {
    pub fn new(tools: impl IntoIterator<Item = Arc<dyn BaseTool>>) -> Self {
        let mut collection = Self::default();
        for tool in tools {
            collection.add(tool);
        }
        collection
    }

    pub fn add(&mut self, tool: Arc<dyn BaseTool>) -> bool {
        let name = tool.name().to_string();
        if self.tools.contains_key(&name) {
            return false;
        }
        self.tools.insert(name, tool);
        true
    }

    pub fn add_many(&mut self, tools: impl IntoIterator<Item = Arc<dyn BaseTool>>) -> usize {
        tools
            .into_iter()
            .map(|tool| usize::from(self.add(tool)))
            .sum()
    }

    pub fn get_tool(&self, name: &str) -> Option<Arc<dyn BaseTool>> {
        self.tools.get(name).cloned()
    }

    pub fn contains(&self, name: &str) -> bool {
        self.tools.contains_key(name)
    }

    pub fn len(&self) -> usize {
        self.tools.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    pub fn to_params(&self) -> Vec<Value> {
        self.tools.values().map(|tool| tool.to_param()).collect()
    }

    pub fn names(&self) -> Vec<String> {
        self.tools.keys().cloned().collect()
    }

    pub async fn execute(&self, name: &str, arguments: Value) -> ToolResult {
        let Some(tool) = self.tools.get(name) else {
            return ToolResult::failure(format!("Tool {name} is invalid"));
        };
        match tool.execute(arguments).await {
            Ok(result) => result,
            Err(error) => ToolResult::failure(error.message),
        }
    }

    pub async fn execute_checked(
        &self,
        name: &str,
        arguments: Value,
    ) -> Result<ToolResult, ToolError> {
        let tool = self
            .tools
            .get(name)
            .ok_or_else(|| ToolError::new(format!("Tool {name} is invalid")))?;
        tool.execute(arguments).await
    }

    pub async fn execute_all(&self) -> Vec<ToolResult> {
        let names = self.names();
        let mut results = Vec::with_capacity(names.len());
        for name in names {
            results.push(self.execute(&name, Value::Object(Default::default())).await);
        }
        results
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn echo_tool() -> Arc<dyn BaseTool> {
        Arc::new(FunctionTool::new(
            ToolDefinition::new(
                "rust_echo",
                "Return the supplied value.",
                json!({"type": "object"}),
            ),
            |arguments| async move { Ok(ToolResult::success(arguments)) },
        ))
    }

    #[tokio::test]
    async fn collection_matches_tool_contract() {
        let mut tools = ToolCollection::default();
        assert!(tools.add(echo_tool()));
        assert!(!tools.add(echo_tool()));
        assert_eq!(tools.names(), vec!["rust_echo"]);
        let result = tools.execute("rust_echo", json!({"value": 7})).await;
        assert_eq!(result.output, Some(json!({"value": 7})));
        assert!(tools.to_params()[0]["function"]["name"] == "rust_echo");
    }

    #[test]
    fn tool_results_combine_without_losing_errors() {
        let combined = (ToolResult::success("a") + ToolResult::success("b")).unwrap();
        assert_eq!(combined.text(), "ab");
        let failed = (ToolResult::failure("left") + ToolResult::failure("right")).unwrap();
        assert_eq!(failed.error.as_deref(), Some("left; right"));
    }
}
