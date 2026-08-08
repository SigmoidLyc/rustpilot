<script lang="ts">
  import {
    Archive,
    ChevronDown,
    ChevronRight,
    FilePlus2,
    Folder,
    ListTodo,
    MoreHorizontal,
    RotateCcw,
    Settings2,
    Trash2
  } from "lucide-svelte";
  import type { TaskSummary } from "../types";

  export let tasks: TaskSummary[] = [];
  export let archivedTasks: TaskSummary[] = [];
  export let selectedTaskId = "";
  export let onNewTask: () => void;
  export let onSelectTask: (taskId: string) => void;
  export let onArchiveTask: (taskId: string) => void;
  export let onRestoreTask: (taskId: string) => void;
  export let onDeleteTask: (taskId: string) => void;
  export let onOpenSettings: () => void;

  let openMenuId = "";
  let archivedExpanded = false;

  function formatDate(timestamp: number): string {
    const date = new Date(timestamp);
    const today = new Date();
    if (date.toDateString() === today.toDateString()) {
      return date.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
    }
    return date.toLocaleDateString([], { month: "short", day: "numeric" });
  }

  function statusLabel(status: TaskSummary["status"]): string {
    switch (status) {
      case "planning":
        return "Planning";
      case "executing":
        return "Working";
      case "verifying":
        return "Checking";
      case "waiting_approval":
        return "Needs approval";
      case "completed":
        return "Completed";
      case "failed":
        return "Failed";
      case "cancelled":
        return "Cancelled";
      default:
        return "Ready";
    }
  }

  function closeMenu(): void {
    openMenuId = "";
  }

  function askDelete(task: TaskSummary): void {
    closeMenu();
    if (window.confirm(`Delete "${task.title}"? This cannot be undone.`)) {
      onDeleteTask(task.id);
    }
  }
</script>

<svelte:window on:click={closeMenu} />

<aside class="sidebar">
  <div class="sidebar-heading">
    <div class="section-label"><ListTodo size={15} strokeWidth={2} /> Projects</div>
    <button
      class="icon-button compact"
      type="button"
      title="New task"
      aria-label="New task"
      on:click={onNewTask}
    >
      <FilePlus2 size={16} />
    </button>
  </div>

  <button class="new-task-button" type="button" on:click={onNewTask}>
    <FilePlus2 size={16} />
    <span>New task</span>
    <kbd>N</kbd>
  </button>

  <div class="sidebar-list" aria-label="Projects">
    {#if tasks.length === 0}
      <div class="sidebar-empty">
        <span class="sidebar-empty-title">No projects yet</span>
        <span>Start with a new task.</span>
      </div>
    {:else}
      <div class="sidebar-group-label">Recent</div>
      {#each tasks as task (task.id)}
        <div class:active={task.id === selectedTaskId} class="task-row-wrap">
          <button
            class="task-row"
            type="button"
            aria-current={task.id === selectedTaskId ? "page" : undefined}
            on:click={() => onSelectTask(task.id)}
          >
            <Folder class="task-row-icon" size={16} strokeWidth={1.8} />
            <span class="task-row-main">
              <span class="task-title">{task.title}</span>
              <span class="task-meta">
                <span class={`task-status-dot status-${task.status}`} aria-hidden="true"></span>
                <span>{statusLabel(task.status)}</span>
                <span class="task-date">{formatDate(task.updated_at)}</span>
              </span>
            </span>
          </button>
          <button
            class="task-row-menu icon-button compact"
            type="button"
            title="Project actions"
            aria-label={`Actions for ${task.title}`}
            aria-expanded={openMenuId === task.id}
            on:click|stopPropagation={() => (openMenuId = openMenuId === task.id ? "" : task.id)}
          >
            <MoreHorizontal size={16} />
          </button>
          {#if openMenuId === task.id}
            <div class="task-menu" role="menu" tabindex="-1">
              <button type="button" role="menuitem" on:click={() => { closeMenu(); onArchiveTask(task.id); }}>
                <Archive size={15} />
                <span>Archive</span>
              </button>
              <button class="danger" type="button" role="menuitem" on:click={() => askDelete(task)}>
                <Trash2 size={15} />
                <span>Delete</span>
              </button>
            </div>
          {/if}
        </div>
      {/each}
    {/if}

    {#if archivedTasks.length > 0}
      <button
        class="archived-toggle"
        type="button"
        aria-expanded={archivedExpanded}
        on:click={() => (archivedExpanded = !archivedExpanded)}
      >
        {#if archivedExpanded}<ChevronDown size={14} />{:else}<ChevronRight size={14} />{/if}
        <Archive size={15} />
        <span>Archived</span>
        <span class="count-badge">{archivedTasks.length}</span>
      </button>
    {/if}

    {#if archivedExpanded}
      <div class="archived-list" aria-label="Archived projects">
        {#each archivedTasks as task (task.id)}
          <div class:selected={task.id === selectedTaskId} class="task-row-wrap archived-row">
            <button
              class="task-row"
              type="button"
              aria-current={task.id === selectedTaskId ? "page" : undefined}
              on:click={() => onSelectTask(task.id)}
            >
              <Folder class="task-row-icon" size={16} strokeWidth={1.8} />
              <span class="task-row-main">
                <span class="task-title">{task.title}</span>
                <span class="task-meta"><span>Archived</span><span class="task-date">{formatDate(task.updated_at)}</span></span>
              </span>
            </button>
            <button
              class="task-row-menu icon-button compact"
              type="button"
              title="Archived project actions"
              aria-label={`Actions for ${task.title}`}
              aria-expanded={openMenuId === task.id}
              on:click|stopPropagation={() => (openMenuId = openMenuId === task.id ? "" : task.id)}
            >
              <MoreHorizontal size={16} />
            </button>
            {#if openMenuId === task.id}
              <div class="task-menu" role="menu" tabindex="-1">
                <button type="button" role="menuitem" on:click={() => { closeMenu(); onRestoreTask(task.id); }}>
                  <RotateCcw size={15} />
                  <span>Restore</span>
                </button>
                <button class="danger" type="button" role="menuitem" on:click={() => askDelete(task)}>
                  <Trash2 size={15} />
                  <span>Delete</span>
                </button>
              </div>
            {/if}
          </div>
        {/each}
      </div>
    {/if}
  </div>

  <div class="sidebar-footer">
    <button class="sidebar-settings" type="button" on:click={onOpenSettings}>
      <Settings2 size={16} />
      <span>Settings</span>
    </button>
    <span class="sidebar-storage">Stored locally on this device</span>
  </div>
</aside>
