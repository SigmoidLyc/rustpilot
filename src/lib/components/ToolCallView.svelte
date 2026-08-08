<script lang="ts">
  import {
    ChevronDown,
    CircleCheck,
    CircleX,
    Clock3,
    Database,
    Eye,
    FileText,
    FolderOpen,
    Globe,
    ListChecks,
    LoaderCircle,
    MessageCircle,
    Minus,
    Monitor,
    Search,
    Terminal,
    Trash2,
    Wrench
  } from "lucide-svelte";
  import type { ToolCallStatus, ToolPresentation } from "../types";

  type ToolMode = "text" | "code" | "terminal" | "diff" | "json";
  type ToolKind =
    | "clock"
    | "files"
    | "editor"
    | "shell"
    | "http"
    | "web"
    | "browser"
    | "vision"
    | "python"
    | "plan"
    | "data"
    | "mcp"
    | "question"
    | "finish"
    | "generic";

  type ToolDescriptor = {
    kind: ToolKind;
    icon: any;
    title: string;
    subtitle: string | null;
    args: string[];
    mode: ToolMode;
    detail: string | null;
    detailLabel: string;
    showArguments: boolean;
  };

  export let item: ToolPresentation;
  export let defaultOpen = false;
  export let nested = false;

  const MAX_PREVIEW_CHARS = 12000;
  let expanded = defaultOpen;

  function record(value: unknown): Record<string, unknown> {
    return value !== null && typeof value === "object" && !Array.isArray(value)
      ? (value as Record<string, unknown>)
      : {};
  }

  function stringValue(input: Record<string, unknown>, key: string): string {
    const value = input[key];
    return typeof value === "string" ? value : "";
  }

  function compactPath(value: string): string {
    if (!value) return "";
    const parts = value.replace(/\\/g, "/").split("/").filter(Boolean);
    if (parts.length <= 2) return value;
    return `.../${parts.slice(-2).join("/")}`;
  }

  function firstLine(value: string): string {
    return (value.replace(/\r\n?/g, "\n").split("\n")[0]?.trim() ?? "").slice(0, 240);
  }

  function preview(value: string): string {
    if (value.length <= MAX_PREVIEW_CHARS) return value;
    return `${value.slice(0, MAX_PREVIEW_CHARS)}\n[preview truncated]`;
  }

  function formatArguments(value: unknown): string {
    try {
      return JSON.stringify(value, null, 2) ?? "{}";
    } catch {
      return String(value);
    }
  }

  function statusLabel(status: ToolCallStatus): string {
    switch (status) {
      case "pending":
        return "Queued";
      case "running":
        return "Working";
      case "completed":
        return "Done";
      case "failed":
        return "Blocked";
      case "cancelled":
        return "Cancelled";
    }
  }

  function durationLabel(call: ToolPresentation): string {
    if (call.status === "pending") return "queued";
    if (call.duration_ms === null) return call.status === "running" ? "active" : "";
    if (call.duration_ms < 1000) return `${call.duration_ms} ms`;
    return `${(call.duration_ms / 1000).toFixed(1)} s`;
  }

  function isRunning(call: ToolPresentation): boolean {
    return call.status === "pending" || call.status === "running";
  }

  function handleToggle(event: Event): void {
    expanded = (event.currentTarget as HTMLDetailsElement).open;
  }

  function shellDetail(command: string, result: string | null): string {
    const output = result ? `\n\n${result}` : "";
    return preview(`$ ${command}${output}`);
  }

  function describe(call: ToolPresentation): ToolDescriptor {
    const input = record(call.arguments);
    const name = call.name.toLowerCase();
    const operation = stringValue(input, "operation").toLowerCase();
    const command = stringValue(input, "command").toLowerCase();
    const path = stringValue(input, "path") || stringValue(input, "filePath");
    const result = preview(call.result ?? "");

    if (name === "rust_clock") {
      return {
        kind: "clock",
        icon: Clock3,
        title: "Read local time",
        subtitle: null,
        args: [],
        mode: "text",
        detail: result || null,
        detailLabel: "Result",
        showArguments: false
      };
    }

    if (name === "rust_files" || name === "rust_sandbox_files") {
      const labels: Record<string, string> = {
        list: "List files",
        read: "Read file",
        write: "Write file",
        delete: "Delete file",
        exists: "Check file"
      };
      const kind = operation === "write" || operation === "delete" ? "editor" : "files";
      const detail = operation === "write" && stringValue(input, "content")
        ? preview(stringValue(input, "content"))
        : result || null;
      return {
        kind,
        icon: operation === "delete" ? Trash2 : operation === "list" ? FolderOpen : FileText,
        title: labels[operation] ?? "File operation",
        subtitle: compactPath(path) || null,
        args: [],
        mode: operation === "list" ? "text" : operation === "write" ? "code" : "text",
        detail,
        detailLabel: operation === "write" ? "File content" : "Result",
        showArguments: !detail && operation !== "exists"
      };
    }

    if (name === "rust_str_replace_editor") {
      const labels: Record<string, string> = {
        view: "View file",
        create: "Create file",
        str_replace: "Edit file",
        insert: "Insert text",
        undo_edit: "Undo edit"
      };
      const oldText = stringValue(input, "old_str");
      const newText = stringValue(input, "new_str");
      const diff = oldText || newText ? `- ${oldText}\n+ ${newText}` : "";
      const range = input.view_range;
      const rangeLabel = Array.isArray(range) && range.length > 0 ? `lines ${range.join("-")}` : "";
      return {
        kind: "editor",
        icon: Eye,
        title: labels[command] ?? "Edit file",
        subtitle: compactPath(path) || null,
        args: rangeLabel ? [rangeLabel] : [],
        mode: command === "str_replace" || command === "insert" ? "diff" : "code",
        detail: preview(diff || stringValue(input, "file_text") || result) || null,
        detailLabel: diff ? "Change" : command === "view" ? "File preview" : "Result",
        showArguments: !diff && !stringValue(input, "file_text") && !result
      };
    }

    if (name === "rust_shell" || name === "rust_bash" || name === "rust_sandbox_shell") {
      const shellCommand = stringValue(input, "command");
      return {
        kind: "shell",
        icon: Terminal,
        title: name === "rust_bash" || name === "rust_sandbox_shell" ? "Bash session" : "Shell",
        subtitle: firstLine(shellCommand) || null,
        args: [],
        mode: "terminal",
        detail: shellDetail(shellCommand, result),
        detailLabel: "Output",
        showArguments: !shellCommand
      };
    }

    if (name === "rust_http") {
      const method = stringValue(input, "method") || "GET";
      return {
        kind: "http",
        icon: Globe,
        title: `${method.toUpperCase()} request`,
        subtitle: stringValue(input, "url") || null,
        args: [],
        mode: "text",
        detail: result || null,
        detailLabel: "Response",
        showArguments: !result
      };
    }

    if (name === "rust_web_search") {
      return {
        kind: "web",
        icon: Search,
        title: "Web search",
        subtitle: stringValue(input, "query") || null,
        args: [],
        mode: "text",
        detail: result || null,
        detailLabel: "Results",
        showArguments: !result
      };
    }

    if (name === "rust_crawl4ai") {
      const urls = input.urls;
      const subtitle = typeof urls === "string" ? urls : Array.isArray(urls) ? `${urls.length} URLs` : null;
      return {
        kind: "web",
        icon: Globe,
        title: "Crawl web page",
        subtitle,
        args: [],
        mode: "text",
        detail: result || null,
        detailLabel: "Output",
        showArguments: !result
      };
    }

    if (name === "rust_browser_use" || name === "rust_sandbox_browser") {
      return {
        kind: "browser",
        icon: Monitor,
        title: "Browser action",
        subtitle: stringValue(input, "action") || null,
        args: [],
        mode: "text",
        detail: result || null,
        detailLabel: "Result",
        showArguments: !result
      };
    }

    if (name === "rust_computer_use") {
      return {
        kind: "browser",
        icon: Monitor,
        title: "Computer action",
        subtitle: stringValue(input, "action") || null,
        args: [],
        mode: "text",
        detail: result || null,
        detailLabel: "Result",
        showArguments: !result
      };
    }

    if (name === "rust_sandbox_vision") {
      return {
        kind: "vision",
        icon: Eye,
        title: "Inspect image",
        subtitle: compactPath(path) || null,
        args: [],
        mode: "text",
        detail: result || null,
        detailLabel: "Image metadata",
        showArguments: !result
      };
    }

    if (name === "rust_python_execute") {
      const code = stringValue(input, "code");
      return {
        kind: "python",
        icon: Database,
        title: "Run Python",
        subtitle: firstLine(code) || null,
        args: [],
        mode: "terminal",
        detail: shellDetail("python", result),
        detailLabel: "Output",
        showArguments: !code && !result
      };
    }

    if (name === "rust_planning") {
      return {
        kind: "plan",
        icon: ListChecks,
        title: "Update plan",
        subtitle: stringValue(input, "command") || null,
        args: stringValue(input, "title") ? [stringValue(input, "title")] : [],
        mode: "text",
        detail: result || null,
        detailLabel: "Plan",
        showArguments: !result
      };
    }

    if (name === "rust_create_chat_completion") {
      const messages = input.messages;
      return {
        kind: "generic",
        icon: MessageCircle,
        title: "Structured completion",
        subtitle: Array.isArray(messages) ? `${messages.length} messages` : null,
        args: [],
        mode: "json",
        detail: result || null,
        detailLabel: "Completion",
        showArguments: !result
      };
    }

    if (name === "rust_visualization_preparation") {
      const chartKind = stringValue(input, "kind");
      const chartTitle = stringValue(input, "title");
      return {
        kind: "data",
        icon: Database,
        title: "Prepare chart spec",
        subtitle: [chartKind, chartTitle].filter(Boolean).join(" / ") || null,
        args: [],
        mode: "json",
        detail: result || null,
        detailLabel: "Chart specification",
        showArguments: !result
      };
    }

    if (name === "rust_data_analysis" || name === "rust_data_visualization") {
      return {
        kind: "data",
        icon: Database,
        title: name === "rust_data_analysis" ? "Analyze data" : "Create visualization",
        subtitle: compactPath(path || stringValue(input, "json_path")) || null,
        args: [],
        mode: "text",
        detail: result || null,
        detailLabel: "Result",
        showArguments: !result
      };
    }

    if (name === "rust_ask_human") {
      return {
        kind: "question",
        icon: MessageCircle,
        title: "Waiting for a decision",
        subtitle: stringValue(input, "question") || null,
        args: [],
        mode: "text",
        detail: result || null,
        detailLabel: "Response",
        showArguments: !result
      };
    }

    if (name === "rust_terminate") {
      return {
        kind: "finish",
        icon: Wrench,
        title: "Finish task",
        subtitle: stringValue(input, "status") || null,
        args: [],
        mode: "text",
        detail: preview(stringValue(input, "message") || result) || null,
        detailLabel: "Message",
        showArguments: false
      };
    }

    if (name === "rust_mcp" || name.startsWith("rust_mcp_")) {
      const target = stringValue(input, "tool_name") || stringValue(input, "server_id");
      return {
        kind: "mcp",
        icon: Wrench,
        title: name.startsWith("rust_mcp_") ? "MCP tool" : "MCP operation",
        subtitle: target || stringValue(input, "action") || null,
        args: [],
        mode: "json",
        detail: result || null,
        detailLabel: "Result",
        showArguments: !result
      };
    }

    const fallback =
      stringValue(input, "command") ||
      stringValue(input, "path") ||
      stringValue(input, "url") ||
      stringValue(input, "action") ||
      stringValue(input, "name");
    return {
      kind: "generic",
      icon: Wrench,
      title: call.name || "Tool call",
      subtitle: fallback ? firstLine(fallback) : null,
      args: [],
      mode: "json",
      detail: result || null,
      detailLabel: "Result",
      showArguments: true
    };
  }

  $: descriptor = describe(item);
  $: running = isRunning(item);
  $: duration = durationLabel(item);
  $: hasDetail = Boolean(descriptor.detail || descriptor.showArguments || item.error);
  $: statusIcon = item.status === "running" ? LoaderCircle : item.status === "completed" ? CircleCheck : item.status === "failed" || item.status === "cancelled" ? CircleX : Minus;
