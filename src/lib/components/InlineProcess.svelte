<script lang="ts">
  import {
    ChevronDown,
    CircleCheck,
    CircleX,
    LoaderCircle,
    Minus,
    Wrench
  } from "lucide-svelte";
  import type {
    AgentMessageToolCall,
    AgentPlanStep,
    AgentStep,
    AssistantPart,
    AssistantToolPart,
    Task,
    TaskMessage,
    ToolCall,
    ToolPresentation
  } from "../types";
  import ToolCallView from "./ToolCallView.svelte";

  export let task: Task | null = null;
  export let message: TaskMessage | null = null;
  export let part: AssistantToolPart | null = null;
  export let toolGroup: Array<{ message: TaskMessage; part: AssistantToolPart }> = [];

  type ProcessItem = {
    id: string;
    kind: "plan" | "step" | "tool";
    title: string;
    status: string;
    duration: string;
    detail: string | null;
    arguments: unknown | undefined;
    result: string | null;
    error: string | null;
    tool?: ToolPresentation;
  };

  let items: ProcessItem[] = [];

  $: items = buildItems(task, message, part);
  $: isWorking =
    task !== null &&
    ["planning", "executing", "verifying", "waiting_approval"].includes(task.status);
  $: isContextGroup = items.length > 1 && items.every((item) => item.kind === "tool" && item.tool && isContextTool(item.tool));
  $: isDirectTool = part !== null && items.length === 1 && items[0]?.kind === "tool" && !!items[0].tool;
  $: summaryLabel = isContextGroup ? (isWorking ? "Gathering context" : "Gathered context") : isWorking ? "Working" : "View work";

  function buildItems(
    currentTask: Task | null,
    currentMessage: TaskMessage | null,
    currentPart: AssistantToolPart | null
  ): ProcessItem[] {
    if (!currentTask) return [];

    const hasActualWork =
      currentTask.tool_calls.length > 0 ||
      currentTask.steps.some((step) => step.phase !== "plan");
    if (!hasActualWork && !currentPart) return [];

    if (currentPart) {
      if (toolGroup.length === 0 && !currentMessage) {
        const record = findToolRecordForPart(currentTask.tool_calls, currentPart, 0);
        return [toolItemFromPart(currentPart, undefined, record)];
      }

      const groupedParts = toolGroup.length > 0
        ? toolGroup
        : contextParts(currentMessage!, currentPart).map((part) => ({ message: currentMessage!, part }));
      const usedRecords = new Set<string>();
      return groupedParts.map((entry) => {
        const request = entry.message.tool_calls?.find(
          (call, index) => call.id === entry.part.call_id || index === entry.part.index
        );
        const record = request
          ? findToolRecord(currentTask.tool_calls, request, usedRecords, entry.message.created_at)
          : findToolRecordForPart(currentTask.tool_calls, entry.part, entry.message.created_at);
        return toolItemFromPart(entry.part, request, record);
      });
    }

    const requestedCalls = currentMessage?.tool_calls ?? [];
    if (requestedCalls.length > 0) {
      const usedRecords = new Set<string>();
      return requestedCalls.map((call) => {
        const record = findToolRecord(
          currentTask.tool_calls,
          call,
          usedRecords,
          currentMessage?.created_at ?? 0
        );
        return toolItemFromRequest(call, record);
      });
    }

    const planItems = (currentTask.plans.at(-1)?.steps ?? []).map(planItem);
    const stepItems = currentTask.steps
      .filter((step) => step.phase !== "plan")
      .map(stepItem);
    const toolItems = currentTask.tool_calls.map(toolItem);
    return [...planItems, ...stepItems, ...toolItems];
  }

  function findToolRecordForPart(
    records: ToolCall[],
    currentPart: AssistantToolPart,
    createdAfter: number
  ): ToolCall | null {
    const byModelId = records.find(
      (record) =>
        record.started_at >= createdAfter &&
        record.model_tool_call_id === currentPart.call_id
    );
    if (byModelId) return byModelId;
    return (
      records.find(
        (record) =>
          record.started_at >= createdAfter &&
          record.name === currentPart.name
      ) ?? null
    );
  }

  function findToolRecord(
    records: ToolCall[],
    request: AgentMessageToolCall,
    usedRecords: Set<string>,
    createdAfter: number
  ): ToolCall | null {
    const requestedArguments = parseArguments(request.function.arguments);
    const exactMatch = records.find(
      (record) =>
        !usedRecords.has(record.id) &&
        record.started_at >= createdAfter &&
        record.name === request.function.name &&
        areEquivalent(record.arguments, requestedArguments)
    );
    const fallback = records.find(
      (record) =>
        !usedRecords.has(record.id) &&
        record.started_at >= createdAfter &&
        record.name === request.function.name
    );
    const record = exactMatch ?? fallback ?? null;
    if (record) usedRecords.add(record.id);
    return record;
  }

  function areEquivalent(left: unknown, right: unknown): boolean {
    try {
      return JSON.stringify(left) === JSON.stringify(right);
    } catch {
      return false;
    }
  }

  function parseArguments(argumentsText: string): unknown {
    try {
      return JSON.parse(argumentsText);
    } catch {
      return argumentsText;
    }
  }

  function planItem(step: AgentPlanStep): ProcessItem {
    return {
      id: step.id,
      kind: "plan",
      title: step.title,
      status: step.status,
      duration: "",
      detail: step.notes || step.description || null,
      arguments: undefined,
      result: null,
      error: null
    };
  }

  function stepItem(step: AgentStep): ProcessItem {
    return {
      id: step.id,
      kind: "step",
      title: step.title,
      status: step.status,
      duration: duration(step.duration_ms),
      detail: step.detail,
      arguments: undefined,
      result: null,
      error: null
    };
  }

  function toolItem(call: ToolCall): ProcessItem {
    return {
      id: call.id,
      kind: "tool",
      title: call.name,
      status: call.status,
      duration: duration(call.duration_ms),
      detail: null,
      arguments: call.arguments,
      result: call.result,
      error: call.error,
      tool: {
        id: call.id,
        name: call.name,
        arguments: call.arguments,
        status: call.status,
        duration_ms: call.duration_ms,
        result: call.result,
        error: call.error
      }
    };
  }

  function toolItemFromRequest(request: AgentMessageToolCall, record: ToolCall | null): ProcessItem {
    if (record) return toolItem(record);
    return {
      id: request.id,
      kind: "tool",
      title: request.function.name,
      status: "pending",
      duration: "queued",
      detail: null,
      arguments: parseArguments(request.function.arguments),
      result: null,
      error: null,
      tool: {
        id: request.id,
        name: request.function.name,
        arguments: parseArguments(request.function.arguments),
        status: "pending",
        duration_ms: null,
        result: null,
        error: null
      }
    };
  }

  function toolItemFromPart(
    currentPart: AssistantToolPart,
    request: AgentMessageToolCall | undefined,
    record: ToolCall | null
  ): ProcessItem {
    if (record) return toolItem(record);
    if (request) return toolItemFromRequest(request, null);
    return {
      id: currentPart.id,
      kind: "tool",
      title: currentPart.name || "Tool call",
      status: "pending",
      duration: "queued",
      detail: null,
      arguments: undefined,
      result: null,
      error: null,
      tool: {
        id: currentPart.id,
        name: currentPart.name || "Tool call",
        arguments: undefined,
        status: "pending",
        duration_ms: null,
        result: null,
        error: null
      }
    };
  }

  function contextParts(message: TaskMessage, currentPart: AssistantToolPart): AssistantToolPart[] {
    const parts = orderedParts(message);
    const index = parts.findIndex((part) => part.id === currentPart.id);
    if (index < 0 || !isContextPart(message, currentPart)) return [currentPart];

    let start = index;
    let end = index;
    while (start > 0 && isContextPart(message, parts[start - 1])) start -= 1;
    while (end < parts.length - 1 && isContextPart(message, parts[end + 1])) end += 1;
    return parts.slice(start, end + 1).filter((part): part is AssistantToolPart => part.type === "tool");
  }

  function orderedParts(message: TaskMessage): AssistantPart[] {
    if (message.parts && message.parts.length > 0) return message.parts;
    return (message.tool_calls ?? []).map((call, index) => ({
      type: "tool" as const,
      id: `${message.id}:tool:${index}`,
      index,
      call_id: call.id,
      name: call.function.name
    }));
  }

  function isContextPart(message: TaskMessage, part: AssistantPart): part is AssistantToolPart {
    if (part.type !== "tool") return false;
    const name = part.name.toLowerCase();
    if (name === "rust_web_search") return true;
    if (name !== "rust_files" && name !== "rust_sandbox_files") return false;

    const request = message.tool_calls?.find(
      (call, index) => call.id === part.call_id || index === part.index
    );
    if (!request) return true;
    const argumentsValue = parseArguments(request.function.arguments);
    if (argumentsValue === null || typeof argumentsValue !== "object" || Array.isArray(argumentsValue)) return true;
    const operation = (argumentsValue as Record<string, unknown>).operation;
    return operation === undefined || operation === "list" || operation === "read" || operation === "exists";
  }

  function isContextTool(item: ToolPresentation): boolean {
    const name = item.name.toLowerCase();
    if (name === "rust_web_search") return true;
    if (name !== "rust_files" && name !== "rust_sandbox_files") return false;
    const argumentsValue = item.arguments;
    if (!argumentsValue || typeof argumentsValue !== "object" || Array.isArray(argumentsValue)) return true;
    const operation = (argumentsValue as Record<string, unknown>).operation;
    return operation === undefined || operation === "list" || operation === "read" || operation === "exists";
  }

  function contextSummary(): string {
    const counts = { read: 0, list: 0, search: 0 };
    for (const item of items) {
      if (!item.tool) continue;
      const name = item.tool.name.toLowerCase();
      if (name === "rust_web_search") counts.search += 1;
      else {
        const operation =
          item.tool.arguments && typeof item.tool.arguments === "object" && !Array.isArray(item.tool.arguments)
            ? (item.tool.arguments as Record<string, unknown>).operation
            : undefined;
        if (operation === "list") counts.list += 1;
        else counts.read += 1;
      }
    }
    const parts: string[] = [];
    if (counts.read) parts.push(`${counts.read} read${counts.read === 1 ? "" : "s"}`);
    if (counts.search) parts.push(`${counts.search} search${counts.search === 1 ? "" : "es"}`);
    if (counts.list) parts.push(`${counts.list} list${counts.list === 1 ? "" : "s"}`);
    return parts.join(", ") || `${items.length} tools`;
  }

  function duration(durationMs: number | null): string {
    if (durationMs === null) return "active";
    if (durationMs < 1000) return `${durationMs} ms`;
    return `${(durationMs / 1000).toFixed(1)} s`;
  }

  function statusLabel(status: string): string {
    switch (status) {
      case "in_progress":
      case "running":
        return "Working";
      case "completed":
        return "Done";
      case "failed":
      case "blocked":
        return "Blocked";
      case "cancelled":
        return "Cancelled";
      default:
        return "Queued";
    }
  }

  function formatArguments(argumentsValue: unknown): string {
    try {
      return JSON.stringify(argumentsValue, null, 2) ?? "{}";
    } catch {
      return String(argumentsValue);
    }
  }
