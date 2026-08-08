<script lang="ts">
  import { Bot, CircleAlert, Terminal, UserRound } from "lucide-svelte";
  import { renderMarkdown } from "../markdown";
  import type { TaskMessage } from "../types";

  export let message: TaskMessage;

  const roleLabels: Record<TaskMessage["role"], string> = {
    user: "You",
    assistant: "RustPilot",
    system: "System",
    tool: "Tool result"
  };
</script>

<article class="message message-{message.role}">
  <div class="message-avatar" aria-hidden="true">
    {#if message.role === "user"}
      <UserRound size={15} />
    {:else if message.role === "assistant"}
      <Bot size={16} />
    {:else if message.role === "tool"}
      <Terminal size={15} />
    {:else}
      <CircleAlert size={15} />
    {/if}
  </div>
  <div class="message-body">
    <div class="message-meta">
      <span>{roleLabels[message.role]}</span>
      {#if message.streaming}
        <span class="streaming-label">working</span>
      {/if}
    </div>
    <div class:markdown-content={message.role === "assistant"} class="message-content">
      {#if message.role === "assistant"}
        {@html renderMarkdown(message.content)}
      {:else}
        {message.content}
      {/if}
      <span class:visible={message.streaming} class="streaming-cursor" aria-hidden="true"></span>
    </div>
  </div>
</article>
