<script lang="ts">
  import { onDestroy, onMount } from "svelte";
  import { getCurrentWebview } from "@tauri-apps/api/webview";
  import { ArrowUp, FileText, LoaderCircle, Paperclip, Square, X } from "lucide-svelte";
  import { fileMime, mimeForName, MAX_ATTACHMENTS, MAX_ATTACHMENT_BYTES } from "../attachments";
  import { isTauriRuntime } from "../api";
  import type { AttachmentPathInput } from "../types";

  export let busy = false;
  export let disabled = false;
  export let placeholder = "Describe a task...";
  export let onSend:
    (prompt: string, files: File[], paths: AttachmentPathInput[]) => Promise<boolean> | boolean;
  export let onStop: () => void;

  type PendingAttachment = {
    id: string;
    file: File | null;
    path: string | null;
    name: string;
    size: number | null;
    mime: string;
    previewUrl: string | null;
  };

  let prompt = "";
  let attachments: PendingAttachment[] = [];
  let fileInput: HTMLInputElement;
  let dragging = false;
  let submitting = false;
  let localError = "";
  let removeNativeDrop: (() => void) | undefined;

  $: canSubmit =
    !busy &&
    !disabled &&
    !submitting &&
    (Boolean(prompt.trim()) || attachments.length > 0);

  function attachmentId(file: File): string {
    return `file:${file.name}:${file.size}:${file.lastModified}`;
  }

  function nativeAttachmentId(path: string): string {
    return `path:${path.toLowerCase()}`;
  }

  function pathName(path: string): string {
    return path.replace(/\\/g, "/").split("/").filter(Boolean).at(-1) ?? "attachment";
  }

  function addFiles(incoming: File[]): void {
    localError = "";
    const existing = new Set(attachments.map((attachment) => attachment.id));
    const next = [...attachments];
    for (const file of incoming) {
      if (next.length >= MAX_ATTACHMENTS) {
        localError = `You can attach at most ${MAX_ATTACHMENTS} files.`;
        break;
      }
      if (file.size > MAX_ATTACHMENT_BYTES) {
        localError = `${file.name} is too large. The per-file limit is 25 MB.`;
        continue;
      }
      const id = attachmentId(file);
      if (existing.has(id)) continue;
      existing.add(id);
      const mime = fileMime(file);
      next.push({
        id,
        file,
        path: null,
        name: file.name,
        size: file.size,
        mime,
        previewUrl: mime.startsWith("image/") ? URL.createObjectURL(file) : null
      });
    }
    attachments = next;
  }

  function addNativePaths(paths: string[]): void {
    localError = "";
    const existing = new Set(attachments.map((attachment) => attachment.id));
    const next = [...attachments];
    for (const rawPath of paths) {
      const path = rawPath.trim();
      if (!path) continue;
      if (next.length >= MAX_ATTACHMENTS) {
        localError = `You can attach at most ${MAX_ATTACHMENTS} files.`;
        break;
      }
      const id = nativeAttachmentId(path);
      if (existing.has(id)) continue;
      const name = pathName(path);
      existing.add(id);
      next.push({
        id,
        file: null,
        path,
        name,
        size: null,
        mime: mimeForName(name),
        previewUrl: null
      });
    }
    attachments = next;
  }

  function removeAttachment(id: string): void {
    const removed = attachments.find((attachment) => attachment.id === id);
    if (removed?.previewUrl) URL.revokeObjectURL(removed.previewUrl);
    attachments = attachments.filter((attachment) => attachment.id !== id);
    localError = "";
  }

  function clearAttachments(): void {
    for (const attachment of attachments) {
      if (attachment.previewUrl) URL.revokeObjectURL(attachment.previewUrl);
    }
    attachments = [];
  }

  function openFilePicker(): void {
    if (!busy && !disabled && !submitting) fileInput?.click();
  }

  function handleFileInput(event: Event): void {
    const input = event.currentTarget as HTMLInputElement;
    addFiles(Array.from(input.files ?? []));
    input.value = "";
  }

  function handlePaste(event: ClipboardEvent): void {
    const files = Array.from(event.clipboardData?.files ?? []);
    if (files.length === 0) return;
    event.preventDefault();
    addFiles(files);
  }

  function handleDragOver(event: DragEvent): void {
    if (disabled || busy || submitting || !event.dataTransfer?.types.includes("Files")) return;
    event.preventDefault();
    dragging = true;
  }

  function handleDragLeave(event: DragEvent): void {
    if (event.currentTarget === event.target) dragging = false;
  }

  function handleDrop(event: DragEvent): void {
    if (disabled || busy || submitting) return;
    event.preventDefault();
    dragging = false;
    if (isTauriRuntime) return;
    addFiles(Array.from(event.dataTransfer?.files ?? []));
  }

  function canAcceptFiles(): boolean {
    return !disabled && !busy && !submitting;
  }

  async function submit(): Promise<void> {
    const value = prompt.trim();
    if (!canSubmit) return;
    submitting = true;
    localError = "";
    try {
      const accepted = await onSend(
        value,
        attachments.flatMap((attachment) => (attachment.file ? [attachment.file] : [])),
        attachments.flatMap((attachment) =>
          attachment.path
            ? [{ path: attachment.path, name: attachment.name, mime: attachment.mime }]
            : []
        )
      );
      if (accepted) {
        prompt = "";
        clearAttachments();
      }
    } catch (error) {
      localError = error instanceof Error ? error.message : String(error);
    } finally {
      submitting = false;
    }
  }

  function handleKeydown(event: KeyboardEvent): void {
    if (event.key === "Enter" && !event.shiftKey) {
      event.preventDefault();
      void submit();
    }
  }

  onMount(() => {
    if (!isTauriRuntime) return;
    let disposed = false;
    void getCurrentWebview()
      .onDragDropEvent((event) => {
        if (event.payload.type === "enter" || event.payload.type === "over") {
          dragging = canAcceptFiles();
          return;
        }
        dragging = false;
        if (event.payload.type === "drop" && canAcceptFiles()) {
          addNativePaths(event.payload.paths);
        }
      })
      .then((unlisten) => {
        if (disposed) unlisten();
        else removeNativeDrop = unlisten;
      })
      .catch(() => {
        // Browser drag/drop remains available if the native event bridge is unavailable.
      });
    return () => {
      disposed = true;
      removeNativeDrop?.();
      removeNativeDrop = undefined;
    };
  });

  onDestroy(clearAttachments);
