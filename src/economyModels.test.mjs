import assert from "node:assert/strict";
import test from "node:test";
import {
  defaultModelId,
  isAutomaticallySelectableEconomyChoice,
  isSelectableEconomyChoice,
  paidEconomyChoiceLabel,
} from "./economyModels.ts";

const eligiblePaid = {
  providerId: "openrouter-primary",
  modelId: "deepseek/deepseek-v4.1-flash",
  choiceKind: "paidEndpoint",
  contextTokens: 65536,
  inputPriceMicrosPer1m: 2730,
  outputPriceMicrosPer1m: 278000,
  catalogObservedAtMs: 1_798_000_000_000,
  catalogPricingSource: "authenticated catalog",
  catalogPricingObservedAtMs: 1_798_000_000_000,
  evaluationStatus: "passed",
  evaluationRuntimeVersion: "0.86.2",
  currentRuntimeVersion: "0.86.2",
  evaluationAtMs: 1_798_000_000_000,
  evaluationPassed: 3,
  evaluationCases: 3,
  evaluationReasonCodes: [],
  catalogPresent: true,
  catalogFresh: true,
  currentZeroPriced: false,
  currentContextTokens: 65536,
  capabilityReason: null,
  endpointId: "parasail/fp8",
  endpointCanonicalModelId: "deepseek/deepseek-v4.1-flash-20260910",
  endpointInputPriceMicrosPer1m: 300000,
  endpointOutputPriceMicrosPer1m: 1200000,
  endpointRequestFeeMicros: null,
  endpointPricingSource: "authenticated endpoint metadata",
  endpointObservedAtMs: 1_798_000_000_000,
  endpointFresh: true,
  endpointContextTokens: 65536,
  endpointMaxPromptTokens: null,
  endpointMaxCompletionTokens: 8192,
  estimatedMaxCostMicrosPerCall: 14745,
  privacyStatus: "eligible",
  privacySource: "reviewed policy",
  privacyObservedAtMs: 1_798_000_000_000,
  selectable: true,
  reasons: [],
};

test("an eligible paid endpoint is selectable only by explicit choice", () => {
  assert.equal(isSelectableEconomyChoice(eligiblePaid), true);
  assert.equal(isAutomaticallySelectableEconomyChoice(eligiblePaid), false);
  assert.equal(paidEconomyChoiceLabel(eligiblePaid), " · Paid · budget required");
});

test("failed, stale, or incomplete paid evidence is blocked", () => {
  assert.equal(isSelectableEconomyChoice({ ...eligiblePaid, evaluationStatus: "failed" }), false);
  assert.equal(isSelectableEconomyChoice({ ...eligiblePaid, evaluationStatus: "stale", endpointFresh: false }), false);
  assert.equal(isSelectableEconomyChoice({ ...eligiblePaid, evaluationPassed: 2 }), false);
  assert.equal(isSelectableEconomyChoice({ ...eligiblePaid, endpointInputPriceMicrosPer1m: null }), false);
  assert.equal(paidEconomyChoiceLabel({ ...eligiblePaid, selectable: false }), " · Paid · not eligible");
});

test("a backend-selectable paid row still needs the tested completion cap in both selectors", () => {
  const missingCap = { ...eligiblePaid, endpointMaxCompletionTokens: null };
  assert.equal(missingCap.selectable, true, "fixture models an inconsistent backend response");
  assert.equal(isSelectableEconomyChoice(missingCap), false);
  assert.equal(paidEconomyChoiceLabel(missingCap), " · Paid · not eligible");

  const testedOutputCap = { ...eligiblePaid, endpointMaxCompletionTokens: 8192 };
  assert.equal(isSelectableEconomyChoice(testedOutputCap), true);
  assert.equal(paidEconomyChoiceLabel(testedOutputCap), " · Paid · budget required");
});

test("ordinary paid models without an Economy status remain available to existing selection flows", () => {
  assert.equal(isAutomaticallySelectableEconomyChoice(undefined), true);
});

test("provider-change defaults skip paid Economy choices and preserve ordinary fallback behavior", () => {
  assert.equal(defaultModelId(
    ["deepseek/deepseek-v4.1-flash", "openai/gpt-4.1"],
    [eligiblePaid],
    () => true,
  ), "openai/gpt-4.1");
  assert.equal(defaultModelId(["deepseek/deepseek-v4.1-flash"], [eligiblePaid], () => true), null);
});
