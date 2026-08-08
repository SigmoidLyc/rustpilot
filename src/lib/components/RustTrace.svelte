<script lang="ts">
  import { ChevronLeft, ChevronRight, ChevronDown, CircleCheck, CircleX, LoaderCircle, Minus } from "lucide-svelte";
  import type { AgentStep, Task, ToolCall, StepPhase, StepStatus } from "../types";
  import ToolCallView from "./ToolCallView.svelte";

  export let task: Task | null = null;
  export let collapsed = false;
  export let onToggle: () => void;

  const phases: StepPhase[] = ["plan", "act", "verify"];
  const phaseLabels: Record<StepPhase, string> = { plan: "Plan", act: "Act", verify: "Verify" };

  function phaseStatus(phase: StepPhase): string {
    const steps = task?.steps.filter((step) => step.phase === phase) ?? [];
    const active = steps.find((step) => step.status === "running");
    if (active) return "running";
    if (steps.some((step) => step.status === "failed")) return "failed";
    if (steps.some((step) => step.status === "completed")) return "completed";
    return "pending";
  }

  function phaseClass(phase: StepPhase): string {
    return "trace-phase-" + phaseStatus(phase);
  }

  function duration(item: AgentStep | ToolCall): string {
    if (item.duration_ms === null || item.duration_ms === undefined) return "active";
    if (item.duration_ms < 1000) return item.duration_ms + " ms";
    return (item.duration_ms / 1000).toFixed(1) + " s";
  }

  function statusIcon(status: StepStatus) {
    if (status === "completed") return CircleCheck;
    if (status === "failed" || status === "cancelled") return CircleX;
    if (status === "running") return LoaderCircle;
    return Minus;
  }

  function visiblePlanStatus(index: number, fallback: string): string {
    const phase = (['plan', 'act', 'verify'] as const)[index];
    if (!phase || !task) return fallback;
    const phaseSteps = task.steps.filter((step) => step.phase === phase);
    if (phaseSteps.some((step) => step.status === "running")) return "in_progress";
    if (phaseSteps.some((step) => step.status === "failed" || step.status === "cancelled")) return "blocked";
    if (phaseSteps.some((step) => step.status === "completed")) return "completed";
    return fallback;
  }

  function completedPlanSteps(): number {
    const plan = task?.plans[0];
    return plan?.steps.filter((step, index) => visiblePlanStatus(index, step.status) === "completed").length ?? 0;
  }

  function rawLog(): string {
    if (!task) return "";
    return task.messages
      .filter((message) => message.role === "tool" || message.role === "system")
      .map((message) => "[" + message.role + "] " + message.content)
      .join("\n\n");
  }
</script>

<aside class:collapsed class="trace-panel">
  <div class="trace-header">
    {#if !collapsed}
      <div>
        <span class="eyebrow">Live execution</span>
        <h2>Rust Trace</h2>
      </div>
    {/if}
    <button
      class="icon-button compact"
      type="button"
      title={collapsed ? "Expand Rust Trace" : "Collapse Rust Trace"}
      aria-label={collapsed ? "Expand Rust Trace" : "Collapse Rust Trace"}
      on:click={onToggle}
    >
      {#if collapsed}
        <ChevronLeft size={17} />
      {:else}
        <ChevronRight size={17} />
      {/if}
    </button>
  </div>

  {#if !collapsed}
    {#if task === null}
      <div class="trace-empty">
        <span class="trace-line"></span>
        <p>Execution details appear here once a task starts.</p>
      </div>
    {:else}
      <div class="trace-phases" aria-label="Agent phases">
        {#each phases as phase, index}
          <div class="trace-phase {phaseClass(phase)}">
            <span class="trace-phase-marker"></span>
            <span>{phaseLabels[phase]}</span>
            {#if index < phases.length - 1}<span class="trace-phase-connector"></span>{/if}
          </div>
        {/each}
      </div>

      <div class="trace-content">
        {#if task.plans.length > 0}
          <section class="trace-section trace-plan-section">
            <div class="trace-section-title">
              <strong class="trace-plan-label">Plan</strong>
              <span>{completedPlanSteps()}/{task.plans[0].steps.length}</span>
            </div>
            <div class="plan-list">
              {#each task.plans[0].steps as planStep, planIndex (planStep.id)}
                {@const planStatus = visiblePlanStatus(planIndex, planStep.status)}
                <div class="plan-step plan-{planStatus}">
                  <span class="plan-step-marker"></span>
                  <div class="plan-step-copy">
                    <strong>{planStep.title}</strong>
                    {#if planStep.notes}<p>{planStep.notes}</p>{/if}
                  </div>
                </div>
              {/each}
            </div>
          </section>
        {/if}

        <section class="trace-section">
          <div class="trace-section-title">Steps <span>{task.steps.length}</span></div>
          {#if task.steps.length === 0}
            <div class="trace-muted">Waiting for the agent.</div>
          {:else}
            <div class="step-list">
              {#each task.steps as step (step.id)}
                {@const Icon = statusIcon(step.status)}
                <div class="step-item step-{step.status}">
                  <div class="step-icon"><Icon size={15} /></div>
                  <div class="step-copy">
                    <div class="step-title-row">
                      <strong>{step.title}</strong>
                      <span>{duration(step)}</span>
                    </div>
                    {#if step.detail}<p>{step.detail}</p>{/if}
                  </div>
                </div>
              {/each}
            </div>
          {/if}
        </section>

        <section class="trace-section">
          <div class="trace-section-title">Tools <span>{task.tool_calls.length}</span></div>
          {#if task.tool_calls.length === 0}
            <div class="trace-muted">No tools called.</div>
          {:else}
            <div class="tool-list">
              {#each task.tool_calls as call (call.id)}
                <ToolCallView item={call} nested />
              {/each}
            </div>
          {/if}
        </section>

        <details class="raw-log">
          <summary>Raw log <ChevronDown class="details-chevron" size={14} /></summary>
          <pre>{rawLog() || "No raw tool output yet."}</pre>
        </details>
      </div>
    {/if}
  {/if}
</aside>