</script>

{#if hasDetail}
  <details
    class:assistant-tool-card-live={running}
    class:assistant-tool-card-nested={nested}
    class:assistant-tool-card-completed={item.status === "completed"}
    class:assistant-tool-card-failed={item.status === "failed"}
    class:assistant-tool-card-cancelled={item.status === "cancelled"}
    class="assistant-tool-card assistant-tool-card-{descriptor.kind}"
    on:toggle={handleToggle}
    open={defaultOpen}
  >
    <summary>
      <span class="assistant-tool-icon" aria-hidden="true"><svelte:component this={descriptor.icon} size={15} /></span>
      <span class="assistant-tool-copy">
        <strong class="assistant-tool-title">{descriptor.title}</strong>
        {#if descriptor.subtitle}<span class="assistant-tool-subtitle">{descriptor.subtitle}</span>{/if}
        {#each descriptor.args as arg}<span class="assistant-tool-arg">{arg}</span>{/each}
      </span>
      <span class="assistant-tool-status"><svelte:component this={statusIcon} class={running ? "assistant-tool-spinner" : ""} size={13} />{statusLabel(item.status)}</span>
      {#if duration}<span class="assistant-tool-duration">{duration}</span>{/if}
      <ChevronDown class="assistant-tool-chevron" size={14} aria-hidden="true" />
    </summary>
    {#if expanded}
      <div class="assistant-tool-body">
        {#if descriptor.detail}
          <span class="assistant-tool-label">{descriptor.detailLabel}</span>
          <pre class:assistant-tool-terminal={descriptor.mode === "terminal"} class:assistant-tool-diff={descriptor.mode === "diff"} class:assistant-tool-code={descriptor.mode === "code"}>{descriptor.detail}</pre>
        {/if}
        {#if descriptor.showArguments}
          <span class="assistant-tool-label">Arguments</span>
          <pre class="assistant-tool-json">{preview(formatArguments(item.arguments))}</pre>
        {/if}
        {#if item.error}
          <span class="assistant-tool-label assistant-tool-label-error">Error</span>
          <pre class="assistant-tool-error">{preview(item.error)}</pre>
        {/if}
      </div>
    {/if}
  </details>
{:else}
  <div
    class:assistant-tool-card-live={running}
    class:assistant-tool-card-nested={nested}
    class:assistant-tool-card-completed={item.status === "completed"}
    class:assistant-tool-card-failed={item.status === "failed"}
    class:assistant-tool-card-cancelled={item.status === "cancelled"}
    class="assistant-tool-card assistant-tool-card-static assistant-tool-card-{descriptor.kind}"
  >
    <span class="assistant-tool-icon" aria-hidden="true"><svelte:component this={descriptor.icon} size={15} /></span>
    <span class="assistant-tool-copy">
      <strong class="assistant-tool-title">{descriptor.title}</strong>
      {#if descriptor.subtitle}<span class="assistant-tool-subtitle">{descriptor.subtitle}</span>{/if}
    </span>
    <span class="assistant-tool-status"><svelte:component this={statusIcon} class={running ? "assistant-tool-spinner" : ""} size={13} />{statusLabel(item.status)}</span>
    {#if duration}<span class="assistant-tool-duration">{duration}</span>{/if}
  </div>
{/if}
