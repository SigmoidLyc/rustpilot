<script lang="ts">
  import {
    Archive,
    ChevronDown,
    ChevronRight,
    FolderOpen,
    FilePlus2,
    Folder,
    ListTodo,
    MoreHorizontal,
    PanelLeftClose,
    RotateCcw,
    Settings2,
    Trash2
  } from "lucide-svelte";
  import { onMount } from "svelte";
  import type { PhysicalPosition } from "@tauri-apps/api/dpi";
  import { getCurrentWebview } from "@tauri-apps/api/webview";
  import { isTauriRuntime } from "../api";
  import type { ProjectSummary, TaskSummary } from "../types";

  export let tasks: TaskSummary[] = [];
  export let archivedTasks: TaskSummary[] = [];
  export let projects: ProjectSummary[] = [];
  export let recentlyClosedProjects: ProjectSummary[] = [];
  export let selectedWorkspace = "";
  export let selectedTaskId = "";
  export let onNewTask: () => void;
  export let onNewTaskInProject: (directory: string) => void;
  export let onSelectTask: (taskId: string) => void;
  export let onSelectProject: (directory: string) => void;
  export let onOpenProject: (path: string) => void;
  export let onPickProject: (kind: "file" | "folder") => void;
  export let onCloseProject: (directory: string) => void;
  export let onReopenProject: (directory: string) => void;
  export let onArchiveTask: (taskId: string) => void;
  export let onRestoreTask: (taskId: string) => void;
  export let onDeleteTask: (taskId: string) => void;
  export let onOpenSettings: () => void;

  let openMenuId = "";
  let archivedExpanded = false;
  let closedExpanded = false;
  let projectDragging = false;
  let sidebarElement: HTMLElement;
  let tasksByWorkspace = new Map<string, TaskSummary[]>();

  $: tasksByWorkspace = groupTasksByWorkspace(tasks);

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

  function groupTasksByWorkspace(items: TaskSummary[]): Map<string, TaskSummary[]> {
    const grouped = new Map<string, TaskSummary[]>();
    for (const task of items) {
      const group = grouped.get(task.workspace);
      if (group) group.push(task);
      else grouped.set(task.workspace, [task]);
    }
    return grouped;
  }

  function projectLabel(directory: string): string {
    return directory.replace(/\\/g, "/").split("/").filter(Boolean).at(-1) ?? directory;
  }

  function nativeDropIsInside(position: PhysicalPosition): boolean {
    if (!sidebarElement) return false;
    const logical = position.toLogical(window.devicePixelRatio || 1);
    const bounds = sidebarElement.getBoundingClientRect();
    return (
      logical.x >= bounds.left &&
      logical.x <= bounds.right &&
      logical.y >= bounds.top &&
      logical.y <= bounds.bottom
    );
  }

  function handleNativeDrop(paths: string[]): void {
    projectDragging = false;
    const path = paths[0]?.trim();
    if (path) onOpenProject(path);
  }

  function handleDragOver(event: DragEvent): void {
    if (!event.dataTransfer?.types.includes("Files")) return;
    event.preventDefault();
    projectDragging = true;
  }

  function handleDragLeave(event: DragEvent): void {
    if (event.currentTarget === event.target) projectDragging = false;
  }

  function handleDrop(event: DragEvent): void {
    event.preventDefault();
    projectDragging = false;
    if (isTauriRuntime) return;
    const path = Array.from(event.dataTransfer?.files ?? [])[0]?.name;
    if (path) onOpenProject(path);
  }

  onMount(() => {
    if (!isTauriRuntime) return;
    let disposed = false;
    let remove: (() => void) | undefined;
    void getCurrentWebview()
      .onDragDropEvent((event) => {
        if (event.payload.type === "enter" || event.payload.type === "over") {
          projectDragging = nativeDropIsInside(event.payload.position);
        } else if (event.payload.type === "leave") {
          projectDragging = false;
        } else if (event.payload.type === "drop") {
          const acceptingDrop = nativeDropIsInside(event.payload.position);
          projectDragging = false;
          if (acceptingDrop) handleNativeDrop(event.payload.paths);
        }
      })
      .then((unlisten) => {
        if (disposed) unlisten();
        else remove = unlisten;
      })
      .catch(() => undefined);
    return () => {
      disposed = true;
      remove?.();
    };
  });
</script>

<svelte:window on:click={closeMenu} />

<aside
  bind:this={sidebarElement}
  class="sidebar"
  class:sidebar-dragging={projectDragging}
  on:dragover={handleDragOver}
  on:dragleave={handleDragLeave}
  on:drop={handleDrop}
