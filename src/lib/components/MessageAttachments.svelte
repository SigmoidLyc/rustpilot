<script lang="ts">
  import { onMount } from "svelte";
  import { FileText, Image, LoaderCircle } from "lucide-svelte";
  import { getAttachmentPreview, isTauriRuntime } from "../api";
  import type { TaskAttachment } from "../types";

  export let taskId: string;
  export let attachments: TaskAttachment[] = [];

  let previews: Record<string, string> = {};
  let loading: Record<string, boolean> = {};
  let failed: Record<string, boolean> = {};

  function formatBytes(bytes: number): string {
    if (bytes < 1024) return `${bytes} B`;
    if (bytes < 1024 * 1024) return `${Math.max(1, Math.round(bytes / 1024))} KB`;
    return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
  }

  function isImage(attachment: TaskAttachment): boolean {
    return attachment.mime.startsWith("image/");
  }

  async function loadPreview(attachment: TaskAttachment): Promise<void> {
    if (!isTauriRuntime || !isImage(attachment) || previews[attachment.id] || loading[attachment.id]) return;
    loading = { ...loading, [attachment.id]: true };
    try {
      const preview = await getAttachmentPreview(taskId, attachment.id);
      previews = { ...previews, [attachment.id]: preview.data_url };
    } catch {
      failed = { ...failed, [attachment.id]: true };
    } finally {
      loading = { ...loading, [attachment.id]: false };
    }
  }

  onMount(() => {
    for (const attachment of attachments) void loadPreview(attachment);
  });
</script>

{#if attachments.length > 0}
  <div class="message-attachments">
    {#each attachments as attachment (attachment.id)}
      {#if isImage(attachment)}
        <div class="message-image-attachment">
          {#if previews[attachment.id]}
            <img src={previews[attachment.id]} alt={attachment.name} />
          {:else if loading[attachment.id]}
            <span class="message-attachment-loading" aria-label="Loading image"><LoaderCircle size={18} /></span>
          {:else}
            <span class="message-attachment-placeholder" aria-hidden="true"><Image size={20} /></span>
          {/if}
          <div class="message-attachment-caption">
            <span title={attachment.name}>{attachment.name}</span>
            <small>{formatBytes(attachment.size)}{failed[attachment.id] ? " - preview unavailable" : ""}</small>
          </div>
        </div>
      {:else}
        <div class="message-file-attachment">
          <span class="message-file-icon" aria-hidden="true"><FileText size={17} /></span>
          <span class="message-file-copy">
            <strong title={attachment.name}>{attachment.name}</strong>
            <small>{attachment.mime} | {formatBytes(attachment.size)}</small>
          </span>
        </div>
      {/if}
    {/each}
  </div>
{/if}
