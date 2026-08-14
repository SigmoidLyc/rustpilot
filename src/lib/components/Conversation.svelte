<script lang="ts">
  import { CircleAlert, RotateCcw } from "lucide-svelte";
  import AssistantTurn from "./AssistantTurn.svelte";
  import InlineProcess from "./InlineProcess.svelte";
  import MessageBubble from "./MessageBubble.svelte";
  import type { Task, TaskMessage } from "../types";

  export let task: Task | null = null;
  export let onRetry: () => void;

  type ConversationEntry =
    | { id: string; kind: "user"; message: TaskMessage }
    | { id: string; kind: "assistant"; messages: TaskMessage[] };

  let entries: ConversationEntry[] = [];
  let assistantMessages: TaskMessage[] = [];
  let hasToolCallMessage = false;

  $: entries = conversationEntries(task);
  $: assistantMessages = task
    ? task.messages.filter(
        (message) => message.role === "assistant" && isVisibleAssistantMessage(message, task)
      )
    : [];
  $: hasToolCallMessage = assistantMessages.some((message) => message.tool_calls.length > 0);

  function isTaskBusy(currentTask: Task | null): boolean {
    return (
      currentTask !== null &&
      ["planning", "executing", "verifying", "waiting_approval"].includes(currentTask.status)
    );
  }

  function isVisibleAssistantMessage(message: TaskMessage, currentTask: Task | null): boolean {
    if (message.tool_calls.length > 0) return true;
    if (message.streaming) return true;
    if (message.reasoning?.trim() && isTaskBusy(currentTask)) return true;
    const content = message.content.trim().toLowerCase();
    if (!content) return false;
    return !content.startsWith("demo mode is active") && !content.startsWith("i will inspect");
  }

  function conversationEntries(currentTask: Task | null): ConversationEntry[] {
    if (!currentTask) return [];

    const grouped: ConversationEntry[] = [];
    let pendingAssistantMessages: TaskMessage[] = [];

    const flushAssistantTurn = (): void => {
      if (pendingAssistantMessages.length === 0) return;
      grouped.push({
        id: `assistant-${pendingAssistantMessages[0].id}`,
        kind: "assistant",
        messages: pendingAssistantMessages
      });
      pendingAssistantMessages = [];
    };

    for (const message of currentTask.messages) {
      if (message.role === "user") {
        flushAssistantTurn();
        grouped.push({ id: message.id, kind: "user", message });
      } else if (message.role === "assistant" && isVisibleAssistantMessage(message, currentTask)) {
        pendingAssistantMessages = [...pendingAssistantMessages, message];
      }
    }

    flushAssistantTurn();
    return grouped;
  }
</script>

<section class="conversation" aria-live="polite">
  {#if task === null}
    <div class="empty-state">
      <h1>What can I help you with?</h1>
      <p>Start with a goal, a question, or a file you want to work on.</p>
    </div>
  {:else}
    <div class="conversation-inner">
      {#each entries as entry (entry.id)}
        {#if entry.kind === "user"}
          <MessageBubble message={entry.message} />
        {:else}
          <AssistantTurn {task} messages={entry.messages} />
        {/if}
      {/each}
      {#if task.tool_calls.length > 0 && !hasToolCallMessage && assistantMessages.length === 0}
        <div class="message-process-only"><InlineProcess {task} /></div>
      {/if}
      {#if task.status === "failed"}
        <div class="error-state">
          <CircleAlert size={17} />
          <div>
            <strong>Task failed</strong>
            <p>{task.error ?? "The agent stopped with an unknown error."}</p>
          </div>
          <button class="secondary-button" type="button" on:click={onRetry}>
            <RotateCcw size={15} />
            Retry
          </button>
        </div>
      {:else if task.status === "cancelled"}
        <div class="cancelled-state">
          <span class="status-dot status-dot-cancelled"></span>
          <span>Task cancelled. You can retry it from here.</span>
          <button class="text-button" type="button" on:click={onRetry}>Retry</button>
        </div>
      {/if}
    </div>
  {/if}
</section>
