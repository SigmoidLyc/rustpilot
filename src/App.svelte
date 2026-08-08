<script lang="ts">
  import { onMount } from "svelte";
  import { ListTree, WifiOff } from "lucide-svelte";
  import ApprovalDialog from "./lib/components/ApprovalDialog.svelte";
  import Composer from "./lib/components/Composer.svelte";
  import Conversation from "./lib/components/Conversation.svelte";
  import RustTrace from "./lib/components/RustTrace.svelte";
  import SettingsPanel from "./lib/components/SettingsPanel.svelte";
  import Sidebar from "./lib/components/Sidebar.svelte";
  import StatusBadge from "./lib/components/StatusBadge.svelte";
  import {
    createTask,
    continueTask,
    archiveTask,
    deleteTask,
    getSettings,
    getTask,
    isTauriRuntime,
    listArchivedTasks,
    listTasks,
    respondToApproval,
    restoreTask,
    retryTask,
    stopTask,
    subscribeToEvents,
    updateSettings
  } from "./lib/api";
  import type {
    AgentStep,
    ApprovalRequest,
    SettingsInput,
    SettingsView,
    Task,
    TaskMessage,
    TaskPlanEvent,
    TaskStatusEvent,
    TaskSummary,
    TaskCompletedEvent,
    TaskFailedEvent,
    TaskCancelledEvent,
    ToolCall,
    ToolResult
  } from "./lib/types";

  const defaultSettings: SettingsView = {
    api_base_url: "https://api.openai.com/v1",
    model: "gpt-4o-mini",
    api_key_configured: false,
    max_steps: 100,
    timeout_secs: 45,
    prompt_cache: "auto",
    demo_mode: true,
    available_tools: []
  };

  let tasks: TaskSummary[] = [];
  let archivedTasks: TaskSummary[] = [];
  let selectedTask: Task | null = null;
  let selectedTaskId = "";
  let settings = defaultSettings;
  let settingsVisible = false;
  let settingsSaving = false;
  let traceCollapsed = true;
  let pendingApproval: ApprovalRequest | null = null;
  let approvalBusy = false;
  let actionError = "";
  let runtimeError = "";
  let pollTimer: ReturnType<typeof setInterval> | undefined;
  let pollInFlight = false;

  $: selectedStatus = selectedTask?.status ?? "idle";
  $: taskIsBusy =
    selectedTask !== null &&
    ["planning", "executing", "verifying", "waiting_approval"].includes(selectedTask.status);

  function summaryForTask(task: Task): TaskSummary {
    return {
      id: task.id,
      title: task.title,
      status: task.status,
      updated_at: task.updated_at,
      demo_mode: task.demo_mode,
      archived: task.archived,
      error: task.error
    };
  }

  function upsertSummary(summary: TaskSummary): void {
    if (summary.archived) {
      tasks = tasks.filter((task) => task.id !== summary.id);
      const nextArchived = archivedTasks.filter((task) => task.id !== summary.id);
      archivedTasks = [summary, ...nextArchived].sort(
        (left, right) => right.updated_at - left.updated_at
      );
      return;
    }
    archivedTasks = archivedTasks.filter((task) => task.id !== summary.id);
    const next = tasks.filter((task) => task.id !== summary.id);
    tasks = [summary, ...next].sort((left, right) => right.updated_at - left.updated_at);
  }

  function setSelectedTask(task: Task): void {
    selectedTaskId = task.id;
    selectedTask = task;
    upsertSummary(summaryForTask(task));
  }

  function updateSelected(mutator: (task: Task) => Task): void {
    if (selectedTask) {
      selectedTask = mutator(selectedTask);
    }
  }

  function replaceMessage(message: TaskMessage): void {
    if (!selectedTask || selectedTask.id !== message.task_id) return;
    const messages = [...selectedTask.messages];
    const index = messages.findIndex((item) => item.id === message.id);
    if (index === -1) messages.push(message);
    else messages[index] = message;
    updateSelected((task) => ({ ...task, messages, updated_at: message.created_at }));
  }

  function replaceStep(step: AgentStep): void {
    if (!selectedTask || selectedTask.id !== step.task_id) return;
    const steps = [...selectedTask.steps];
    const index = steps.findIndex((item) => item.id === step.id);
    if (index === -1) steps.push(step);
    else steps[index] = step;
    updateSelected((task) => ({ ...task, steps, updated_at: Date.now() }));
  }

  function replaceToolCall(call: ToolCall): void {
    if (!selectedTask || selectedTask.id !== call.task_id) return;
    const toolCalls = [...selectedTask.tool_calls];
    const index = toolCalls.findIndex((item) => item.id === call.id);
    if (index === -1) toolCalls.push(call);
    else toolCalls[index] = call;
    updateSelected((task) => ({ ...task, tool_calls: toolCalls, updated_at: Date.now() }));
  }

  function replaceToolResult(result: ToolResult): void {
    if (!selectedTask || selectedTask.id !== result.task_id) return;
    const toolCalls = selectedTask.tool_calls.map((call) =>
      call.id === result.tool_call_id
        ? {
            ...call,
            status: result.status,
            result: result.output,
            error: result.error,
            duration_ms: result.duration_ms
          }
        : call
    );
    updateSelected((task) => ({ ...task, tool_calls: toolCalls, updated_at: Date.now() }));
  }

  function handleCreated(task: Task): void {
    actionError = "";
    pendingApproval = null;
    setSelectedTask(task);
  }

  function handleStatus(event: TaskStatusEvent): void {
    const existing = tasks.find((task) => task.id === event.task_id);
    if (existing) {
      upsertSummary({
        ...existing,
        status: event.status,
        updated_at: event.updated_at,
        error: event.error
      });
    }
    if (selectedTask?.id === event.task_id) {
      updateSelected((task) => ({
        ...task,
        status: event.status,
        updated_at: event.updated_at,
        error: event.error
      }));
    }
  }

  function handleApproval(request: ApprovalRequest): void {
    if (selectedTask?.id === request.task_id) {
      updateSelected((task) => ({
        ...task,
        status: "waiting_approval",
        approval_requests: [...task.approval_requests, request],
        updated_at: Date.now()
      }));
      pendingApproval = request;
    }
  }

  function handleCompleted(event: TaskCompletedEvent): void {
    if (selectedTask?.id === event.task_id) {
      updateSelected((task) => ({
        ...task,
        status: "completed",
        final_answer: event.final_answer,
        error: null,
        updated_at: Date.now()
      }));
    }
    const existing = tasks.find((task) => task.id === event.task_id);
    if (existing) upsertSummary({ ...existing, status: "completed", error: null, updated_at: Date.now() });
  }

  function handleFailed(event: TaskFailedEvent): void {
    pendingApproval = null;
    if (selectedTask?.id === event.task_id) {
      updateSelected((task) => ({ ...task, status: "failed", error: event.error, updated_at: Date.now() }));
    }
    const existing = tasks.find((task) => task.id === event.task_id);
    if (existing) upsertSummary({ ...existing, status: "failed", error: event.error, updated_at: Date.now() });
  }

  function handleCancelled(event: TaskCancelledEvent): void {
    pendingApproval = null;
    if (selectedTask?.id === event.task_id) {
      updateSelected((task) => ({ ...task, status: "cancelled", updated_at: Date.now() }));
    }
    const existing = tasks.find((task) => task.id === event.task_id);
    if (existing) upsertSummary({ ...existing, status: "cancelled", updated_at: Date.now() });
  }

  function handlePlan(event: TaskPlanEvent): void {
    if (selectedTask?.id !== event.task_id) return;
    updateSelected((task) => {
      const plans = task.plans.some((plan) => plan.id === event.plan.id)
        ? task.plans.map((plan) => (plan.id === event.plan.id ? event.plan : plan))
        : [...task.plans, event.plan];
      return { ...task, plans, active_plan_id: event.plan.id, updated_at: event.plan.updated_at };
    });
  }

  async function hydrate(): Promise<void> {
    if (!isTauriRuntime) {
      runtimeError = "Open the Tauri desktop window to run Rust tools.";
      return;
    }
    try {
      const [loadedTasks, loadedArchivedTasks, loadedSettings] = await Promise.all([
        listTasks(),
        listArchivedTasks(),
        getSettings()
      ]);
      tasks = loadedTasks;
      archivedTasks = loadedArchivedTasks;
      settings = loadedSettings;
    } catch (error) {
      runtimeError = error instanceof Error ? error.message : String(error);
    }
  }

  async function refreshSelectedTask(): Promise<void> {
    if (!isTauriRuntime || !selectedTaskId || pollInFlight) return;
    pollInFlight = true;
    try {
      const task = await getTask(selectedTaskId);
      setSelectedTask(task);
      if (task.status === "waiting_approval") {
        pendingApproval =
          task.approval_requests.find((request) => request.status === "pending") ?? null;
      } else if (pendingApproval?.task_id === task.id) {
        pendingApproval = null;
      }
    } catch {
      // The event stream may disappear while the app is closing or a task is deleted.
      // Keep the current view; the next poll or an explicit selection will recover it.
    } finally {
      pollInFlight = false;
    }
  }

  onMount(() => {
    let unlisten: (() => void)[] = [];
    let disposed = false;
    const initialize = async () => {
      try {
        await hydrate();
        if (disposed) return;
        unlisten = await subscribeToEvents({
          task_created: handleCreated,
          task_status: handleStatus,
          task_message: replaceMessage,
          task_step: replaceStep,
          task_tool_call: replaceToolCall,
          task_tool_result: replaceToolResult,
          task_approval_required: handleApproval,
          task_completed: handleCompleted,
          task_failed: handleFailed,
          task_cancelled: handleCancelled,
          task_plan: handlePlan
        });
        if (!disposed) await refreshSelectedTask();
      } catch (error) {
        runtimeError = error instanceof Error ? error.message : String(error);
      }
    };
    void initialize();
    pollTimer = setInterval(() => {
      if (!disposed) void refreshSelectedTask();
    }, 650);
    return () => {
      disposed = true;
      if (pollTimer) clearInterval(pollTimer);
      unlisten.forEach((remove) => remove());
    };
  });

  function newTask(): void {
    selectedTask = null;
    selectedTaskId = "";
    pendingApproval = null;
    actionError = "";
  }

  async function sendTask(prompt: string): Promise<void> {
    actionError = "";
    if (!isTauriRuntime) {
      actionError = "The desktop runtime is required to start a task.";
      return;
    }
    if (settings.demo_mode) {
      actionError = "Configure an API key in Settings before sending a task.";
      return;
    }
    try {
      const task = selectedTask
        ? await continueTask(selectedTask.id, prompt)
        : await createTask(prompt);
      handleCreated(task);
    } catch (error) {
      actionError = error instanceof Error ? error.message : String(error);
    }
  }

  async function stopCurrentTask(): Promise<void> {
    if (!selectedTask) return;
    try {
      await stopTask(selectedTask.id);
    } catch (error) {
      actionError = error instanceof Error ? error.message : String(error);
    }
  }

  async function retryCurrentTask(): Promise<void> {
    if (!selectedTask) return;
    actionError = "";
    try {
      const task = await retryTask(selectedTask.id);
      setSelectedTask(task);
    } catch (error) {
      actionError = error instanceof Error ? error.message : String(error);
    }
  }

  async function selectTask(taskId: string): Promise<void> {
    actionError = "";
    try {
      setSelectedTask(await getTask(taskId));
    } catch (error) {
      actionError = error instanceof Error ? error.message : String(error);
    }
  }

  async function archiveProject(taskId: string): Promise<void> {
    actionError = "";
    try {
      const summary = await archiveTask(taskId);
      upsertSummary(summary);
      if (selectedTaskId === taskId) newTask();
    } catch (error) {
      actionError = error instanceof Error ? error.message : String(error);
    }
  }

  async function restoreProject(taskId: string): Promise<void> {
    actionError = "";
    try {
      const summary = await restoreTask(taskId);
      upsertSummary(summary);
      if (selectedTask?.id === taskId) {
        selectedTask = {
          ...selectedTask,
          archived: false,
          updated_at: summary.updated_at
        };
      }
    } catch (error) {
      actionError = error instanceof Error ? error.message : String(error);
    }
  }

  async function deleteProject(taskId: string): Promise<void> {
    actionError = "";
    try {
      await deleteTask(taskId);
      tasks = tasks.filter((task) => task.id !== taskId);
      archivedTasks = archivedTasks.filter((task) => task.id !== taskId);
      if (selectedTaskId === taskId) newTask();
    } catch (error) {
      actionError = error instanceof Error ? error.message : String(error);
    }
  }

  async function openSettings(): Promise<void> {
    actionError = "";
    if (!isTauriRuntime) {
      runtimeError = "Settings are available inside the Tauri desktop window.";
      return;
    }
    try {
      settings = await getSettings();
      settingsVisible = true;
    } catch (error) {
      actionError = error instanceof Error ? error.message : String(error);
    }
  }

  async function saveSettings(input: SettingsInput): Promise<void> {
    settingsSaving = true;
    actionError = "";
    try {
      settings = await updateSettings(input);
      settingsVisible = false;
    } catch (error) {
      actionError = error instanceof Error ? error.message : String(error);
    } finally {
      settingsSaving = false;
    }
  }

  async function decideApproval(approved: boolean): Promise<void> {
    if (!pendingApproval) return;
    approvalBusy = true;
    try {
      const delivered = await respondToApproval(
        pendingApproval.task_id,
        pendingApproval.id,
        approved
      );
      if (!delivered) actionError = "This approval request is no longer active.";
      pendingApproval = null;
    } catch (error) {
      actionError = error instanceof Error ? error.message : String(error);
    } finally {
      approvalBusy = false;
    }
  }
