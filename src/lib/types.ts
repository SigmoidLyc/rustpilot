export type AgentStatus =
  | "idle"
  | "planning"
  | "executing"
  | "verifying"
  | "waiting_approval"
  | "completed"
  | "failed"
  | "cancelled";

export type StepPhase = "plan" | "act" | "verify";
export type StepStatus = "pending" | "running" | "completed" | "failed" | "cancelled";
export type ToolCallStatus = "pending" | "running" | "completed" | "failed" | "cancelled";

export type PlanStepStatus = "not_started" | "in_progress" | "completed" | "blocked";

export interface AgentMemoryEntry {
  id: string;
  role: string;
  content: string;
  created_at: number;
  tool_call_id: string | null;
  tool_names: string[];
  tool_calls: AgentMessageToolCall[];
  name: string | null;
  base64_image: string | null;
  attachments: TaskAttachment[];
}

export interface TaskAttachment {
  id: string;
  name: string;
  mime: string;
  size: number;
  storage_key: string;
}

export interface AttachmentInput {
  name: string;
  mime: string;
  data: string;
}

export interface AttachmentPathInput {
  path: string;
  name: string;
  mime: string;
}

export interface AgentMessageToolCall {
  id: string;
  type: string;
  function: {
    name: string;
    arguments: string;
  };
}

export type AssistantPart =
  | { type: "text"; id: string; start: number; end: number }
  | { type: "tool"; id: string; index: number; call_id: string; name: string };

export type AssistantToolPart = Extract<AssistantPart, { type: "tool" }>;

export interface AgentPlanStep {
  id: string;
  title: string;
  description: string;
  status: PlanStepStatus;
  notes: string;
}

export interface AgentPlan {
  id: string;
  title: string;
  steps: AgentPlanStep[];
  created_at: number;
  updated_at: number;
}

export interface AgentToolDefinition {
  name: string;
  description: string;
}

export interface TaskMessage {
  id: string;
  task_id: string;
  role: "user" | "assistant" | "system" | "tool";
  content: string;
  created_at: number;
  streaming: boolean;
  parts?: AssistantPart[];
  tool_calls: AgentMessageToolCall[];
  tool_call_id: string | null;
  name: string | null;
  base64_image: string | null;
  attachments: TaskAttachment[];
}

export interface AgentStep {
  id: string;
  task_id: string;
  phase: StepPhase;
  title: string;
  detail: string | null;
  status: StepStatus;
  started_at: number;
  ended_at: number | null;
  duration_ms: number | null;
}

export interface ToolCall {
  id: string;
  task_id: string;
  name: string;
  arguments: unknown;
  model_tool_call_id?: string | null;
  status: ToolCallStatus;
  started_at: number;
  ended_at: number | null;
  duration_ms: number | null;
  result: string | null;
  error: string | null;
}

export interface ToolPresentation {
  id: string;
  name: string;
  arguments: unknown;
  status: ToolCallStatus;
  duration_ms: number | null;
  result: string | null;
  error: string | null;
}

export interface ToolResult {
  id: string;
  task_id: string;
  tool_call_id: string;
  status: ToolCallStatus;
  output: string | null;
  error: string | null;
  duration_ms: number | null;
}

export interface ApprovalRequest {
  id: string;
  task_id: string;
  tool_name: string;
  reason: string;
  details: string;
  created_at: number;
  status: string;
}

export interface LlmUsage {
  total_input_tokens: number;
  total_completion_tokens: number;
  total_cached_input_tokens: number;
  total_cache_write_tokens: number;
  cache_hit_count: number;
  cache_write_count: number;
}

export interface Task {
  id: string;
  title: string;
  prompt: string;
  status: AgentStatus;
  created_at: number;
  updated_at: number;
  demo_mode: boolean;
  archived: boolean;
  agent_name: string;
  agent_kind: string;
  messages: TaskMessage[];
  memory: AgentMemoryEntry[];
  plans: AgentPlan[];
  active_plan_id: string | null;
  steps: AgentStep[];
  tool_calls: ToolCall[];
  approval_requests: ApprovalRequest[];
  llm_usage: LlmUsage;
  final_answer: string | null;
  error: string | null;
  event_seq: number;
}

export type PersistedStreamEvent =
  | { kind: "text_delta"; delta: string }
  | {
      kind: "tool_call_delta";
      index: number;
      id: string | null;
      name: string | null;
    };

export interface TaskEvent {
  task_id: string;
  seq: number;
  kind: "task" | "stream";
  event: string | null;
  message_id: string | null;
  payload: unknown;
}

export interface TaskEventPage {
  task_id: string;
  snapshot: Task | null;
  events: TaskEvent[];
  cursor: number;
  has_more: boolean;
  reset: boolean;
}

export interface TaskSummary {
  id: string;
  title: string;
  status: AgentStatus;
  updated_at: number;
  demo_mode: boolean;
  archived: boolean;
  error: string | null;
}

export interface TaskStatusEvent {
  task_id: string;
  status: AgentStatus;
  updated_at: number;
  error: string | null;
}

export interface TaskCompletedEvent {
  task_id: string;
  final_answer: string;
  demo_mode: boolean;
}

export interface TaskFailedEvent {
  task_id: string;
  error: string;
}

export interface TaskCancelledEvent {
  task_id: string;
}

export interface TaskPlanEvent {
  task_id: string;
  plan: AgentPlan;
}

export interface SettingsView {
  api_base_url: string;
  model: string;
  api_key_configured: boolean;
  max_steps: number;
  timeout_secs: number;
  prompt_cache: PromptCacheMode;
  demo_mode: boolean;
  available_tools: AgentToolDefinition[];
}

export type PromptCacheMode = "auto" | "enabled" | "disabled";

export interface SettingsInput {
  api_base_url: string;
  model: string;
  api_key: string | null;
  max_steps: number;
  timeout_secs: number;
  prompt_cache: PromptCacheMode;
}

export type AgentEventPayload =
  | Task
  | TaskMessage
  | AgentStep
  | ToolCall
  | ToolResult
  | ApprovalRequest
  | TaskStatusEvent
  | TaskCompletedEvent
  | TaskFailedEvent
  | TaskCancelledEvent
  | TaskPlanEvent;
