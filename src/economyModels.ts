import { invoke, isTauri } from "@tauri-apps/api/core";

export type EconomyModelStatus = {
  providerId: string;
  modelId: string;
  contextTokens: number | null;
  inputPriceMicrosPer1m: number | null;
  outputPriceMicrosPer1m: number | null;
  catalogObservedAtMs: number;
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

export async function getEconomyModelStatuses(providerId: string): Promise<EconomyStatusResponse> {
  if (!isTauri()) throw new Error("Economy status requires the desktop runtime.");
  return invoke<EconomyStatusResponse>("get_economy_model_status", { providerId });
}

export function formatEconomyEvidenceDate(value: number | null): string {
  if (value === null || !Number.isFinite(value)) return "not recorded";
  const date = new Date(value);
  return Number.isNaN(date.getTime()) ? "not recorded" : date.toISOString().slice(0, 10);
}
