<script lang="ts">
  import { ArrowUp, Square } from "lucide-svelte";

  export let busy = false;
  export let disabled = false;
  export let placeholder = "Describe a task...";
  export let onSend: (prompt: string) => void;
  export let onStop: () => void;

  let prompt = "";

  function submit(): void {
    const value = prompt.trim();
    if (!value || busy || disabled) {
      return;
    }
    onSend(value);
    prompt = "";
  }

  function handleKeydown(event: KeyboardEvent): void {
    if (event.key === "Enter" && !event.shiftKey) {
      event.preventDefault();
      submit();
    }
  }
</script>

<form class="composer" on:submit|preventDefault={submit}>
  <textarea
    bind:value={prompt}
    rows="1"
    {placeholder}
    aria-label="Task prompt"
    disabled={disabled || busy}
    on:keydown={handleKeydown}
  ></textarea>
  {#if busy}
    <button
      class="stop-button"
      type="button"
      title="Stop task"
      aria-label="Stop task"
      on:click={onStop}
    >
      <Square size={16} fill="currentColor" />
    </button>
  {:else}
    <button
      class="send-button"
      type="submit"
      title="Run task"
      aria-label="Run task"
      disabled={disabled || !prompt.trim()}
    >
      <ArrowUp size={18} />
    </button>
  {/if}
</form>
