import { invoke, isTauri } from "@tauri-apps/api/core";

export type EconomyModelStatus = {
  providerId: string;
  modelId: string;
  choiceKind: "free" | "paidEndpoint";
  contextTokens: number | null;
  inputPriceMicrosPer1m: number | null;
  outputPriceMicrosPer1m: number | null;
  catalogObservedAtMs: number;
  catalogPricingSource: string | null;
  catalogPricingObservedAtMs: number | null;
  evaluationStatus: "notEvaluated" | "stale" | "passed" | "failed";
  evaluationRuntimeVersion: string | null;
  currentRuntimeVersion: string | null;
  evaluationAtMs: number | null;
  evaluationPassed: number;
  evaluationCases: number;
  evaluationReasonCodes: string[];
  catalogPresent: boolean;
  catalogFresh: boolean;
  currentZeroPriced: boolean;
  currentContextTokens: number | null;
  capabilityReason: string | null;
  endpointId: string | null;
  endpointCanonicalModelId: string | null;
  endpointInputPriceMicrosPer1m: number | null;
  endpointOutputPriceMicrosPer1m: number | null;
  endpointCacheReadPriceMicrosPer1m: number | null;
  endpointCacheWritePriceMicrosPer1m: number | null;
  endpointCacheWrite1hPriceMicrosPer1m: number | null;
  endpointPricingOverridesPresent: boolean;
  endpointRequestFeeMicros: number | null;
  endpointPricingSource: string | null;
  endpointObservedAtMs: number | null;
  endpointFresh: boolean;
  endpointContextTokens: number | null;
  endpointMaxPromptTokens: number | null;
  endpointMaxCompletionTokens: number | null;
  evaluatedMaxInputTokensPerCall: number | null;
  evaluatedMaxOutputTokensPerCall: number | null;
  evaluatedMaxSecondsPerTask: number | null;
  estimatedMaxCostMicrosPerCall: number | null;
  privacyStatus: "eligible" | "unknown" | "incompatible";
  privacySource: string | null;
  privacyObservedAtMs: number | null;
  selectable: boolean;
  reasons: string[];
};

export type FreeModelDailyRequests = {
  used: number;
  limit: number;
  remaining: number;
  observedAtMs: number;
};

export type EconomyStatusResponse = {
  models: EconomyModelStatus[];
  freeModelDailyRequests: FreeModelDailyRequests | null;
  catalogObservedAtMs: number;
};

/** The backend status is authoritative; prices/evaluation alone never admit a choice. */
export function isSelectableEconomyChoice(status: EconomyModelStatus | undefined): boolean {
  if (!status || !status.selectable || !status.catalogPresent || !status.catalogFresh
    || status.evaluationStatus !== "passed" || status.evaluationCases === 0
    || status.evaluationPassed !== status.evaluationCases
    || !status.evaluationRuntimeVersion
    || status.currentRuntimeVersion !== status.evaluationRuntimeVersion
    || status.privacyStatus !== "eligible") return false;
  if (status.choiceKind === "free") return status.currentZeroPriced;
  return Boolean(!status.currentZeroPriced
    && status.inputPriceMicrosPer1m !== null && status.inputPriceMicrosPer1m > 0
    && status.outputPriceMicrosPer1m !== null && status.outputPriceMicrosPer1m > 0
    && status.catalogPricingSource && status.catalogPricingObservedAtMs !== null
    && status.endpointId && status.endpointCanonicalModelId
    && status.endpointInputPriceMicrosPer1m !== null && status.endpointInputPriceMicrosPer1m > 0
    && status.endpointOutputPriceMicrosPer1m !== null && status.endpointOutputPriceMicrosPer1m > 0
    && (status.endpointRequestFeeMicros === null || status.endpointRequestFeeMicros === 0)
    && status.endpointPricingSource && status.endpointObservedAtMs !== null && status.endpointFresh
    && status.endpointContextTokens !== null
    && (status.endpointMaxPromptTokens === null || status.endpointMaxPromptTokens > 0)
    && status.endpointMaxCompletionTokens !== null
    && status.estimatedMaxCostMicrosPerCall !== null && status.estimatedMaxCostMicrosPerCall >= 0);
}

/** Paid choices require explicit selection and must never become defaults. */
export function isAutomaticallySelectableEconomyChoice(status: EconomyModelStatus | undefined): boolean {
  return status === undefined || (status.choiceKind !== "paidEndpoint" && isSelectableEconomyChoice(status));
}

export function defaultModelId(
  modelIds: string[],
  statuses: EconomyModelStatus[] | undefined,
  isModelSelectable: (modelId: string) => boolean,
): string | null {
  return modelIds.find((modelId) => {
    const status = statuses?.find((item) => item.modelId === modelId);
    return isAutomaticallySelectableEconomyChoice(status) && isModelSelectable(modelId);
  }) ?? null;
}

export function paidEconomyChoiceLabel(status: EconomyModelStatus | undefined): string {
  if (status?.choiceKind !== "paidEndpoint") return "";
  return isSelectableEconomyChoice(status) ? " · Paid · budget required" : " · Paid · not eligible";
}

export async function getEconomyModelStatuses(providerId: string): Promise<EconomyStatusResponse> {
  if (!isTauri()) throw new Error("Economy status requires the desktop runtime.");
  return invoke<EconomyStatusResponse>("get_economy_model_status", { providerId });
}

export function formatEconomyEvidenceDate(value: number | null): string {
  if (value === null || !Number.isFinite(value)) return "not recorded";
  const date = new Date(value);
  return Number.isNaN(date.getTime()) ? "not recorded" : date.toISOString().slice(0, 10);
}
