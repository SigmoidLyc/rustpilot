import type {
  ModelCapabilities,
  ModelVariant,
  ReasoningEffortSelection
} from "./types";

export type ReasoningEffortOption = ModelVariant;

export function reasoningEffortOptions(
  capabilities: ModelCapabilities
): readonly ReasoningEffortOption[] {
  return capabilities.variants;
}

export function reasoningEffortName(
  selection: ReasoningEffortSelection,
  options: readonly ReasoningEffortOption[]
): string {
  if (selection === "default") return "Default";
  return options.find((option) => option.id === selection)?.name ?? "Default";
}