>
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
    <div class="project-actions">
      <button type="button" class="project-action" on:click={() => onPickProject("folder")}>
        <FolderOpen size={14} />
        <span>Open folder</span>
      </button>
      <button type="button" class="project-action" on:click={() => onPickProject("file")}>
        <FilePlus2 size={14} />
        <span>Open file</span>
      </button>
    </div>

    {#if projects.length === 0}
      <div class="sidebar-empty">
        <span class="sidebar-empty-title">No projects open</span>
        <span>Open a folder or file to begin.</span>
      </div>
    {:else}
      <div class="sidebar-group-label">Open</div>
      {#each projects as project (project.id)}
        {@const projectTaskList = tasksByWorkspace.get(project.directory) ?? []}
        <div class:selected={project.directory === selectedWorkspace} class="project-block">
          <button
            class="project-row"
            type="button"
            aria-current={project.directory === selectedWorkspace ? "page" : undefined}
            on:click={() => onSelectProject(project.directory)}
          >
            <Folder class="project-row-icon" size={16} strokeWidth={1.8} />
            <span class="project-row-main">
              <span class="project-name" title={project.directory}>{project.name || projectLabel(project.directory)}</span>
              <span class="task-meta">
                <span>{projectTaskList.length} task{projectTaskList.length === 1 ? "" : "s"}</span>
                <span class="task-date" title={project.directory}>{project.directory}</span>
              </span>
            </span>
          </button>
          <button
            class="task-row-menu icon-button compact"
            type="button"
            title="Project actions"
            aria-label={`Actions for ${project.name}`}
            aria-expanded={openMenuId === project.id}
            on:click|stopPropagation={() => (openMenuId = openMenuId === project.id ? "" : project.id)}
          >
            <MoreHorizontal size={16} />
          </button>
          {#if openMenuId === project.id}
            <div class="task-menu" role="menu" tabindex="-1">
              <button type="button" role="menuitem" on:click={() => { closeMenu(); onNewTaskInProject(project.directory); }}>
                <FilePlus2 size={15} />
                <span>New task here</span>
              </button>
              <button type="button" role="menuitem" on:click={() => { closeMenu(); onCloseProject(project.directory); }}>
                <PanelLeftClose size={15} />
                <span>Close project</span>
              </button>
            </div>
          {/if}
        </div>
        {#if project.directory === selectedWorkspace && projectTaskList.length > 0}
          <div class="project-tasks">
            {#each projectTaskList as task (task.id)}
              <div class:active={task.id === selectedTaskId} class="task-row-wrap">
                <button class="task-row" type="button" on:click={() => onSelectTask(task.id)}>
                  <span class={`task-status-dot status-${task.status}`} aria-hidden="true"></span>
                  <span class="task-row-main">
                    <span class="task-title">{task.title}</span>
                    <span class="task-meta"><span>{statusLabel(task.status)}</span><span class="task-date">{formatDate(task.updated_at)}</span></span>
                  </span>
                </button>
                <button class="task-row-menu icon-button compact" type="button" title="Task actions" aria-label={`Actions for ${task.title}`} aria-expanded={openMenuId === task.id} on:click|stopPropagation={() => (openMenuId = openMenuId === task.id ? "" : task.id)}><MoreHorizontal size={16} /></button>
                {#if openMenuId === task.id}
                  <div class="task-menu" role="menu" tabindex="-1">
                    <button type="button" role="menuitem" on:click={() => { closeMenu(); onArchiveTask(task.id); }}><Archive size={15} /><span>Archive</span></button>
                    <button class="danger" type="button" role="menuitem" on:click={() => askDelete(task)}><Trash2 size={15} /><span>Delete</span></button>
                  </div>
                {/if}
              </div>
            {/each}
          </div>
        {/if}
      {/each}
    {/if}

    {#if recentlyClosedProjects.length > 0}
      <button class="archived-toggle" type="button" aria-expanded={closedExpanded} on:click={() => (closedExpanded = !closedExpanded)}>
        {#if closedExpanded}<ChevronDown size={14} />{:else}<ChevronRight size={14} />{/if}
        <RotateCcw size={15} /><span>Recently closed</span><span class="count-badge">{recentlyClosedProjects.length}</span>
      </button>
      {#if closedExpanded}
        <div class="archived-list" aria-label="Recently closed projects">
          {#each recentlyClosedProjects as project (project.id)}
            <button class="closed-project-row" type="button" on:click={() => onReopenProject(project.directory)}>
              <Folder size={15} /><span title={project.directory}>{project.name}</span>
            </button>
          {/each}
        </div>
      {/if}
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
