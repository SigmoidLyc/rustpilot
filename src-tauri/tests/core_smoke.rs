#![allow(dead_code)]

#[path = "../src/agent.rs"]
mod agent;

use agent::{
    AgentKind, AgentState, BaseAgentRuntime, FunctionCall, Memory, Message, MessageToolCall,
    PlanningFlowRuntime, ToolCallAgentRuntime,
};

#[test]
fn memory_is_bounded_and_replayable() {
    let mut memory = Memory {
        max_messages: 2,
        ..Memory::default()
    };
    memory.add_message(Message::user("one"));
    memory.add_message(Message::assistant_with_tools(
        Some("thinking".to_string()),
        vec![MessageToolCall {
            id: "call-1".to_string(),
            call_type: "function".to_string(),
            function: FunctionCall {
                name: "rust_clock".to_string(),
                arguments: "{}".to_string(),
            },
        }],
    ));
    memory.add_message(Message::tool("now", "rust_clock", "call-1"));
    assert_eq!(memory.messages.len(), 2);
    assert_eq!(memory.messages[0].role, agent::Role::Assistant);
    assert!(memory.to_openai_messages()[0]["tool_calls"].is_array());
}

#[test]
fn agent_state_and_step_limit_match_react_runtime() {
    let mut agent = BaseAgentRuntime::new("test", "system");
    assert_eq!(agent.state, AgentState::Idle);
    agent.begin().expect("idle agent should start");
    agent.max_steps = 1;
    assert_eq!(agent.next_step().expect("first step should run"), 1);
    assert!(agent.next_step().is_err());
    assert_eq!(agent.state, AgentState::Finished);
}

#[test]
fn tool_call_agent_preserves_calls_and_special_tools() {
    let mut agent = ToolCallAgentRuntime::new("manus", "system");
    agent.base.begin().expect("tool agent should start");
    let call = MessageToolCall {
        id: "call-1".to_string(),
        call_type: "function".to_string(),
        function: FunctionCall {
            name: "rust_files".to_string(),
            arguments: "{\"operation\":\"list\"}".to_string(),
        },
    };
    assert!(agent.set_response(None, vec![call.clone()]));
    assert_eq!(agent.tool_calls, vec![call]);
    assert!(agent.should_finish_execution("terminate"));
}

#[test]
fn planning_flow_selects_marked_executor_and_active_step() {
    let mut flow = PlanningFlowRuntime::new("plan-1");
    flow.executors
        .insert("browser".to_string(), AgentKind::Browser);
    assert_eq!(
        flow.executor_for(Some("browser"), AgentKind::Manus),
        AgentKind::Browser
    );
    let steps = ["done", "active", "later"];
    let index = flow.next_active_step(&steps, |step| *step == "active");
    assert_eq!(index, Some(1));
    assert_eq!(flow.current_step_index, Some(1));
}

#[test]
fn specialized_prompt_profiles_are_explicitly_rust_tool_oriented() {
    for kind in [
        AgentKind::Manus,
        AgentKind::Browser,
        AgentKind::DataAnalysis,
        AgentKind::Swe,
        AgentKind::Mcp,
        AgentKind::SandboxManus,
    ] {
        let (system, next) = agent::prompt_profile(kind, ".");
        assert!(!system.is_empty());
        assert!(next.contains("rust_"));
    }
}
