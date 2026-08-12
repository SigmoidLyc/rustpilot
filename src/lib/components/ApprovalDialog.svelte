<script lang="ts">
  import { Check, ShieldAlert, X } from "lucide-svelte";
  import type { ApprovalRequest } from "../types";

  export let request: ApprovalRequest | null = null;
  export let busy = false;
  export let onDecision: (approved: boolean, remember?: boolean) => void;
  let remember = false;

  $: if (request?.id) remember = false;
</script>

{#if request}
  <div class="modal-backdrop approval-backdrop">
    <div class="approval-dialog" role="dialog" aria-modal="true" aria-labelledby="approval-title">
      <div class="dialog-icon"><ShieldAlert size={20} /></div>
      <div class="dialog-heading">
        <span class="eyebrow">User approval required</span>
        <h2 id="approval-title">{request.tool_name}</h2>
      </div>
      <p class="dialog-reason">{request.reason}</p>
      <details class="approval-details" open>
        <summary>Tool arguments</summary>
        <pre>{request.details}</pre>
      </details>
      {#if request.rememberable}
        <label class="approval-remember">
          <input bind:checked={remember} type="checkbox" disabled={busy} />
          <span>Remember this kind of action for this workspace</span>
        </label>
      {/if}
      <div class="dialog-actions">
        <button class="secondary-button" type="button" disabled={busy} on:click={() => onDecision(false)}>
          <X size={16} />
          Decline
        </button>
        <button class="primary-button" type="button" disabled={busy} on:click={() => onDecision(true, remember)}>
          <Check size={16} />
          {busy ? "Sending..." : "Approve"}
        </button>
      </div>
    </div>
  </div>
{/if}
