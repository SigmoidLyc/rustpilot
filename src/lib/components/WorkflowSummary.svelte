<script lang="ts">
  import InlineProcess from "./InlineProcess.svelte";
  import type { Task } from "../types";

  export let task: Task | null = null;
</script>

<InlineProcess {task} />
