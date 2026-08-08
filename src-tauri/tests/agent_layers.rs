#![allow(dead_code)]

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PlanStepStatus {
    NotStarted,
    InProgress,
    Completed,
    Blocked,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentPlanStep {
    pub id: String,
    pub title: String,
    pub description: String,
    pub status: PlanStepStatus,
    pub notes: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentPlan {
    pub id: String,
    pub title: String,
    pub steps: Vec<AgentPlanStep>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[path = "../src/agent.rs"]
mod agent;
#[path = "../src/agents.rs"]
mod agents;
#[path = "../src/bedrock.rs"]
mod bedrock;
#[path = "../src/config.rs"]
mod config;
#[path = "../src/flow.rs"]
mod flow;
#[path = "../src/llm.rs"]
mod llm;
#[path = "../src/mcp_server.rs"]
mod mcp_server;
#[path = "../src/protocol.rs"]
mod protocol;
#[path = "../src/react.rs"]
mod react;
#[path = "../src/tool.rs"]
mod tool;

use agents::{AgentFactory, AgentSpec};
use bedrock::{bedrock_response_to_openai, openai_messages_to_bedrock};
use config::Config;
use flow::PlanningFlow;
use mcp_server::McpServer;
use tool::{BaseTool, FunctionTool, ToolCollection, ToolDefinition, ToolResult};

#[test]
fn agent_catalog_is_available_from_the_library() {
    let specs = AgentSpec::all(".");
    assert_eq!(specs.len(), 6);
    assert!(specs.iter().all(|spec| spec.validate().is_ok()));
    let instance = AgentFactory::create_from_name("browser", ".", ToolCollection::default());
    assert_eq!(instance.spec().kind, agent::AgentKind::Browser);
    assert_eq!(instance.spec().max_observe, Some(10_000));
}

#[tokio::test]
async fn planning_flow_runs_a_real_three_step_executor() {
    let mut flow = PlanningFlow::with_agents(AgentSpec::all("."));
    let result = flow
        .execute_with(
            "inspect and verify",
            |agent, prompt| async move {
                assert!(agent
                    .tool_names
                    .iter()
                    .all(|name| name.starts_with("rust_")));
                assert!(prompt.contains("CURRENT PLAN STATUS"));
                Ok(format!("{} executed", agent.name))
            },
            &CancellationToken::new(),
        )
        .await
        .expect("planning flow should complete");
    assert!(result.contains("3/3 completed"));
}

#[tokio::test]
async fn mcp_stdio_contract_lists_and_calls_registered_tools() {
    let echo: Arc<dyn BaseTool> = Arc::new(FunctionTool::new(
        ToolDefinition::new("rust_echo", "Echo", json!({"type": "object"})),
        |arguments| async move { Ok(ToolResult::success(arguments)) },
    ));
    let mut server = McpServer::new("rustpilot");
    assert!(server.register_tool(echo));
    let response = server
        .handle_request(
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": {"name": "rust_echo", "arguments": {"value": 7}}
            }),
            &CancellationToken::new(),
        )
        .await
        .expect("MCP calls return a JSON-RPC response");
    assert!(!response["result"]["isError"].as_bool().unwrap_or(true));
    assert!(response["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default()
        .contains("value"));
}

#[test]
fn bedrock_adapter_round_trips_openai_tool_messages() {
    let messages = vec![
        json!({"role": "assistant", "tool_calls": [{"id": "call-1", "function": {"name": "rust_clock", "arguments": "{}"}}]}),
        json!({"role": "tool", "tool_call_id": "call-1", "content": "now"}),
    ];
    let (_, converted) = openai_messages_to_bedrock(&messages).unwrap();
    assert_eq!(
        converted[1]["content"][0]["toolResult"]["toolUseId"],
        "call-1"
    );
    let openai = bedrock_response_to_openai(&json!({
        "output": {"message": {"role": "assistant", "content": [{"text": "ok"}]}}
    }));
    assert_eq!(openai["choices"][0]["message"]["content"], "ok");
}

#[test]
fn config_loads_without_secrets_or_project_files() {
    let config = Config::load(".").expect("default config should load");
    assert!(config.llm("default").api_key.is_empty());
    assert_eq!(config.app.sandbox.work_dir, "/workspace");
}
