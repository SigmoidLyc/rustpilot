import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import type {
  AttachmentInput,
  AttachmentPathInput,
  Task,
  TaskEvent,
  TaskEventPage,
  TaskSummary,
  SettingsInput,
  SettingsView
} from "./types";

export const isTauriRuntime =
  typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;

export async function invokeRust<T>(command: string, args?: Record<string, unknown>): Promise<T> {
  if (!isTauriRuntime) {
    throw new Error("RustPilot is running outside the Tauri desktop runtime.");
  }
  return invoke<T>(command, args);
}

export const listTasks = () => invokeRust<TaskSummary[]>("list_tasks");
export const listArchivedTasks = () => invokeRust<TaskSummary[]>("list_archived_tasks");
export const getTask = (taskId: string) => invokeRust<Task>("get_task", { taskId });
export const getTaskEvents = (taskId: string, after?: number) =>
  invokeRust<TaskEventPage>("get_task_events", { taskId, after });
export const createTask = (
  prompt: string,
  attachments: AttachmentInput[] = [],
  paths: AttachmentPathInput[] = []
) =>
  invokeRust<Task>("create_task", {
    prompt,
    attachmentInputs: attachments,
    attachmentPaths: paths
  });
export const continueTask = (
  taskId: string,
  prompt: string,
  attachments: AttachmentInput[] = [],
  paths: AttachmentPathInput[] = []
) =>
  invokeRust<Task>("continue_task", {
    taskId,
    prompt,
    attachmentInputs: attachments,
    attachmentPaths: paths
  });
export const getAttachmentPreview = (taskId: string, attachmentId: string) =>
  invokeRust<{ mime: string; data_url: string }>("get_attachment_preview", {
    taskId,
    attachmentId
  });
export const stopTask = (taskId: string) => invokeRust<boolean>("stop_task", { taskId });
export const retryTask = (taskId: string) => invokeRust<Task>("retry_task", { taskId });
export const archiveTask = (taskId: string) => invokeRust<TaskSummary>("archive_task", { taskId });
export const restoreTask = (taskId: string) => invokeRust<TaskSummary>("restore_task", { taskId });
export const deleteTask = (taskId: string) => invokeRust<TaskSummary>("delete_task", { taskId });
export const getSettings = () => invokeRust<SettingsView>("get_settings");
export const updateSettings = (input: SettingsInput) =>
  invokeRust<SettingsView>("update_settings", { input });
export const respondToApproval = (
  taskId: string,
  approvalId: string,
  approved: boolean
) =>
  invokeRust<boolean>("respond_to_approval", {
    taskId,
    approvalId,
    approved
  });

export interface EventHandlers {
  task_event?: (payload: TaskEvent) => void;
}

const eventNames = ["task_event"] as const;

export async function subscribeToEvents(handlers: EventHandlers): Promise<UnlistenFn[]> {
  if (!isTauriRuntime) {
    return [];
  }
  const unlisteners: UnlistenFn[] = [];
  for (const eventName of eventNames) {
    const handler = handlers[eventName] as ((payload: unknown) => void) | undefined;
    if (!handler) {
      continue;
    }
    try {
      unlisteners.push(
        await listen<unknown>(eventName, (event) => {
          handler(event.payload);
        })
      );
    } catch {
      // Older or restricted Tauri runtimes may not expose the event plugin.
      // Explicit task selection still performs an authoritative snapshot sync.
    }
  }
  return unlisteners;
}
