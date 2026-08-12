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
    getTaskEvents,
    isTauriRuntime,
    listArchivedTasks,
    listTasks,
    listProjects,
    listRecentProjects,
    openProject,
    pickProject,
    closeProject,
    touchProject,
    respondToApproval,
    restoreTask,
    retryTask,
    stopTask,
    subscribeToEvents,
    updateSettings
  } from "./lib/api";
  import { serializeFiles } from "./lib/attachments";
  import type {
    AgentStep,
    AssistantPart,
    AttachmentPathInput,
    ApprovalMode,
    ApprovalRequest,
    SettingsInput,
    SettingsView,
    Task,
    TaskMessage,
    TaskPlanEvent,
    TaskStatusEvent,
    TaskSummary,
    ProjectSummary,
    TaskCompletedEvent,
    TaskFailedEvent,
    TaskCancelledEvent,
    TaskEvent,
    PersistedStreamEvent,
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
    approval_mode: "guarded",
    remembered_approvals: 0,
    demo_mode: true,
    available_tools: []
  };

  let tasks: TaskSummary[] = [];
  let archivedTasks: TaskSummary[] = [];
  let projects: ProjectSummary[] = [];
  let recentlyClosedProjects: ProjectSummary[] = [];
  let selectedWorkspace = "";
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
  const eventCursors = new Map<string, number>();
  let syncingTaskId: string | null = null;
  let bufferedEvents: TaskEvent[] = [];
  let syncGeneration = 0;

  $: selectedStatus = selectedTask?.status ?? "idle";
  $: taskIsBusy =
    selectedTask !== null &&
    ["planning", "executing", "verifying", "waiting_approval"].includes(selectedTask.status);

  function summaryForTask(task: Task): TaskSummary {
    return {
      id: task.id,
      title: task.title,
      workspace: task.workspace,
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

  function setSelectedTask(task: Task, resetCursor = false): void {
    selectedTaskId = task.id;
    selectedTask = task;
    selectedWorkspace = task.workspace;
    const knownCursor = eventCursors.get(task.id) ?? 0;
    eventCursors.set(task.id, resetCursor ? task.event_seq : Math.max(knownCursor, task.event_seq));
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
        approval_requests: task.approval_requests.some((item) => item.id === request.id)
          ? task.approval_requests.map((item) => (item.id === request.id ? request : item))
          : [...task.approval_requests, request],
        updated_at: Date.now()
      }));
      pendingApproval = request;
    }
  }

  function handleApprovalUpdated(request: ApprovalRequest): void {
    if (selectedTask?.id !== request.task_id) return;
    updateSelected((task) => ({
      ...task,
      approval_requests: task.approval_requests.map((item) =>
        item.id === request.id ? request : item
      ),
      updated_at: Date.now()
    }));
    pendingApproval = request.status === "pending" ? request : null;
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

  function handleTaskSummary(summary: TaskSummary): void {
    upsertSummary(summary);
    if (selectedTask?.id === summary.id) {
      if (summary.archived) newTask();
      else {
        updateSelected((task) => ({
          ...task,
          archived: false,
          updated_at: summary.updated_at
        }));
      }
    }
  }

  function handleTaskDeleted(summary: TaskSummary): void {
    tasks = tasks.filter((task) => task.id !== summary.id);
    archivedTasks = archivedTasks.filter((task) => task.id !== summary.id);
    if (selectedTaskId === summary.id) newTask();
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

  function createStreamingMessage(taskId: string, messageId: string): TaskMessage {
    return {
      id: messageId,
      task_id: taskId,
      role: "assistant",
      content: "",
      created_at: Date.now(),
      streaming: true,
      parts: [],
      tool_calls: [],
      tool_call_id: null,
      name: null,
      base64_image: null,
      attachments: []
    };
  }

  function reduceStreamMessage(message: TaskMessage, event: PersistedStreamEvent): TaskMessage {
    const next: TaskMessage = {
      ...message,
      streaming: true,
      parts: [...(message.parts ?? [])]
    };
    if (event.kind === "text_delta" && event.delta) {
      const parts = next.parts ?? [];
      const last = parts[parts.length - 1];
      const start = last?.type === "text" ? last.end : next.content.length;
      const end = start + event.delta.length;
      next.content += event.delta;
      if (last?.type === "text" && last.end === start) {
        parts[parts.length - 1] = { ...last, end };
      } else {
        parts.push({
          type: "text",
          id: `${next.id}:text:${start}`,
          start,
          end
        });
      }
      next.parts = parts;
      return next;
    }
    if (event.kind !== "tool_call_delta") return next;
    const parts = next.parts ?? [];
    const callId = event.id ?? "";
    const part = parts.find(
      (item): item is Extract<AssistantPart, { type: "tool" }> =>
        item.type === "tool" && item.index === event.index
    );
    if (part) {
      const nextCallId = callId
        ? part.call_id.startsWith("stream:")
          ? callId
          : part.call_id === callId || part.call_id.endsWith(callId)
            ? part.call_id
            : `${part.call_id}${callId}`
        : part.call_id;
      part.call_id = nextCallId;
      if (event.name) part.name += event.name;
    } else {
      parts.push({
        type: "tool",
        id: `${next.id}:tool:${event.index}`,
        index: event.index,
        call_id: callId || `stream:${event.index}`,
        name: event.name ?? ""
      });
    }
    next.parts = parts;
    return next;
  }

  function applyStreamTaskEvent(event: TaskEvent): void {
    if (!selectedTask || selectedTask.id !== event.task_id || !event.message_id) return;
    const streamEvent = event.payload as PersistedStreamEvent;
    if (!streamEvent || typeof streamEvent.kind !== "string") return;
    const messages = [...selectedTask.messages];
    const index = messages.findIndex((message) => message.id === event.message_id);
    const current = index >= 0 ? messages[index] : createStreamingMessage(event.task_id, event.message_id);
    const next = reduceStreamMessage(current, streamEvent);
    if (index >= 0) messages[index] = next;
    else messages.push(next);
    updateSelected((task) => ({ ...task, messages, event_seq: event.seq, updated_at: Date.now() }));
  }

  function applyTaskEventNow(event: TaskEvent): void {
    const cursor = eventCursors.get(event.task_id) ?? 0;
    if (event.seq <= cursor) return;
    eventCursors.set(event.task_id, event.seq);
    if (event.kind === "stream") {
      applyStreamTaskEvent(event);
      return;
    }
    switch (event.event) {
      case "task_created": {
        const task = event.payload as Task;
        upsertSummary(summaryForTask(task));
        if (selectedTask?.id === task.id && selectedTask.messages.length === 0) {
          setSelectedTask(task);
        }
        break;
      }
      case "task_status":
        handleStatus(event.payload as TaskStatusEvent);
        break;
      case "task_message":
        replaceMessage(event.payload as TaskMessage);
        break;
      case "task_step":
        replaceStep(event.payload as AgentStep);
        break;
      case "task_tool_call":
        replaceToolCall(event.payload as ToolCall);
        break;
      case "task_tool_result":
        replaceToolResult(event.payload as ToolResult);
        break;
      case "task_approval_required":
        handleApproval(event.payload as ApprovalRequest);
        break;
      case "task_approval_updated":
        handleApprovalUpdated(event.payload as ApprovalRequest);
        break;
      case "task_completed":
        handleCompleted(event.payload as TaskCompletedEvent);
        break;
      case "task_failed":
        handleFailed(event.payload as TaskFailedEvent);
        break;
      case "task_cancelled":
        handleCancelled(event.payload as TaskCancelledEvent);
        break;
      case "task_summary":
        handleTaskSummary(event.payload as TaskSummary);
        break;
      case "task_deleted":
        handleTaskDeleted(event.payload as TaskSummary);
        break;
      case "task_plan":
        handlePlan(event.payload as TaskPlanEvent);
        break;
    }
    if (selectedTask?.id === event.task_id) {
      updateSelected((task) => ({ ...task, event_seq: event.seq }));
    }
  }

  function handleTaskEvent(event: TaskEvent): void {
    if (syncingTaskId === event.task_id) {
      bufferedEvents = [...bufferedEvents, event];
      return;
    }
    applyTaskEventNow(event);
  }

  async function syncTask(taskId: string): Promise<void> {
    const generation = ++syncGeneration;
    syncingTaskId = taskId;
    bufferedEvents = [];
    try {
      let page = await getTaskEvents(taskId);
      if (generation !== syncGeneration) return;
      if (page.snapshot) {
        setSelectedTask(page.snapshot, true);
      }
      while (true) {
        for (const event of page.events) applyTaskEventNow(event);
        if (!page.has_more) break;
        page = await getTaskEvents(taskId, eventCursors.get(taskId) ?? page.cursor);
        if (generation !== syncGeneration) return;
      }
      const pending = bufferedEvents
        .filter((event) => event.task_id === taskId)
        .sort((left, right) => left.seq - right.seq);
      syncingTaskId = null;
      bufferedEvents = [];
      for (const event of pending) applyTaskEventNow(event);
      if (selectedTask?.id === taskId) {
        pendingApproval =
          selectedTask.status === "waiting_approval"
            ? selectedTask.approval_requests.find((request) => request.status === "pending") ?? null
            : null;
      }
    } catch {
      syncingTaskId = null;
      bufferedEvents = [];
      try {
        setSelectedTask(await getTask(taskId), true);
      } catch {
        // The task may have been deleted while the window was reconnecting.
      }
    }
  }

  async function hydrate(): Promise<void> {
    if (!isTauriRuntime) {
      runtimeError = "Open the Tauri desktop window to run Rust tools.";
      return;
    }
    try {
      const [loadedTasks, loadedArchivedTasks, loadedProjects, loadedRecentProjects, loadedSettings] = await Promise.all([
        listTasks(),
        listArchivedTasks(),
        listProjects(),
        listRecentProjects(),
        getSettings()
      ]);
      tasks = loadedTasks;
      archivedTasks = loadedArchivedTasks;
      projects = loadedProjects;
      recentlyClosedProjects = loadedRecentProjects;
      selectedWorkspace = loadedProjects[0]?.directory ?? loadedTasks[0]?.workspace ?? "";
      settings = loadedSettings;
    } catch (error) {
      runtimeError = error instanceof Error ? error.message : String(error);
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
          task_event: handleTaskEvent
        });
      } catch (error) {
        runtimeError = error instanceof Error ? error.message : String(error);
      }
    };
    void initialize();
    return () => {
      disposed = true;
      syncGeneration += 1;
      syncingTaskId = null;
      unlisten.forEach((remove) => remove());
    };
  });

  function newTask(): void {
    syncGeneration += 1;
    syncingTaskId = null;
    bufferedEvents = [];
    selectedTask = null;
    selectedTaskId = "";
    pendingApproval = null;
    actionError = "";
  }

  function newTaskInProject(directory: string): void {
    selectedWorkspace = directory;
    newTask();
    selectedWorkspace = directory;
  }

  async function refreshProjects(): Promise<void> {
    [projects, recentlyClosedProjects] = await Promise.all([listProjects(), listRecentProjects()]);
  }

  async function selectProject(directory: string): Promise<void> {
    actionError = "";
    try {
      await touchProject(directory);
      await refreshProjects();
      selectedWorkspace = directory;
      const task = tasks.find((item) => item.workspace === directory);
      if (task) await syncTask(task.id);
      else newTask();
    } catch (error) {
      actionError = error instanceof Error ? error.message : String(error);
    }
  }

  async function openProjectPath(path: string): Promise<void> {
    actionError = "";
    try {
      const project = await openProject(path);
      await refreshProjects();
      selectedWorkspace = project.directory;
      const task = tasks.find((item) => item.workspace === project.directory);
      if (task) await syncTask(task.id);
      else newTask();
    } catch (error) {
      actionError = error instanceof Error ? error.message : String(error);
    }
  }

  async function chooseProject(kind: "file" | "folder"): Promise<void> {
    actionError = "";
    try {
      const project = await pickProject(kind);
      if (project) {
        await refreshProjects();
        selectedWorkspace = project.directory;
        const task = tasks.find((item) => item.workspace === project.directory);
        if (task) await syncTask(task.id);
        else newTask();
      }
    } catch (error) {
      actionError = error instanceof Error ? error.message : String(error);
    }
  }

  async function closeProjectPath(directory: string): Promise<void> {
    actionError = "";
    try {
      await closeProject(directory);
      await refreshProjects();
      if (selectedWorkspace === directory) {
        const next = projects[0];
        if (next) await selectProject(next.directory);
        else {
          newTask();
          selectedWorkspace = "";
        }
      }
    } catch (error) {
      actionError = error instanceof Error ? error.message : String(error);
    }
  }

  async function sendTask(
    prompt: string,
    files: File[],
    paths: AttachmentPathInput[]
  ): Promise<boolean> {
    actionError = "";
    if (!isTauriRuntime) {
      actionError = "The desktop runtime is required to start a task.";
      return false;
    }
    if (settings.demo_mode) {
      actionError = "Configure an API key in Settings before sending a task.";
      return false;
    }
    try {
      const attachments = await serializeFiles(files);
      const task = selectedTask
        ? await continueTask(selectedTask.id, prompt, attachments, paths)
        : await createTask(prompt, attachments, paths, selectedWorkspace || undefined);
      handleCreated(task);
      await syncTask(task.id);
      return true;
    } catch (error) {
      actionError = error instanceof Error ? error.message : String(error);
      return false;
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
      await syncTask(task.id);
    } catch (error) {
      actionError = error instanceof Error ? error.message : String(error);
    }
  }

  async function selectTask(taskId: string): Promise<void> {
    actionError = "";
    await syncTask(taskId);
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

  async function changeApprovalMode(mode: ApprovalMode): Promise<boolean> {
    if (mode === settings.approval_mode) return true;
    actionError = "";
    try {
      settings = await updateSettings({
        api_base_url: settings.api_base_url,
        model: settings.model,
        api_key: null,
        max_steps: settings.max_steps,
        timeout_secs: settings.timeout_secs,
        prompt_cache: settings.prompt_cache,
        approval_mode: mode,
        clear_approval_rules: false
      });
      return true;
    } catch (error) {
      actionError = error instanceof Error ? error.message : String(error);
      return false;
    }
  }

  async function decideApproval(approved: boolean, remember = false): Promise<void> {
    if (!pendingApproval) return;
    approvalBusy = true;
    try {
      const delivered = await respondToApproval(
        pendingApproval.task_id,
        pendingApproval.id,
        approved,
        remember
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
      {projects}
      recentlyClosedProjects={recentlyClosedProjects}
      {selectedWorkspace}
      selectedTaskId={selectedTaskId}
      {archivedTasks}
      onNewTask={newTask}
      onNewTaskInProject={newTaskInProject}
      onSelectTask={selectTask}
      onSelectProject={selectProject}
      onOpenProject={openProjectPath}
      onPickProject={chooseProject}
      onCloseProject={closeProjectPath}
      onReopenProject={openProjectPath}
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
          approvalMode={settings.approval_mode}
          onApprovalModeChange={changeApprovalMode}
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
