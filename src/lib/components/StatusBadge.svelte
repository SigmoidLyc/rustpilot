<script lang="ts">
  import type { AgentStatus } from "../types";

  export let status: AgentStatus;

  const labels: Record<AgentStatus, string> = {
    idle: "Idle",
    planning: "Planning",
    executing: "Executing",
    verifying: "Verifying",
    waiting_approval: "Needs approval",
    completed: "Completed",
    failed: "Failed",
    cancelled: "Cancelled"
  };
</script>

<span class="status-badge status-{status}">
  <span class="status-dot" aria-hidden="true"></span>
  {labels[status]}
</span>