</script>

{#if items.length > 0}
  {#if isDirectTool && items[0]?.tool}
    <ToolCallView item={items[0].tool} />
  {:else if isContextGroup}
    <details class:assistant-process-live={isWorking} class="assistant-context-group" aria-label="Context gathering">
      <summary>
        <span class="assistant-process-icon assistant-context-icon" aria-hidden="true"><Wrench size={13} /></span>
        <span class="assistant-process-label">{summaryLabel}</span>
        <span class="assistant-process-count">{contextSummary()}</span>
        <ChevronDown class="assistant-process-chevron" size={14} aria-hidden="true" />
      </summary>
      <div class="assistant-context-list">
        {#each items as item (item.id)}
          {#if item.tool}<ToolCallView item={item.tool} nested />{/if}
        {/each}
      </div>
    </details>
  {:else}
    <details class:assistant-process-live={isWorking} class="assistant-process" aria-label="Assistant process">
      <summary>
        <span class="assistant-process-icon" aria-hidden="true"><Wrench size={13} /></span>
        <span class="assistant-process-label">{summaryLabel}</span>
        <span class="assistant-process-count">{items.length} {items.length === 1 ? "item" : "items"}</span>
        <ChevronDown class="assistant-process-chevron" size={14} aria-hidden="true" />
      </summary>

      <div class="assistant-process-list">
        {#each items as item (item.id)}
          {#if item.kind === "tool" && item.tool}
            <ToolCallView item={item.tool} nested />
          {:else}
            <details class="assistant-process-item">
              <summary>
                <span class="assistant-process-marker process-status-{item.status}" aria-hidden="true">
                  {#if item.status === "running" || item.status === "in_progress"}
                    <LoaderCircle size={11} />
                  {:else if item.status === "completed"}
                    <CircleCheck size={11} />
                  {:else if item.status === "failed" || item.status === "blocked" || item.status === "cancelled"}
                    <CircleX size={11} />
                  {:else}
                    <Minus size={11} />
                  {/if}
                </span>
                <span class="assistant-process-copy">
                  <strong>{item.title}</strong>
                  <span>{statusLabel(item.status)}{item.duration ? ` - ${item.duration}` : ""}</span>
                </span>
                <ChevronDown class="assistant-process-chevron" size={13} aria-hidden="true" />
              </summary>
              <div class="assistant-process-detail">
                {#if item.detail}<p>{item.detail}</p>{/if}
                {#if item.arguments !== undefined}
                  <span>Arguments</span>
                  <pre>{formatArguments(item.arguments)}</pre>
                {/if}
                {#if item.result}
                  <span>Result</span>
                  <pre>{item.result}</pre>
                {/if}
                {#if item.error}
                  <span class="assistant-process-error">Error</span>
                  <pre class="assistant-process-error">{item.error}</pre>
                {/if}
              </div>
            </details>
          {/if}
        {/each}
      </div>
    </details>
  {/if}
{/if}
