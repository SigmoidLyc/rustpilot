<script lang="ts">
  import { onDestroy, onMount } from "svelte";
  import type { PhysicalPosition } from "@tauri-apps/api/dpi";
  import { getCurrentWebview } from "@tauri-apps/api/webview";
  import {
    ArrowUp,
    Brain,
    Check,
    ChevronDown,
    FileText,
    LoaderCircle,
    Paperclip,
    ShieldAlert,
    ShieldCheck,
    Settings2,
    Square,
    X
  } from "lucide-svelte";
  import { fileMime, mimeForName, MAX_ATTACHMENTS, MAX_ATTACHMENT_BYTES } from "../attachments";
  import { isTauriRuntime } from "../api";
  import { reasoningEffortName, reasoningEffortOptions } from "../reasoning";
  import type {
    ApprovalMode,
    AttachmentPathInput,
    ModelCapabilities,
    ReasoningEffortSelection,
    TaskModelSelection
  } from "../types";

  export let busy = false;
  export let disabled = false;
  export let placeholder = "Describe a task...";
  export let model = "";
  export let capabilities: ModelCapabilities;
  export let reasoningEffort: ReasoningEffortSelection = "default";
  export let approvalMode: ApprovalMode = "guarded";
  export let onApprovalModeChange: (mode: ApprovalMode) => Promise<boolean> | boolean = () => true;
  export let onSend:
    (
      prompt: string,
      files: File[],
      paths: AttachmentPathInput[],
      selection: TaskModelSelection
    ) => Promise<boolean> | boolean;
  export let onModelChange: (model: string) => void = () => {};
  export let onReasoningEffortChange: (effort: ReasoningEffortSelection) => void = () => {};
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
  let approvalSaving = false;
  let approvalMenu: HTMLDetailsElement;
  let modelMenu: HTMLDetailsElement;
  let reasoningMenu: HTMLDetailsElement;
  let modelMenuPanel: HTMLDivElement;
  let reasoningMenuPanel: HTMLDivElement;
  let modelDraft = "";
  let previousModel = "";
  let removeNativeDrop: (() => void) | undefined;
  let composerElement: HTMLFormElement;

  $: if (model !== previousModel) {
    previousModel = model;
    modelDraft = model;
  }
  $: reasoningOptions = reasoningEffortOptions(capabilities);
  $: reasoningLabel = reasoningEffortName(reasoningEffort, reasoningOptions);

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

  function nativeDropIsInside(position: PhysicalPosition): boolean {
    if (!composerElement) return false;
    const logical = position.toLogical(window.devicePixelRatio || 1);
    const bounds = composerElement.getBoundingClientRect();
    return (
      logical.x >= bounds.left &&
      logical.x <= bounds.right &&
      logical.y >= bounds.top &&
      logical.y <= bounds.bottom
    );
  }

  function approvalModeName(mode: ApprovalMode): string {
    return mode === "guarded" ? "Guarded" : "Confirm";
  }

  function toggleApprovalMenu(event: MouseEvent): void {
    if (disabled || busy || submitting || approvalSaving) event.preventDefault();
  }

  function toggleSelectionMenu(event: MouseEvent): void {
    if (disabled || busy || submitting) event.preventDefault();
  }

  function closeApprovalMenu(): void {
    if (approvalMenu) approvalMenu.open = false;
  }

  function closeModelMenu(): void {
    if (modelMenu) modelMenu.open = false;
  }

  function closeReasoningMenu(): void {
    if (reasoningMenu) reasoningMenu.open = false;
  }

  function alignSelectionMenu(menu: HTMLDivElement, anchor: HTMLDetailsElement): void {
    if (!menu || !anchor?.open) return;
    const anchorRect = anchor.getBoundingClientRect();
    const menuWidth = menu.getBoundingClientRect().width;
    const viewportPadding = 14;
    const minLeft = viewportPadding;
    const maxLeft = Math.max(minLeft, window.innerWidth - viewportPadding - menuWidth);
    const preferredLeft = anchorRect.right - menuWidth;
    const left = Math.min(Math.max(preferredLeft, minLeft), maxLeft);
    menu.style.left = `${left - anchorRect.left}px`;
    menu.style.right = "auto";
  }

  function scheduleSelectionMenuAlignment(menu: HTMLDivElement, anchor: HTMLDetailsElement): void {
    requestAnimationFrame(() => alignSelectionMenu(menu, anchor));
  }

  function handleWindowResize(): void {
    if (modelMenu?.open) alignSelectionMenu(modelMenuPanel, modelMenu);
    if (reasoningMenu?.open) alignSelectionMenu(reasoningMenuPanel, reasoningMenu);
  }

  function handleWindowClick(event: MouseEvent): void {
    if (approvalMenu?.open && !approvalMenu.contains(event.target as Node)) closeApprovalMenu();
    if (modelMenu?.open && !modelMenu.contains(event.target as Node)) closeModelMenu();
    if (reasoningMenu?.open && !reasoningMenu.contains(event.target as Node)) closeReasoningMenu();
  }

  function handleWindowKeydown(event: KeyboardEvent): void {
    if (event.key === "Escape" && approvalMenu?.open) {
      closeApprovalMenu();
      event.stopPropagation();
    } else if (event.key === "Escape" && modelMenu?.open) {
      closeModelMenu();
      event.stopPropagation();
    } else if (event.key === "Escape" && reasoningMenu?.open) {
      closeReasoningMenu();
      event.stopPropagation();
    }
  }

  function applyModel(): void {
    const next = modelDraft.trim();
    if (!next) {
      localError = "Enter a model name before applying it.";
      return;
    }
    localError = "";
    onModelChange(next);
    closeModelMenu();
  }

  function selectReasoningEffort(value: ReasoningEffortSelection): void {
    if (disabled || busy || submitting) return;
    localError = "";
    onReasoningEffortChange(value);
    closeReasoningMenu();
  }

  async function selectApprovalMode(mode: ApprovalMode): Promise<void> {
    if (approvalSaving || disabled || busy || submitting) return;
    if (mode === approvalMode) {
      closeApprovalMenu();
      return;
    }
    approvalSaving = true;
    localError = "";
    try {
      const accepted = await onApprovalModeChange(mode);
      if (accepted !== false) closeApprovalMenu();
    } catch (error) {
      localError = error instanceof Error ? error.message : String(error);
    } finally {
      approvalSaving = false;
    }
  }

  async function submit(): Promise<void> {
    const value = prompt.trim();
    if (!canSubmit) return;
    if (!model.trim()) {
      localError = "Select a model before sending.";
      return;
    }
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
        ),
        { model: model.trim(), reasoning_effort: reasoningEffort }
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
          dragging = canAcceptFiles() && nativeDropIsInside(event.payload.position);
          return;
        }
        dragging = false;
        if (event.payload.type === "drop" && canAcceptFiles() && nativeDropIsInside(event.payload.position)) {
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

<svelte:window
  on:click={handleWindowClick}
  on:keydown={handleWindowKeydown}
  on:resize={handleWindowResize}
/>

<form
  bind:this={composerElement}
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

  <div class="composer-toolbar">
    <div class="composer-left-actions">
      <details class="composer-approval" bind:this={approvalMenu}>
        <summary
          class="composer-approval-trigger"
          class:composer-approval-disabled={disabled || busy || submitting || approvalSaving}
          title={`Approval mode: ${approvalModeName(approvalMode)}`}
          aria-label={`Approval mode: ${approvalModeName(approvalMode)}`}
          on:click={toggleApprovalMenu}
        >
          {#if approvalMode === "guarded"}
            <ShieldCheck size={16} />
          {:else}
            <ShieldAlert size={16} />
          {/if}
          <span class="composer-approval-label">{approvalModeName(approvalMode)}</span>
          <ChevronDown size={14} class="composer-approval-chevron" />
        </summary>
        <div class="composer-approval-menu">
          <div class="composer-approval-heading">
            <strong>Permission mode</strong>
            <span>Choose how much the agent pauses before sensitive work.</span>
          </div>
          <button
            class:composer-approval-option-active={approvalMode === "guarded"}
            class="composer-approval-option"
            type="button"
            disabled={approvalSaving}
            on:click={() => void selectApprovalMode("guarded")}
          >
            <span class="composer-approval-option-icon"><ShieldCheck size={15} /></span>
            <span class="composer-approval-option-copy">
              <strong>Guarded</strong>
              <small>Routine workspace work keeps moving; risky actions pause.</small>
            </span>
            {#if approvalMode === "guarded"}<Check size={15} />{/if}
          </button>
          <button
            class:composer-approval-option-active={approvalMode === "confirm"}
            class="composer-approval-option"
            type="button"
            disabled={approvalSaving}
            on:click={() => void selectApprovalMode("confirm")}
          >
            <span class="composer-approval-option-icon composer-approval-option-icon-alert"><ShieldAlert size={15} /></span>
            <span class="composer-approval-option-copy">
              <strong>Confirm</strong>
              <small>Ask before every action that can change state or reach outside.</small>
            </span>
            {#if approvalMode === "confirm"}<Check size={15} />{/if}
          </button>
        </div>
      </details>
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
    </div>
    <div class="composer-actions">
      <div class="composer-selection-actions">
        <details
          class="composer-model"
          bind:this={modelMenu}
          on:toggle={() => scheduleSelectionMenuAlignment(modelMenuPanel, modelMenu)}
        >
          <summary
            class="composer-selection-trigger composer-model-trigger"
            class:composer-selection-disabled={disabled || busy || submitting}
            title={`Model: ${model || "Not selected"}`}
            aria-label={`Model: ${model || "Not selected"}`}
            on:click={toggleSelectionMenu}
          >
            <Settings2 size={14} />
            <span class="composer-model-name">{model || "Model"}</span>
            <ChevronDown size={13} class="composer-selection-chevron" />
          </summary>
          <div bind:this={modelMenuPanel} class="composer-selection-menu composer-model-menu">
            <div class="composer-selection-heading">
              <strong>Model</strong>
            </div>
            <label class="composer-model-field">
              <span>Model name</span>
              <input
                bind:value={modelDraft}
                type="text"
                spellcheck="false"
                autocomplete="off"
                disabled={disabled || busy || submitting}
                on:keydown={(event) => {
                  if (event.key === "Enter") {
                    event.preventDefault();
                    applyModel();
                  }
                }}
              />
            </label>
            <button
              class="composer-selection-apply"
              type="button"
              disabled={disabled || busy || submitting || !modelDraft.trim()}
              on:click={applyModel}
            >
              <Check size={14} />
              Apply model
            </button>
          </div>
        </details>

        {#if reasoningOptions.length > 0}
          <details
            class="composer-reasoning"
            bind:this={reasoningMenu}
            on:toggle={() => scheduleSelectionMenuAlignment(reasoningMenuPanel, reasoningMenu)}
          >
            <summary
              class="composer-selection-trigger"
              class:composer-selection-disabled={disabled || busy || submitting}
              title={`Reasoning effort: ${reasoningLabel}`}
              aria-label={`Reasoning effort: ${reasoningLabel}`}
              on:click={toggleSelectionMenu}
            >
              <Brain size={15} />
              <span class="composer-reasoning-label">{reasoningLabel}</span>
              <ChevronDown size={13} class="composer-selection-chevron" />
            </summary>
            <div bind:this={reasoningMenuPanel} class="composer-selection-menu composer-reasoning-menu">
              <div class="composer-selection-heading">
                <strong>Reasoning effort</strong>
              </div>
              <button
                class:composer-selection-option-active={reasoningEffort === "default"}
                class="composer-selection-option"
                type="button"
                disabled={disabled || busy || submitting}
                on:click={() => selectReasoningEffort("default")}
              >
                <span class="composer-selection-option-copy">
                  <strong>Default</strong>
                </span>
                {#if reasoningEffort === "default"}<Check size={15} />{/if}
              </button>
              {#each reasoningOptions as option (option.id)}
                <button
                  class:composer-selection-option-active={reasoningEffort === option.id}
                  class="composer-selection-option"
                  type="button"
                  disabled={disabled || busy || submitting}
                  on:click={() => selectReasoningEffort(option.id)}
                >
                  <span class="composer-selection-option-copy">
                    <strong>{option.name}</strong>
                  </span>
                  {#if reasoningEffort === option.id}<Check size={15} />{/if}
                </button>
              {/each}
            </div>
          </details>
        {/if}
      </div>
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
  </div>
  {#if localError}
    <div class="composer-error" role="status">{localError}</div>
  {/if}
</form>