</script>

<div class="app-shell">
  <div class="workspace">
    <Sidebar
      {tasks}
      selectedTaskId={selectedTaskId}
      {archivedTasks}
      onNewTask={newTask}
      onSelectTask={selectTask}
      onArchiveTask={archiveProject}
      onRestoreTask={restoreProject}
      onDeleteTask={deleteProject}
      onOpenSettings={openSettings}
    />

    <main class="main-panel">
      {#if selectedTask}
      <div class="task-header">
        <div class="task-header-copy">
          <span class="eyebrow">Project</span>
          <h1>{selectedTask.title}</h1>
          <p>{selectedTask.prompt}</p>
        </div>
        <div class="task-header-meta">
          <StatusBadge status={selectedStatus} />
          <button
            class="icon-button"
            type="button"
            title="Open execution trace"
            aria-label="Open execution trace"
            on:click={() => (traceCollapsed = false)}
          >
            <ListTree size={17} />
          </button>
        </div>
      </div>
      {/if}

      {#if runtimeError}
        <div class="runtime-notice"><WifiOff size={15} /><span>{runtimeError}</span></div>
      {/if}
      {#if actionError}
        <div class="action-notice"><span class="status-dot status-dot-failed"></span><span>{actionError}</span></div>
      {/if}

      <Conversation task={selectedTask} onRetry={retryCurrentTask} />

      <div class="composer-area">
        <Composer
          busy={taskIsBusy}
          disabled={!isTauriRuntime}
          placeholder={settings.demo_mode ? "Configure an API key in Settings to start..." : "Describe a task..."}
          onSend={sendTask}
          onStop={stopCurrentTask}
        />
      </div>
    </main>

    {#if !traceCollapsed}
      <RustTrace task={selectedTask} collapsed={false} onToggle={() => (traceCollapsed = true)} />
    {/if}
  </div>
</div>

<SettingsPanel
  visible={settingsVisible}
  {settings}
  saving={settingsSaving}
  onClose={() => (settingsVisible = false)}
  onSave={saveSettings}
/>

<ApprovalDialog request={pendingApproval} busy={approvalBusy} onDecision={decideApproval} />
