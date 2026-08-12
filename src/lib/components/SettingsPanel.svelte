<script lang="ts">
  import { KeyRound, Save, Settings2, Trash2, X } from "lucide-svelte";
  import type { PromptCacheMode, SettingsInput, SettingsView } from "../types";

  export let visible = false;
  export let settings: SettingsView;
  export let saving = false;
  export let onClose: () => void;
  export let onSave: (input: SettingsInput) => void;

  let apiBaseUrl = "";
  let model = "";
  let apiKey = "";
  let clearStoredKey = false;
  let maxSteps = 100;
  let timeoutSecs = 45;
  let promptCache: PromptCacheMode = "auto";
  let clearApprovalRules = false;

  $: if (settings) {
    apiBaseUrl = settings.api_base_url;
    model = settings.model;
    maxSteps = settings.max_steps;
    timeoutSecs = settings.timeout_secs;
    promptCache = settings.prompt_cache;
    clearApprovalRules = false;
    clearStoredKey = false;
  }

  function submit(): void {
    onSave({
      api_base_url: apiBaseUrl,
      model,
      api_key: clearStoredKey ? "" : apiKey.trim() ? apiKey.trim() : null,
      max_steps: Number(maxSteps),
      timeout_secs: Number(timeoutSecs),
      prompt_cache: promptCache,
      approval_mode: settings.approval_mode,
      clear_approval_rules: clearApprovalRules
    });
  }
</script>

{#if visible}
  <div class="modal-backdrop">
    <div class="settings-panel" role="dialog" aria-modal="true" aria-labelledby="settings-title">
      <div class="panel-heading">
        <div class="dialog-heading">
          <div class="panel-title-icon"><Settings2 size={18} /></div>
          <div>
            <span class="eyebrow">Runtime configuration</span>
            <h2 id="settings-title">LLM settings</h2>
          </div>
        </div>
        <button class="icon-button" type="button" title="Close settings" aria-label="Close settings" on:click={onClose}>
          <X size={18} />
        </button>
      </div>

      <form on:submit|preventDefault={submit}>
        <label>
          <span>API base URL</span>
          <input bind:value={apiBaseUrl} type="url" placeholder="https://api.openai.com/v1" />
        </label>
        <label>
          <span>Model</span>
          <input bind:value={model} type="text" placeholder="gpt-4o-mini" />
        </label>
        <label>
          <span class="label-with-icon"><KeyRound size={14} /> API key</span>
          <input bind:value={apiKey} on:input={() => (clearStoredKey = false)} type="password" placeholder={settings.api_key_configured ? "Key is configured in memory" : "Required to run tasks"} autocomplete="off" />
        </label>
        {#if settings.api_key_configured && !apiKey && !clearStoredKey}
          <button class="clear-key-button" type="button" on:click={() => (clearStoredKey = true)}>
            <Trash2 size={14} />
            Clear in-memory key
          </button>
        {/if}
        <p class="field-note">The key stays in memory for this app session and is never written to task history.</p>

        <div class="settings-grid">
          <label>
            <span>Max agent steps</span>
            <input bind:value={maxSteps} type="number" min="1" step="1" />
          </label>
          <label>
            <span>Timeout (seconds)</span>
            <input bind:value={timeoutSecs} type="number" min="5" max="120" />
          </label>
        </div>

        <label>
          <span>Prompt cache</span>
          <select bind:value={promptCache}>
            <option value="auto">Auto</option>
            <option value="enabled">Enabled</option>
            <option value="disabled">Disabled</option>
          </select>
        </label>

        {#if settings.remembered_approvals > 0}
          <label class="settings-checkline">
            <input bind:checked={clearApprovalRules} type="checkbox" />
            <span>Clear {settings.remembered_approvals} remembered approval rule{settings.remembered_approvals === 1 ? "" : "s"}</span>
          </label>
        {/if}

        <div class="settings-mode">
          <span class:mode-live={!settings.demo_mode} class="mode-indicator"></span>
          <div>
            <strong>{settings.demo_mode ? "API key required" : "OpenAI-compatible mode"}</strong>
            <p>{settings.demo_mode ? "Configure an API key before sending a task." : "New tasks will use the configured endpoint."}</p>
          </div>
        </div>

        <div class="dialog-actions">
          <button class="secondary-button" type="button" disabled={saving} on:click={onClose}>Cancel</button>
          <button class="primary-button" type="submit" disabled={saving}>
            <Save size={16} />
            {saving ? "Saving..." : "Save settings"}
          </button>
        </div>
      </form>
    </div>
  </div>
{/if}
