//! Public schema compatibility layer for the agent API.

pub use crate::agent::{
    AgentKind, AgentState, BaseAgentRuntime, FunctionCall, Memory, Message, MessageToolCall,
    PlanningFlowRuntime, Role, ToolCallAgentRuntime, ToolChoice,
};

pub type Function = FunctionCall;
pub type ToolCall = MessageToolCall;
pub type RoleType = Role;

pub const ROLE_VALUES: [Role; 4] = [Role::System, Role::User, Role::Assistant, Role::Tool];
pub const TOOL_CHOICE_VALUES: [ToolChoice; 3] =
    [ToolChoice::None, ToolChoice::Auto, ToolChoice::Required];

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn message_factories_match_schema() {
        let user = Message::user_message("hello");
        assert_eq!(user.role, Role::User);
        assert_eq!(user.to_dict()["content"], "hello");

        let call = ToolCall {
            id: "call-1".to_string(),
            call_type: "function".to_string(),
            function: Function {
                name: "rust_clock".to_string(),
                arguments: json!({}).to_string(),
            },
        };
        let assistant = Message::from_tool_calls(Some("thinking".to_string()), vec![call]);
        assert_eq!(assistant.role, Role::Assistant);
        assert!(assistant.to_dict()["tool_calls"].is_array());
    }
}