</script>

<form
  class="composer"
  class:composer-dragging={dragging}
  on:submit|preventDefault={() => void submit()}
  on:dragover={handleDragOver}
  on:dragleave={handleDragLeave}
  on:drop={handleDrop}
>
  <input
    bind:this={fileInput}
    class="composer-file-input"
    type="file"
    multiple
    tabindex="-1"
    aria-hidden="true"
    on:change={handleFileInput}
  />

  {#if attachments.length > 0}
    <div class="composer-attachments" aria-label="Pending attachments">
      {#each attachments as attachment (attachment.id)}
        <div class="composer-attachment">
          {#if attachment.previewUrl}
            <img src={attachment.previewUrl} alt="" class="composer-attachment-preview" />
          {:else}
            <span class="composer-attachment-icon" aria-hidden="true"><FileText size={16} /></span>
          {/if}
          <span class="composer-attachment-copy">
            <strong title={attachment.name}>{attachment.name}</strong>
            <span>
              {attachment.size === null
                ? "Desktop file"
                : `${Math.max(1, Math.round(attachment.size / 1024))} KB`}
            </span>
          </span>
          <button
            class="composer-attachment-remove"
            type="button"
            title={`Remove ${attachment.name}`}
            aria-label={`Remove ${attachment.name}`}
            on:click={() => removeAttachment(attachment.id)}
          >
            <X size={13} />
          </button>
        </div>
      {/each}
    </div>
  {/if}

  <textarea
    bind:value={prompt}
    rows="1"
    {placeholder}
    aria-label="Task prompt"
    disabled={disabled || busy || submitting}
    on:keydown={handleKeydown}
    on:paste={handlePaste}
  ></textarea>

  <div class="composer-actions">
    <button
      class="attach-button"
      type="button"
      title="Attach files"
      aria-label="Attach files"
      disabled={disabled || busy || submitting}
      on:click={openFilePicker}
    >
      <Paperclip size={17} />
    </button>
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
    {:else if submitting}
      <button class="send-button composer-submit-busy" type="button" disabled aria-label="Uploading attachments">
        <LoaderCircle size={17} />
      </button>
    {:else}
      <button
        class="send-button"
        type="submit"
        title="Run task"
        aria-label="Run task"
        disabled={!canSubmit}
      >
        <ArrowUp size={18} />
      </button>
    {/if}
  </div>
  {#if localError}
    <div class="composer-error" role="status">{localError}</div>
  {/if}
</form>
