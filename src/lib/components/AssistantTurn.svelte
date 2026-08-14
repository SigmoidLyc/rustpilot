<script lang="ts">
  import { Bot } from "lucide-svelte";
  import InlineProcess from "./InlineProcess.svelte";
  import { renderMarkdown } from "../markdown";
  import type { AssistantPart, AssistantToolPart, Task, TaskMessage } from "../types";

  export let task: Task;
  export let messages: TaskMessage[] = [];

  let hasEmbeddedProcess = false;
  let fallbackProcessMessage: TaskMessage | null = null;
  let isStreaming = false;
  let activeStreamingMessage: TaskMessage | null = null;
  let pendingReasoningMessage: TaskMessage | null = null;
  let hasStreamingOutput = false;
  let hasStreamingReasoning = false;
  let showProgressLabel = false;
  type TurnPart = { message: TaskMessage; part: AssistantPart };
  type TurnToolPart = { message: TaskMessage; part: AssistantToolPart };
  let turnParts: TurnPart[] = [];

  $: hasEmbeddedProcess = messages.some((message) =>
    orderedParts(message).some((part) => part.type === "tool")
  );
  $: fallbackProcessMessage = hasEmbeddedProcess ? null : (messages.at(-1) ?? null);
  $: activeStreamingMessage = messages.filter((message) => message.streaming).at(-1) ?? null;
  $: isStreaming = activeStreamingMessage !== null;
  $: pendingReasoningMessage =
    !isStreaming &&
    ["planning", "executing", "verifying", "waiting_approval"].includes(task.status)
      ? messages
          .filter(
            (message) =>
              Boolean(message.reasoning?.trim()) &&
              message.content.trim().length === 0 &&
              message.tool_calls.length === 0
          )
          .at(-1) ?? null
      : null;
  $: showProgressLabel = isStreaming || pendingReasoningMessage !== null;
  $: hasStreamingOutput = activeStreamingMessage
    ? orderedParts(activeStreamingMessage).some(
        (part) =>
          part.type === "tool" ||
          (part.type === "text" && activeStreamingMessage && partText(activeStreamingMessage, part).trim().length > 0)
      )
    : false;
  $: hasStreamingReasoning = Boolean(
    activeStreamingMessage?.reasoning?.trim() || pendingReasoningMessage?.reasoning?.trim()
  );
  $: turnParts = messages.flatMap((message) =>
    orderedParts(message).map((part) => ({ message, part }))
  );

  function orderedParts(message: TaskMessage): AssistantPart[] {
    if (message.parts && message.parts.length > 0) return message.parts;

    const parts: AssistantPart[] = [];
    if ((message.reasoning ?? "").length > 0) {
      parts.push({
        type: "reasoning",
        id: `${message.id}:reasoning`,
        start: 0,
        end: (message.reasoning ?? "").length
      });
    }
    if (message.content.length > 0) {
      parts.push({
        type: "text",
        id: `${message.id}:text`,
        start: 0,
        end: message.content.length
      });
    }
    for (const [index, call] of (message.tool_calls ?? []).entries()) {
      parts.push({
        type: "tool",
        id: `${message.id}:tool:${index}`,
        index,
        call_id: call.id,
        name: call.function.name
      });
    }
    return parts;
  }

  function partText(message: TaskMessage, part: AssistantPart): string {
    if (part.type === "text") return message.content.slice(part.start, part.end);
    if (part.type === "reasoning") return (message.reasoning ?? "").slice(part.start, part.end);
    return "";
  }

  function isLastPart(message: TaskMessage, part: AssistantPart): boolean {
    const parts = orderedParts(message);
    return parts.at(-1)?.id === part.id;
  }

  function isContextTool(
    message: TaskMessage | undefined,
    part: AssistantPart | undefined
  ): part is AssistantToolPart {
    if (!message || !part || part.type !== "tool") return false;
    const name = part.name.toLowerCase();
    if (name === "rust_web_search") return true;
    if (name !== "rust_files" && name !== "rust_sandbox_files") return false;

    const request = message.tool_calls?.find(
      (call, index) => call.id === part.call_id || index === part.index
    );
    if (!request) return true;
    try {
      const argumentsValue = JSON.parse(request.function.arguments) as unknown;
      if (argumentsValue === null || typeof argumentsValue !== "object" || Array.isArray(argumentsValue)) return true;
      const operation = (argumentsValue as Record<string, unknown>).operation;
      return operation === undefined || operation === "list" || operation === "read" || operation === "exists";
    } catch {
      return true;
    }
  }

  function isGroupedToolPart(part: AssistantPart, index: number): boolean {
    if (!isContextTool(turnParts[index]?.message, part) || index === 0) return false;
    return isContextTool(turnParts[index - 1]?.message, turnParts[index - 1]?.part);
  }

  function contextGroup(index: number): TurnToolPart[] {
    if (!isContextTool(turnParts[index]?.message, turnParts[index]?.part)) return [];
    let start = index;
    let end = index;
    while (start > 0 && isContextTool(turnParts[start - 1]?.message, turnParts[start - 1]?.part)) start -= 1;
    while (end < turnParts.length - 1 && isContextTool(turnParts[end + 1]?.message, turnParts[end + 1]?.part)) end += 1;
    return turnParts
      .slice(start, end + 1)
      .filter((entry): entry is TurnToolPart => entry.part.type === "tool");
  }
</script>

<article class="message message-assistant assistant-turn">
  <div class="message-avatar" aria-hidden="true">
    <Bot size={16} />
  </div>
  <div class="message-body">
    <div class="message-meta">
      <span>RustPilot</span>
      {#if showProgressLabel && !hasStreamingOutput}
        <span class="streaming-label">{hasStreamingReasoning ? "thinking" : "planning"}</span>
      {/if}
    </div>

    {#each turnParts as turnPart, turnIndex (turnPart.message.id + ":" + turnPart.part.id)}
      {@const message = turnPart.message}
      {@const part = turnPart.part}
      {#if part.type === "text" && partText(message, part).trim()}
          <div class="message-content markdown-content assistant-turn-segment">
            {@html renderMarkdown(partText(message, part))}
            <span
              class:visible={message.streaming && isLastPart(message, part)}
              class="streaming-cursor"
              aria-hidden="true"
            ></span>
          </div>
      {:else if part.type === "tool"}
        {#if !isGroupedToolPart(part, turnIndex)}
          <InlineProcess {task} {message} part={part} toolGroup={contextGroup(turnIndex)} />
        {/if}
      {/if}
    {/each}

    {#each messages as message (message.id)}
      {#if orderedParts(message).length === 0 && message.id === fallbackProcessMessage?.id}
        <InlineProcess {task} {message} />
      {/if}
    {/each}
  </div>
</article>
