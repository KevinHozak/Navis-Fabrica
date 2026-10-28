use crate::usage::{UsageCategory, UsageSignal, UsageStatus};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

const SIMPLE_CONTEXT_TOKENS: u64 = 8_192;
const STANDARD_CONTEXT_TOKENS: u64 = 16_384;
const COMPLEX_CONTEXT_TOKENS: u64 = 32_768;
const MAX_LATENCY_AGE_MS: u128 = 24 * 60 * 60 * 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Complexity {
    Simple,
    Standard,
    Complex,
}

impl Complexity {
    pub fn context_floor(self) -> u64 {
        match self {
            Self::Simple => SIMPLE_CONTEXT_TOKENS,
            Self::Standard => STANDARD_CONTEXT_TOKENS,
            Self::Complex => COMPLEX_CONTEXT_TOKENS,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Pace {
    Fast,
    Balanced,
    Thorough,
    Economy,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Candidate {
    pub provider_id: String,
    pub telemetry_provider_id: String,
    pub provider_name: String,
    pub model_id: String,
    pub model_name: String,
    pub is_local: bool,
    pub context_tokens: Option<u64>,
    pub supports_tools: Option<bool>,
    pub reasoning_efforts: Vec<String>,
    pub quality_rating: Option<u8>,
    pub latency_ms: Option<u128>,
    pub latency_collected_at_ms: Option<u128>,
    /// Backend-only proof that dated coding-trial and privacy evidence passed.
    /// Never accepted from a request or settings file.
    #[serde(skip)]
    pub economy_evidence_ready: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Request {
    pub complexity: Complexity,
    pub pace: Pace,
    #[serde(default = "default_require_context_evidence")]
    pub require_context_evidence: bool,
    #[serde(default)]
    pub minimum_context_tokens: Option<u64>,
    pub require_tools: bool,
    pub local_only: bool,
    pub desired_completion_minutes: Option<u32>,
    pub manual_provider_id: Option<String>,
    pub manual_model_id: Option<String>,
    pub candidates: Vec<Candidate>,
    pub usage_signals: Vec<UsageSignal>,
    pub now_ms: u128,
}

fn default_require_context_evidence() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CandidateDecision {
    pub provider_id: String,
    pub provider_name: String,
    pub model_id: String,
    pub model_name: String,
    pub eligible: bool,
    pub selected: bool,
    pub reasons: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Decision {
    pub selected_provider_id: Option<String>,
    pub selected_model_id: Option<String>,
    pub required_context_tokens: u64,
    pub desired_completion_note: String,
    pub candidates: Vec<CandidateDecision>,
}

/// Capability and privacy facts that a provider adapter must refresh before a job
/// is admitted. Missing facts fail closed; they are never inferred from a model
/// name or a previously successful run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CandidateFacts {
    pub provider_id: String,
    pub model_id: String,
    pub runtime_id: String,
    pub runtime_installed: bool,
    pub authenticated: bool,
    pub healthy: bool,
    pub health_checked_at_ms: Option<u128>,
    pub model_available: bool,
    pub local_only: bool,
    /// Paid candidates require verified per-token pricing and a maximum output
    /// token policy before they can be selected for a job.
    pub billing: BillingPolicy,
    pub input_micros_per_million_tokens: Option<u64>,
    pub output_micros_per_million_tokens: Option<u64>,
    pub pricing_source: Option<String>,
    pub pricing_verified_at_ms: Option<u128>,
    /// Dated coding trial and endpoint privacy evidence are required before a
    /// zero-price remote route may be considered for Economy.
    #[serde(skip)]
    pub economy_policy: Option<crate::economy::EconomyModelPolicy>,
    /// Opaque core-minted proof; client JSON cannot supply or persist it.
    #[serde(skip)]
    pub economy_admission: Option<crate::economy::VerifiedEconomyAdmission>,
    /// Explicit-refresh, backend-validated endpoint quote. It is an
    /// observation, not caller authority, and cannot be supplied in JSON.
    #[serde(skip)]
    pub paid_endpoint_quote: Option<PaidEndpointQuoteObservation>,
    /// Context and tool facts come from refreshed backend observations, not
    /// the configured model label. None is unknown and fails capability checks.
    #[serde(default)]
    pub context_tokens: Option<u64>,
    #[serde(default)]
    pub supports_tools: Option<bool>,
    /// Snapshot provenance is validated separately. These fields let one stale
    /// candidate be rejected without hiding other candidates.
    #[serde(default = "default_evidence_ready")]
    pub evidence_ready: bool,
    #[serde(default)]
    pub evidence_reasons: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaidEndpointQuoteObservation {
    pub canonical_model_id: String,
    pub endpoint_id: String,
    pub input_micros_per_million_tokens: u64,
    pub output_micros_per_million_tokens: u64,
    pub request_fee_micros: u64,
    pub context_tokens: u64,
    pub max_prompt_tokens: Option<u64>,
    pub max_completion_tokens: u64,
    pub source: String,
    pub observed_at_ms: u128,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum BillingPolicy {
    Local,
    /// Provider-managed subscription billing; unknown allowance is not treated
    /// as zero-cost or unlimited, and no API-cost estimate is fabricated.
    Subscription,
    Paid,
    Unknown,
}

fn default_evidence_ready() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CandidateRoute {
    pub provider_id: String,
    pub model_id: String,
}

/// Optional per-task needs. Legacy tasks use Standard/Balanced, require tools,
/// and do not claim local-only privacy. An omitted candidate list permits only
/// the current settings route; it never expands providers automatically.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TaskRoutingRequirements {
    #[serde(default)]
    pub complexity: Option<Complexity>,
    #[serde(default)]
    pub pace: Option<Pace>,
    #[serde(default)]
    pub require_tools: Option<bool>,
    #[serde(default)]
    pub local_only: Option<bool>,
    /// Minimum context capacity required by this task.
    #[serde(default)]
    pub context_tokens: Option<u64>,
    #[serde(default)]
    pub desired_completion_minutes: Option<u32>,
    #[serde(default)]
    pub max_input_tokens: Option<u64>,
    #[serde(default)]
    pub max_output_tokens: Option<u64>,
    #[serde(default)]
    pub allowed_candidates: Vec<CandidateRoute>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AssignmentRequest {
    pub task_id: String,
    pub request: Request,
    pub required_runtime_id: Option<String>,
    pub max_input_tokens: Option<u64>,
    pub max_output_tokens: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct JobRoutingRequest {
    pub implementer: AssignmentRequest,
    pub reviewer: Option<AssignmentRequest>,
    pub candidate_facts: Vec<CandidateFacts>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RouteProvenance {
    pub task_id: String,
    pub runtime_id: Option<String>,
    pub explicit_override: bool,
    pub selected_reason: Option<String>,
    pub estimated_cost_micros: Option<u64>,
    pub pricing_source: Option<String>,
    pub pricing_verified_at_ms: Option<u128>,
    pub decision: Decision,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobRoutingDecision {
    pub implementer: RouteProvenance,
    pub reviewer: Option<RouteProvenance>,
}

/// Routes implementer and reviewer independently. Provider/model eligibility is
/// filtered before pace ranking; a paid route without verified prices or an
/// output token ceiling is rejected before it can be reserved or dispatched.
pub fn route_job(request: JobRoutingRequest) -> JobRoutingDecision {
    let implementer = route_assignment(&request.implementer, &request.candidate_facts);
    let reviewer = request
        .reviewer
        .as_ref()
        .map(|assignment| route_assignment(assignment, &request.candidate_facts));
    JobRoutingDecision {
        implementer,
        reviewer,
    }
}

fn route_assignment(assignment: &AssignmentRequest, facts: &[CandidateFacts]) -> RouteProvenance {
    let mut policy_request = assignment.request.clone();
    let mut policy_candidates = Vec::new();
    let mut rejected = Vec::new();
    let mut usable_facts = Vec::new();
    for candidate in &assignment.request.candidates {
        let fact = facts.iter().find(|fact| {
            fact.provider_id == candidate.provider_id && fact.model_id == candidate.model_id
        });
        let mut reason = None;
        if crate::economy::is_router_model_id(&candidate.model_id) {
            reason =
                Some("Rejected: provider router IDs are not fixed model identities.".to_owned());
        }
        match fact {
            None if reason.is_none() => reason = Some("Rejected: provider runtime, authentication, health, and privacy facts are missing.".to_owned()),
            None => {},
            Some(fact) => {
                if reason.is_some() {}
                else if crate::economy::is_free_variant_id(&candidate.model_id) && fact.economy_admission.is_none() {
                    reason = Some("Rejected: free model variants require backend verified Economy admission.".into());
                }
                else if !fact.evidence_ready { reason = Some(fact.evidence_reasons.first().cloned().unwrap_or_else(|| "Rejected: provider observations are incomplete or stale.".to_owned())); }
                else if fact.billing == BillingPolicy::Unknown { reason = Some("Rejected: provider billing type is unknown.".into()); }
                else if !fact.runtime_installed { reason = Some("Rejected: configured runtime is not installed.".into()); }
                else if assignment.required_runtime_id.as_ref().is_some_and(|runtime| runtime != &fact.runtime_id) { reason = Some("Rejected: candidate does not match the required runtime.".into()); }
                else if !fact.model_available { reason = Some("Rejected: configured model is unavailable.".into()); }
                else if !fact.authenticated { reason = Some("Rejected: provider credentials or authentication are unavailable.".into()); }
                else if !fact.healthy || fact.health_checked_at_ms.is_none_or(|checked| checked > assignment.request.now_ms || assignment.request.now_ms.saturating_sub(checked) > 5 * 60 * 1000) { reason = Some("Rejected: provider health is unavailable, stale, or unhealthy.".into()); }
                else if assignment.request.local_only && (!candidate.is_local || !fact.local_only) { reason = Some("Rejected: local-only privacy excludes this provider.".into()); }
                else if assignment.request.pace == Pace::Economy
                    && fact.billing == BillingPolicy::Paid
                    && fact.input_micros_per_million_tokens == Some(0)
                    && fact.output_micros_per_million_tokens == Some(0)
                    && fact.economy_admission.is_none()
                { reason = Some("Rejected: zero-price remote models need dated coding-trial and endpoint privacy evidence before Economy selection.".into()); }
                else if fact.billing == BillingPolicy::Paid && (assignment.max_input_tokens.is_none() || assignment.max_output_tokens.is_none()) { reason = Some("Rejected: paid work requires maximum input and output-token policies.".into()); }
                else if fact.billing == BillingPolicy::Paid && (fact.input_micros_per_million_tokens.is_none() || fact.output_micros_per_million_tokens.is_none() || fact.pricing_source.as_deref().is_none_or(|source|source.trim().is_empty()) || fact.pricing_verified_at_ms.is_none_or(|checked| checked > assignment.request.now_ms || assignment.request.now_ms.saturating_sub(checked) > 24 * 60 * 60 * 1000)) { reason = Some("Rejected: paid work requires fresh verified input and output pricing provenance.".into()); }
            }
        }
        if let Some(reason) = reason {
            rejected.push(CandidateDecision {
                provider_id: candidate.provider_id.clone(),
                provider_name: candidate.provider_name.clone(),
                model_id: candidate.model_id.clone(),
                model_name: candidate.model_name.clone(),
                eligible: false,
                selected: false,
                reasons: vec![reason],
            });
        } else {
            let fact = fact.expect("eligible candidates have facts");
            let mut observed_candidate = candidate.clone();
            observed_candidate.context_tokens = fact.context_tokens;
            observed_candidate.supports_tools = fact.supports_tools;
            observed_candidate.is_local = fact.local_only;
            policy_candidates.push(observed_candidate);
            usable_facts.push(fact);
        }
    }
    policy_request.candidates = policy_candidates;
    let mut decision = decide(policy_request);
    decision.candidates.extend(rejected);
    // A job-level explicit override is a commitment. If it is absent from the
    // currently eligible set, pause instead of silently selecting another route.
    let override_incomplete = assignment.request.manual_provider_id.is_some()
        != assignment.request.manual_model_id.is_some();
    if override_incomplete {
        decision.selected_provider_id = None;
        decision.selected_model_id = None;
        for row in &mut decision.candidates {
            row.selected = false;
        }
    } else if let (Some(provider), Some(model)) = (
        &assignment.request.manual_provider_id,
        &assignment.request.manual_model_id,
    ) {
        let accepted = decision
            .candidates
            .iter()
            .any(|row| row.provider_id == *provider && row.model_id == *model && row.eligible);
        if !accepted {
            decision.selected_provider_id = None;
            decision.selected_model_id = None;
            for row in &mut decision.candidates {
                row.selected = false;
            }
        }
    }
    // For Economy, use verified bounded estimates where available, then retain
    // the original deterministic policy as the stable tie-breaker.
    if assignment.request.pace == Pace::Economy && assignment.request.manual_provider_id.is_none() {
        let selected = decision
            .candidates
            .iter()
            .find(|row| row.selected)
            .map(|row| (row.provider_id.clone(), row.model_id.clone()));
        let mut ranked = decision
            .candidates
            .iter()
            .filter(|row| row.eligible)
            .filter_map(|row| {
                let fact = usable_facts.iter().find(|fact| {
                    fact.provider_id == row.provider_id && fact.model_id == row.model_id
                })?;
                let estimate = estimate_candidate_cost(
                    fact,
                    assignment.max_input_tokens?,
                    assignment.max_output_tokens?,
                );
                Some(((row.provider_id.clone(), row.model_id.clone()), estimate))
            })
            .collect::<Vec<_>>();
        ranked.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
        if let Some(((provider, model), _)) = ranked.first() {
            for row in &mut decision.candidates {
                row.selected =
                    row.eligible && row.provider_id == *provider && row.model_id == *model;
                row.reasons.retain(|reason| {
                    !reason.starts_with("Selected") && !reason.starts_with("Highest eligible")
                });
                if row.eligible {
                    row.reasons.push(if row.selected {
                        "Selected as the lowest verified maximum-cost eligible route under Economy."
                            .into()
                    } else {
                        "Eligible route ranked after the selected maximum-cost estimate.".into()
                    });
                }
            }
            decision.selected_provider_id = Some(provider.clone());
            decision.selected_model_id = Some(model.clone());
        } else {
            let _ = selected;
            decision.selected_provider_id = None;
            decision.selected_model_id = None;
            for row in &mut decision.candidates {
                row.selected = false;
            }
        }
    }
    let selected_fact = decision
        .selected_provider_id
        .as_ref()
        .zip(decision.selected_model_id.as_ref())
        .and_then(|(provider, model)| {
            facts
                .iter()
                .find(|fact| &fact.provider_id == provider && &fact.model_id == model)
        });
    let cost = selected_fact.and_then(|fact| match fact.billing {
        BillingPolicy::Local => Some(0),
        BillingPolicy::Subscription | BillingPolicy::Unknown => None,
        BillingPolicy::Paid => estimate_cost_micros(
            fact,
            assignment.max_input_tokens?,
            assignment.max_output_tokens?,
        ),
    });
    let selected_reason = decision
        .candidates
        .iter()
        .find(|row| row.selected)
        .and_then(|row| {
            row.reasons.iter().rev().find(|reason| {
                reason.starts_with("Selected")
                    || reason.starts_with("Highest eligible")
                    || reason.starts_with("Economy selected")
            })
        })
        .cloned();
    RouteProvenance {
        task_id: assignment.task_id.clone(),
        runtime_id: selected_fact.map(|fact| fact.runtime_id.clone()),
        explicit_override: assignment.request.manual_provider_id.is_some(),
        selected_reason,
        estimated_cost_micros: cost,
        pricing_source: selected_fact.and_then(|fact| fact.pricing_source.clone()),
        pricing_verified_at_ms: selected_fact.and_then(|fact| fact.pricing_verified_at_ms),
        decision,
    }
}

fn estimate_cost_micros(
    fact: &CandidateFacts,
    input_tokens: u64,
    output_tokens: u64,
) -> Option<u64> {
    let input = fact.input_micros_per_million_tokens? as u128 * input_tokens as u128;
    let output = fact.output_micros_per_million_tokens? as u128 * output_tokens as u128;
    u64::try_from(input.saturating_add(output).div_ceil(1_000_000)).ok()
}

fn estimate_candidate_cost(
    fact: &CandidateFacts,
    input_tokens: u64,
    output_tokens: u64,
) -> Option<u64> {
    match fact.billing {
        BillingPolicy::Local => Some(0),
        BillingPolicy::Subscription | BillingPolicy::Unknown => None,
        BillingPolicy::Paid => estimate_cost_micros(fact, input_tokens, output_tokens),
    }
}

pub const FALLBACK_POLICY_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum FallbackTrigger {
    ProviderFailure,
    QuotaBoundary,
    QuotaUnknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FallbackPolicy {
    pub enabled: bool,
    pub on_provider_failure: bool,
    pub on_quota_boundary: bool,
    pub local_only: bool,
    pub max_handoffs: u8,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FallbackRequest {
    pub source_run_id: String,
    pub policy: FallbackPolicy,
    pub trigger: FallbackTrigger,
    pub current_provider_id: String,
    pub visited_provider_ids: Vec<String>,
    pub handoff_count: u8,
    pub source_run_terminal: bool,
    pub safe_checkpoint_ref: Option<String>,
    pub effects_reconciled: bool,
    pub routing: Request,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FallbackDecision {
    pub policy_version: u32,
    pub accepted: bool,
    pub local_only: bool,
    pub source_run_id: String,
    pub trigger: FallbackTrigger,
    pub source_provider_id: String,
    pub target_provider_id: Option<String>,
    pub target_model_id: Option<String>,
    pub checkpoint_ref: Option<String>,
    pub handoff_count: u8,
    pub reason: String,
    pub routing: Option<Decision>,
}

fn denied_fallback(request: &FallbackRequest, reason: impl Into<String>) -> FallbackDecision {
    FallbackDecision {
        policy_version: FALLBACK_POLICY_VERSION,
        accepted: false,
        local_only: request.policy.local_only,
        source_run_id: request.source_run_id.clone(),
        trigger: request.trigger,
        source_provider_id: request.current_provider_id.clone(),
        target_provider_id: None,
        target_model_id: None,
        checkpoint_ref: request.safe_checkpoint_ref.clone(),
        handoff_count: request.handoff_count,
        reason: reason.into(),
        routing: None,
    }
}

/// Propose one bounded provider fallback. This is a decision only: the caller
/// must show it to the user and require an explicit start action.
pub fn evaluate_fallback(mut request: FallbackRequest) -> FallbackDecision {
    if !request.policy.enabled {
        return denied_fallback(&request, "Provider fallback is disabled by policy.");
    }
    let trigger_enabled = match request.trigger {
        FallbackTrigger::ProviderFailure => request.policy.on_provider_failure,
        FallbackTrigger::QuotaBoundary => request.policy.on_quota_boundary,
        FallbackTrigger::QuotaUnknown => false,
    };
    if !trigger_enabled {
        return denied_fallback(
            &request,
            "This failure or quota state is not an enabled fallback boundary; unknown quota never triggers fallback.",
        );
    }
    if request.policy.max_handoffs == 0 || request.policy.max_handoffs > 3 {
        return denied_fallback(
            &request,
            "Fallback policy must allow between one and three handoffs.",
        );
    }
    if request.handoff_count >= request.policy.max_handoffs {
        return denied_fallback(
            &request,
            "The configured fallback handoff limit has been reached.",
        );
    }
    let Some(checkpoint_ref) = request.safe_checkpoint_ref.as_deref() else {
        return denied_fallback(&request, "No safe provider checkpoint is available.");
    };
    if !request.source_run_terminal
        || checkpoint_ref.trim().is_empty()
        || checkpoint_ref.len() > 256
        || checkpoint_ref.chars().any(char::is_control)
    {
        return denied_fallback(
            &request,
            "Fallback requires a terminal run and a valid safe checkpoint.",
        );
    }
    if !request.effects_reconciled {
        return denied_fallback(
            &request,
            "Completed external effects have not been reconciled; fallback is paused to prevent duplicates.",
        );
    }
    if request.source_run_id.trim().is_empty()
        || request.source_run_id.len() > 100
        || request.current_provider_id.trim().is_empty()
        || request.current_provider_id.len() > 80
        || request.visited_provider_ids.len() > 16
    {
        return denied_fallback(&request, "Fallback route history is invalid.");
    }
    if request.routing.manual_provider_id.is_some() != request.routing.manual_model_id.is_some() {
        return denied_fallback(
            &request,
            "A manual fallback override must include both provider and model.",
        );
    }
    let mut visited = request
        .visited_provider_ids
        .iter()
        .cloned()
        .collect::<HashSet<_>>();
    visited.insert(request.current_provider_id.clone());
    request
        .routing
        .candidates
        .retain(|candidate| !visited.contains(&candidate.provider_id));
    request.routing.local_only |= request.policy.local_only;
    let routing = decide(request.routing.clone());
    if let (Some(provider), Some(model)) = (
        request.routing.manual_provider_id.as_deref(),
        request.routing.manual_model_id.as_deref(),
    ) {
        if !routing.candidates.iter().any(|candidate| {
            candidate.provider_id == provider && candidate.model_id == model && candidate.eligible
        }) {
            return FallbackDecision {
                routing: Some(routing),
                ..denied_fallback(
                    &request,
                    "The manual fallback override is not eligible or has already appeared in route history.",
                )
            };
        }
    }
    let Some(provider_id) = routing.selected_provider_id.clone() else {
        return FallbackDecision {
            routing: Some(routing),
            ..denied_fallback(
                &request,
                "No eligible unvisited provider is available for this fallback.",
            )
        };
    };
    let model_id = routing.selected_model_id.clone();
    FallbackDecision {
        policy_version: FALLBACK_POLICY_VERSION,
        accepted: true,
        local_only: request.policy.local_only,
        source_run_id: request.source_run_id,
        trigger: request.trigger,
        source_provider_id: request.current_provider_id,
        target_provider_id: Some(provider_id),
        target_model_id: model_id,
        checkpoint_ref: Some(checkpoint_ref.to_owned()),
        handoff_count: request.handoff_count.saturating_add(1),
        reason: "Fallback is eligible at this configured terminal checkpoint. Review the target and confirm before starting the handoff.".into(),
        routing: Some(routing),
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CalibrationSample {
    pub provider_id: String,
    pub model_id: String,
    pub succeeded: bool,
    pub latency_ms: Option<u128>,
    pub measured_usage: Option<f64>,
    pub usage_unit: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CalibrationSummary {
    pub provider_id: String,
    pub model_id: String,
    pub sample_count: usize,
    pub success_rate: f64,
    pub median_latency_ms: Option<u128>,
    pub measured_usage_by_unit: Vec<CalibrationUsageSummary>,
    pub unknown_usage_count: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CalibrationUsageSummary {
    pub unit: String,
    pub sample_count: usize,
    pub median_value: f64,
}

pub fn has_confirmed_quota_boundary(request: &Request, provider_id: &str) -> bool {
    request.usage_signals.iter().any(|signal| {
        signal.category == UsageCategory::SubscriptionWindow
            && signal.status == UsageStatus::Available
            && signal.confidence == "measured"
            && signal
                .provider_id
                .as_deref()
                .is_some_and(|id| id == provider_id)
            && signal.unit == "% used"
            && signal
                .value
                .is_some_and(|value| value.is_finite() && value >= 100.0)
            && signal.collected_at_ms <= request.now_ms
            && request.now_ms.saturating_sub(signal.collected_at_ms)
                <= max_age_ms(UsageCategory::SubscriptionWindow)
            && signal
                .reset_at_ms
                .is_none_or(|reset_at| reset_at > request.now_ms)
    })
}

pub fn calibrate(samples: Vec<CalibrationSample>) -> Vec<CalibrationSummary> {
    let mut groups = std::collections::BTreeMap::<(String, String), Vec<CalibrationSample>>::new();
    for sample in samples.into_iter().filter(|sample| {
        !sample.provider_id.trim().is_empty()
            && !sample.model_id.trim().is_empty()
            && sample.measured_usage.is_none_or(|value| {
                value.is_finite()
                    && value >= 0.0
                    && sample.usage_unit.as_deref().is_some_and(|unit| {
                        !unit.trim().is_empty()
                            && unit.len() <= 40
                            && !unit.chars().any(char::is_control)
                    })
            })
    }) {
        groups
            .entry((sample.provider_id.clone(), sample.model_id.clone()))
            .or_default()
            .push(sample);
    }
    groups
        .into_iter()
        .map(|((provider_id, model_id), group)| {
            let mut latencies = group
                .iter()
                .filter_map(|sample| sample.latency_ms)
                .collect::<Vec<_>>();
            latencies.sort_unstable();
            let median_latency_ms =
                (!latencies.is_empty()).then(|| latencies[(latencies.len() - 1) / 2]);
            let mut usage_by_unit = std::collections::BTreeMap::<String, Vec<f64>>::new();
            for sample in &group {
                if let (Some(value), Some(unit)) =
                    (sample.measured_usage, sample.usage_unit.as_ref())
                {
                    usage_by_unit.entry(unit.clone()).or_default().push(value);
                }
            }
            let measured_usage_by_unit = usage_by_unit
                .into_iter()
                .map(|(unit, mut values)| {
                    values.sort_by(f64::total_cmp);
                    CalibrationUsageSummary {
                        unit,
                        sample_count: values.len(),
                        median_value: values[(values.len() - 1) / 2],
                    }
                })
                .collect();
            let sample_count = group.len();
            let succeeded = group.iter().filter(|sample| sample.succeeded).count();
            CalibrationSummary {
                provider_id,
                model_id,
                sample_count,
                success_rate: succeeded as f64 / sample_count as f64,
                median_latency_ms,
                measured_usage_by_unit,
                unknown_usage_count: group
                    .iter()
                    .filter(|sample| sample.measured_usage.is_none())
                    .count(),
            }
        })
        .collect()
}

fn measured_signals<'a>(
    request: &'a Request,
    candidate: &Candidate,
    category: UsageCategory,
) -> Vec<&'a UsageSignal> {
    request
        .usage_signals
        .iter()
        .filter(|signal| {
            signal.category == category
                && signal.status == UsageStatus::Available
                && signal.confidence == "measured"
                && signal.provider_id.as_deref() == Some(candidate.telemetry_provider_id.as_str())
                && signal
                    .model_id
                    .as_deref()
                    .is_none_or(|id| id == candidate.model_id)
                && signal.collected_at_ms <= request.now_ms
                && request.now_ms.saturating_sub(signal.collected_at_ms) <= max_age_ms(category)
                && signal
                    .value
                    .is_some_and(|value| value.is_finite() && value >= 0.0)
                && match category {
                    UsageCategory::SubscriptionWindow => {
                        signal.unit == "% used"
                            && signal.value.is_some_and(|value| value <= 100.0)
                            && signal
                                .reset_at_ms
                                .is_none_or(|reset_at| reset_at > request.now_ms)
                    }
                    UsageCategory::ApiSpend => matches!(signal.unit.as_str(), "USD" | "$"),
                    UsageCategory::ContextLimit | UsageCategory::LocalResource => false,
                }
        })
        .collect()
}

fn measured_signal<'a>(
    request: &'a Request,
    candidate: &Candidate,
    category: UsageCategory,
) -> Option<&'a UsageSignal> {
    measured_signals(request, candidate, category)
        .into_iter()
        .max_by_key(|signal| signal.collected_at_ms)
}

fn subscription_window_usage(request: &Request, candidate: &Candidate) -> Option<(f64, usize)> {
    let signals = measured_signals(request, candidate, UsageCategory::SubscriptionWindow);
    let count = signals.len();
    let most_used = signals
        .into_iter()
        .filter_map(|signal| signal.value)
        .max_by(f64::total_cmp)?;
    Some((most_used, count))
}

fn max_age_ms(category: UsageCategory) -> u128 {
    match category {
        UsageCategory::SubscriptionWindow => 30 * 60 * 1000,
        UsageCategory::ApiSpend => 24 * 60 * 60 * 1000,
        UsageCategory::ContextLimit => 4 * 60 * 60 * 1000,
        UsageCategory::LocalResource => 5 * 60 * 1000,
    }
}

fn latency(candidate: &Candidate, now_ms: u128) -> Option<u128> {
    let collected = candidate.latency_collected_at_ms?;
    if collected > now_ms || now_ms.saturating_sub(collected) > MAX_LATENCY_AGE_MS {
        return None;
    }
    candidate.latency_ms
}

fn is_openrouter_free_route(model_id: &str) -> bool {
    model_id == "openrouter/free"
        || model_id == "openrouter/auto"
        || model_id.ends_with(":free")
        || model_id.ends_with("/free")
}

fn latency_first(a: Option<u128>, b: Option<u128>) -> std::cmp::Ordering {
    match (a, b) {
        (Some(a), Some(b)) => a.cmp(&b),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    }
}

fn quality(candidate: &Candidate) -> Option<u8> {
    candidate
        .quality_rating
        .filter(|score| (1..=5).contains(score))
}

fn effort_rank(candidate: &Candidate) -> u8 {
    if candidate
        .reasoning_efforts
        .iter()
        .any(|value| matches!(value.to_ascii_lowercase().as_str(), "max" | "xhigh"))
    {
        3
    } else if candidate
        .reasoning_efforts
        .iter()
        .any(|value| value.eq_ignore_ascii_case("high"))
    {
        2
    } else if candidate
        .reasoning_efforts
        .iter()
        .any(|value| value.eq_ignore_ascii_case("medium"))
    {
        1
    } else {
        0
    }
}

fn complexity_effort(candidate: &Candidate, complexity: Complexity) -> u8 {
    if complexity == Complexity::Complex {
        effort_rank(candidate)
    } else {
        0
    }
}

fn desired_latency_ordering(request: &Request, a: &Candidate, b: &Candidate) -> std::cmp::Ordering {
    if request.desired_completion_minutes.is_some() {
        latency_first(latency(a, request.now_ms), latency(b, request.now_ms))
    } else {
        std::cmp::Ordering::Equal
    }
}

fn usage_value(request: &Request, candidate: &Candidate, category: UsageCategory) -> Option<f64> {
    if category == UsageCategory::SubscriptionWindow {
        return subscription_window_usage(request, candidate).map(|(value, _)| value);
    }
    measured_signal(request, candidate, category)?
        .value
        .filter(|value| value.is_finite() && *value >= 0.0)
}

fn lower_measurement_first(a: Option<f64>, b: Option<f64>) -> std::cmp::Ordering {
    match (a, b) {
        (Some(a), Some(b)) => a.partial_cmp(&b).unwrap_or(std::cmp::Ordering::Equal),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    }
}

pub fn decide(request: Request) -> Decision {
    let required_context_tokens = if request.require_context_evidence {
        request
            .complexity
            .context_floor()
            .max(request.minimum_context_tokens.unwrap_or(0))
    } else {
        0
    };
    let mut rows = request.candidates.iter().cloned().map(|candidate| {
        let mut reasons = Vec::new();
        let mut eligible = true;
        if request.require_context_evidence {
            match candidate.context_tokens {
                Some(tokens) if tokens >= required_context_tokens => reasons.push(format!("Observed context capacity {tokens} tokens meets the {required_context_tokens}-token task requirement.")),
                Some(tokens) => { eligible = false; reasons.push(format!("Rejected: context is {tokens} tokens; {required_context_tokens} required.")); },
                None => { eligible = false; reasons.push("Rejected: requested model context capacity is unknown.".into()); },
            }
        } else {
            reasons.push("Task did not request a context-capacity guarantee; no capacity is inferred.".into());
        }
        if request.require_tools {
            match candidate.supports_tools {
                Some(true) => reasons.push("Required tool support is reported.".into()),
                Some(false) => { eligible = false; reasons.push("Rejected: model reports no tool support.".into()); },
                None => { eligible = false; reasons.push("Rejected: tool support is unknown.".into()); },
            }
        }
        if crate::economy::is_router_model_id(&candidate.model_id) {
            eligible = false;
            reasons.push("Rejected: provider router IDs are not fixed model identities.".into());
        }
        if crate::economy::is_free_variant_id(&candidate.model_id)
            && (request.pace != Pace::Economy || !candidate.economy_evidence_ready)
        {
            eligible = false;
            reasons.push("Rejected: free model variants require backend verified Economy admission and cannot use paid routing.".into());
        }
        if request.pace == Pace::Economy
            && !candidate.is_local
            && is_openrouter_free_route(&candidate.model_id)
            && !candidate.economy_evidence_ready
        {
            eligible = false;
            reasons.push("Rejected: this zero-price remote model has no backend-verified coding trial and endpoint privacy evidence.".into());
        }
        if request.local_only && !candidate.is_local {
            eligible = false;
            reasons.push("Rejected: local-only privacy excludes this provider.".into());
        } else if request.local_only {
            reasons.push("Local-only privacy requirement is met.".into());
        }
        if let Some(rating) = quality(&candidate) {
            reasons.push(format!("User-estimated quality rating: {rating}/5."));
        } else {
            reasons.push("Quality rating is unknown; no quality is inferred.".into());
        }
        if request.complexity == Complexity::Complex {
            let effort = match effort_rank(&candidate) {
                3 => "very high",
                2 => "high",
                1 => "medium",
                _ => "unknown or low",
            };
            reasons.push(format!("Complex work prefers stronger configured reasoning effort; this model reports {effort}."));
        }
        if let Some(measured) = latency(&candidate, request.now_ms) {
            reasons.push(format!("Recent measured run latency: {measured} ms."));
            if request.desired_completion_minutes.is_some() {
                reasons.push("Recent latency informs the desired-time preference only; it does not predict this task's completion time.".into());
            }
        } else {
            reasons.push("No fresh completed-run latency measurement is available.".into());
        }
        if let Some((value, window_count)) = subscription_window_usage(&request, &candidate) {
            reasons.push(format!("Fresh measured subscription usage: {value}% used in the most constrained of {window_count} active window(s)."));
        } else {
            reasons.push("No fresh measured subscription usage is available.".into());
        }
        if let Some(value) = usage_value(&request, &candidate, UsageCategory::ApiSpend) {
            reasons.push(format!("Fresh measured API spend: {value} USD."));
        } else {
            reasons.push("No fresh comparable USD API spend measurement is available.".into());
        }
        CandidateDecision {
            provider_id: candidate.provider_id.clone(), provider_name: candidate.provider_name.clone(),
            model_id: candidate.model_id.clone(), model_name: candidate.model_name.clone(),
            eligible, selected: false, reasons,
        }
    }).collect::<Vec<_>>();

    let candidates = request.candidates.clone();
    // Stable, deterministic rank by selected pace; unknown measurements remain last.
    let mut eligible_indexes = rows
        .iter()
        .enumerate()
        .filter(|(_, row)| row.eligible)
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    eligible_indexes.sort_by(|left, right| {
        let a = &candidates[*left];
        let b = &candidates[*right];
        let cmp = match request.pace {
            Pace::Fast => latency_first(latency(a, request.now_ms), latency(b, request.now_ms))
                .then_with(|| {
                    complexity_effort(b, request.complexity)
                        .cmp(&complexity_effort(a, request.complexity))
                })
                .then_with(|| quality(b).cmp(&quality(a)))
                .then_with(|| {
                    lower_measurement_first(
                        usage_value(&request, a, UsageCategory::SubscriptionWindow),
                        usage_value(&request, b, UsageCategory::SubscriptionWindow),
                    )
                }),
            Pace::Balanced => quality(b)
                .cmp(&quality(a))
                .then_with(|| desired_latency_ordering(&request, a, b))
                .then_with(|| {
                    complexity_effort(b, request.complexity)
                        .cmp(&complexity_effort(a, request.complexity))
                })
                .then_with(|| {
                    lower_measurement_first(
                        usage_value(&request, a, UsageCategory::SubscriptionWindow),
                        usage_value(&request, b, UsageCategory::SubscriptionWindow),
                    )
                })
                .then_with(|| {
                    latency_first(latency(a, request.now_ms), latency(b, request.now_ms))
                }),
            Pace::Thorough => quality(b)
                .cmp(&quality(a))
                .then_with(|| desired_latency_ordering(&request, a, b))
                .then_with(|| {
                    complexity_effort(b, request.complexity)
                        .cmp(&complexity_effort(a, request.complexity))
                })
                .then_with(|| effort_rank(b).cmp(&effort_rank(a)))
                .then_with(|| {
                    latency_first(latency(a, request.now_ms), latency(b, request.now_ms))
                }),
            Pace::Economy => lower_measurement_first(
                usage_value(&request, a, UsageCategory::ApiSpend),
                usage_value(&request, b, UsageCategory::ApiSpend),
            )
            .then_with(|| {
                lower_measurement_first(
                    usage_value(&request, a, UsageCategory::SubscriptionWindow),
                    usage_value(&request, b, UsageCategory::SubscriptionWindow),
                )
            })
            .then_with(|| desired_latency_ordering(&request, a, b))
            .then_with(|| {
                complexity_effort(b, request.complexity)
                    .cmp(&complexity_effort(a, request.complexity))
            })
            .then_with(|| latency_first(latency(a, request.now_ms), latency(b, request.now_ms))),
        };
        cmp.then_with(|| a.provider_id.cmp(&b.provider_id))
            .then_with(|| a.model_id.cmp(&b.model_id))
    });

    let manual_index = request
        .manual_provider_id
        .as_ref()
        .zip(request.manual_model_id.as_ref())
        .and_then(|(provider, model)| {
            candidates.iter().position(|candidate| {
                candidate.provider_id == *provider && candidate.model_id == *model
            })
        });
    let selected_index = manual_index
        .filter(|index| rows[*index].eligible)
        .or_else(|| eligible_indexes.first().copied());
    if let (Some(manual_index), Some(provider), Some(model)) = (
        manual_index,
        request.manual_provider_id.as_ref(),
        request.manual_model_id.as_ref(),
    ) {
        if rows[manual_index].eligible {
            rows[manual_index]
                .reasons
                .push("Selected by the user's eligible manual override.".into());
        } else {
            rows[manual_index].reasons.push(format!(
                "Manual override {provider}/{model} was ignored because it is ineligible."
            ));
        }
    }
    if let Some(index) = selected_index {
        rows[index].selected = true;
        if manual_index != Some(index) {
            rows[index].reasons.push(format!(
                "Highest eligible {:?} preference under the deterministic policy.",
                request.pace
            ));
        }
    }
    for index in eligible_indexes {
        if Some(index) != selected_index {
            rows[index].reasons.push(format!(
                "Eligible alternative ranked after the selected model for {:?} pace.",
                request.pace
            ));
        }
    }
    let desired_completion_note = request.desired_completion_minutes.map(|minutes| format!("{minutes} minutes is a soft preference; it can favor models with lower recent measured latency after the primary pace preference. Prior run latency cannot predict or guarantee this task's completion time.")).unwrap_or_else(|| "No desired completion time supplied; completion time is not estimated or guaranteed.".into());
    Decision {
        selected_provider_id: selected_index.map(|index| rows[index].provider_id.clone()),
        selected_model_id: selected_index.map(|index| rows[index].model_id.clone()),
        required_context_tokens,
        desired_completion_note,
        candidates: rows,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usage::UsageSignal;

    fn candidate(
        id: &str,
        local: bool,
        context: Option<u64>,
        tools: Option<bool>,
        rating: Option<u8>,
        latency: Option<u128>,
    ) -> Candidate {
        Candidate {
            provider_id: id.into(),
            telemetry_provider_id: id.into(),
            provider_name: id.into(),
            model_id: "model".into(),
            model_name: "model".into(),
            is_local: local,
            context_tokens: context,
            supports_tools: tools,
            reasoning_efforts: vec!["high".into()],
            quality_rating: rating,
            latency_ms: latency,
            latency_collected_at_ms: latency.map(|_| 10_000),
            economy_evidence_ready: false,
        }
    }

    fn request(candidates: Vec<Candidate>) -> Request {
        Request {
            complexity: Complexity::Standard,
            pace: Pace::Balanced,
            require_context_evidence: true,
            minimum_context_tokens: None,
            require_tools: true,
            local_only: false,
            desired_completion_minutes: None,
            manual_provider_id: None,
            manual_model_id: None,
            candidates,
            usage_signals: vec![],
            now_ms: 10_100,
        }
    }

    #[test]
    fn unknown_context_is_tolerated_only_when_task_did_not_request_a_context_guarantee() {
        let candidate = candidate("codex", false, None, Some(true), Some(5), None);
        let mut request = request(vec![candidate]);
        request.require_context_evidence = false;
        let decision = decide(request.clone());
        assert_eq!(decision.selected_provider_id.as_deref(), Some("codex"));
        assert_eq!(decision.required_context_tokens, 0);

        request.require_context_evidence = true;
        let decision = decide(request);
        assert_eq!(decision.selected_provider_id, None);
        assert!(decision
            .candidates
            .iter()
            .flat_map(|row| &row.reasons)
            .any(|reason| reason.contains("context capacity is unknown")));
    }

    #[test]
    fn rejects_unknown_tools_and_insufficient_context_before_ranking() {
        let decision = decide(request(vec![
            candidate("unknown-tools", false, Some(64_000), None, Some(5), Some(1)),
            candidate("small", false, Some(8_000), Some(true), Some(5), Some(1)),
            candidate(
                "eligible",
                false,
                Some(32_000),
                Some(true),
                Some(3),
                Some(10),
            ),
        ]));
        assert_eq!(decision.selected_provider_id.as_deref(), Some("eligible"));
        assert!(!decision.candidates[0].eligible);
        assert!(!decision.candidates[1].eligible);
    }

    #[test]
    fn local_only_filters_remote_and_manual_override_wins_when_eligible() {
        let mut request = request(vec![
            candidate("remote", false, Some(64_000), Some(true), Some(5), Some(1)),
            candidate("local", true, Some(32_000), Some(true), Some(1), Some(20)),
        ]);
        request.local_only = true;
        request.manual_provider_id = Some("local".into());
        request.manual_model_id = Some("model".into());
        let decision = decide(request);
        assert_eq!(decision.selected_provider_id.as_deref(), Some("local"));
        assert!(!decision.candidates[0].eligible);
        assert!(decision.candidates[1].selected);
    }

    #[test]
    fn pace_ranking_uses_only_fresh_measured_values_and_time_is_not_a_guarantee() {
        let mut request = request(vec![
            candidate("slow", false, Some(32_000), Some(true), Some(5), Some(500)),
            candidate("fast", false, Some(32_000), Some(true), Some(2), Some(100)),
        ]);
        request.pace = Pace::Fast;
        request.desired_completion_minutes = Some(1);
        request.usage_signals.push(UsageSignal {
            id: "estimate".into(),
            provider_id: Some("slow".into()),
            model_id: Some("model".into()),
            category: UsageCategory::ApiSpend,
            metric: "spend".into(),
            label: "spend".into(),
            value: Some(0.0),
            unit: "USD".into(),
            window_duration_seconds: None,
            reset_at_ms: None,
            collected_at_ms: 10_000,
            source: "manual".into(),
            confidence: "estimated".into(),
            status: UsageStatus::Estimated,
            note: None,
        });
        let decision = decide(request);
        assert_eq!(decision.selected_provider_id.as_deref(), Some("fast"));
        assert!(decision.desired_completion_note.contains("soft preference"));
    }

    #[test]
    fn economy_uses_fresh_comparable_usd_and_not_stale_or_unlike_units() {
        let mut request = request(vec![
            candidate(
                "costly",
                false,
                Some(32_000),
                Some(true),
                Some(5),
                Some(100),
            ),
            candidate("cheap", false, Some(32_000), Some(true), Some(2), Some(300)),
        ]);
        request.pace = Pace::Economy;
        let signal =
            |provider: &str, value: f64, unit: &str, status, collected_at_ms| UsageSignal {
                id: format!("{provider}:{unit}"),
                provider_id: Some(provider.into()),
                model_id: Some("model".into()),
                category: UsageCategory::ApiSpend,
                metric: "spend".into(),
                label: "API spend".into(),
                value: Some(value),
                unit: unit.into(),
                window_duration_seconds: None,
                reset_at_ms: None,
                collected_at_ms,
                source: "provider telemetry".into(),
                confidence: "measured".into(),
                status,
                note: None,
            };
        request.usage_signals = vec![
            signal("costly", 0.01, "tokens", UsageStatus::Available, 10_000),
            signal("costly", 0.50, "USD", UsageStatus::Available, 10_000),
            signal("cheap", 0.02, "USD", UsageStatus::Available, 10_000),
            signal("cheap", 0.02, "USD", UsageStatus::Stale, 10_000),
        ];
        assert_eq!(
            decide(request).selected_provider_id.as_deref(),
            Some("cheap")
        );
    }

    #[test]
    fn economy_uses_profile_adapter_identity_and_most_constrained_subscription_window() {
        let mut profile = candidate(
            "codex-profile",
            false,
            Some(32_000),
            Some(true),
            Some(4),
            None,
        );
        profile.telemetry_provider_id = "codex".into();
        let other = candidate("other", false, Some(32_000), Some(true), Some(4), None);
        let mut request = request(vec![profile, other]);
        request.pace = Pace::Economy;
        let signal =
            |id: &str, provider: &str, metric: &str, value: f64, reset_at_ms| UsageSignal {
                id: id.into(),
                provider_id: Some(provider.into()),
                model_id: Some("model".into()),
                category: UsageCategory::SubscriptionWindow,
                metric: metric.into(),
                label: metric.into(),
                value: Some(value),
                unit: "% used".into(),
                window_duration_seconds: Some(3_600),
                reset_at_ms: Some(reset_at_ms),
                collected_at_ms: 10_000,
                source: "provider telemetry".into(),
                confidence: "measured".into(),
                status: UsageStatus::Available,
                note: None,
            };
        request.usage_signals = vec![
            signal("primary", "codex", "primaryUsagePercent", 100.0, 20_000),
            signal("secondary", "codex", "secondaryUsagePercent", 10.0, 30_000),
            signal("other-window", "other", "primaryUsagePercent", 50.0, 20_000),
        ];

        let decision = decide(request);
        assert_eq!(decision.selected_provider_id.as_deref(), Some("other"));
        assert!(decision.candidates[0]
            .reasons
            .iter()
            .any(|reason| reason.contains("100% used") && reason.contains("2 active window(s)")));
    }

    #[test]
    fn complex_tasks_prefer_higher_configured_reasoning_effort_after_pace_signals() {
        let mut basic = candidate("basic", false, Some(64_000), Some(true), Some(4), Some(100));
        basic.reasoning_efforts = vec!["low".into()];
        let mut advanced = candidate(
            "advanced",
            false,
            Some(64_000),
            Some(true),
            Some(4),
            Some(100),
        );
        advanced.reasoning_efforts = vec!["high".into()];
        let mut request = request(vec![basic, advanced]);
        request.complexity = Complexity::Complex;
        let decision = decide(request);
        assert_eq!(decision.required_context_tokens, COMPLEX_CONTEXT_TOKENS);
        assert_eq!(decision.selected_provider_id.as_deref(), Some("advanced"));
    }

    #[test]
    fn desired_completion_time_is_a_soft_recent_latency_preference() {
        let mut request = request(vec![
            candidate(
                "slower",
                false,
                Some(32_000),
                Some(true),
                Some(4),
                Some(500),
            ),
            candidate(
                "faster",
                false,
                Some(32_000),
                Some(true),
                Some(4),
                Some(100),
            ),
        ]);
        request.desired_completion_minutes = Some(30);
        let decision = decide(request);
        assert_eq!(decision.selected_provider_id.as_deref(), Some("faster"));
        assert!(decision.desired_completion_note.contains("soft preference"));
        assert!(decision.candidates[1]
            .reasons
            .iter()
            .any(|reason| reason.contains("does not predict")));
    }

    fn fallback_request(candidates: Vec<Candidate>) -> FallbackRequest {
        FallbackRequest {
            source_run_id: "run-1".into(),
            policy: FallbackPolicy {
                enabled: true,
                on_provider_failure: true,
                on_quota_boundary: true,
                local_only: false,
                max_handoffs: 1,
            },
            trigger: FallbackTrigger::ProviderFailure,
            current_provider_id: "primary".into(),
            visited_provider_ids: vec![],
            handoff_count: 0,
            source_run_terminal: true,
            safe_checkpoint_ref: Some("checkpoint-1".into()),
            effects_reconciled: true,
            routing: request(candidates),
        }
    }

    #[test]
    fn fallback_requires_a_configured_terminal_checkpoint_and_reconciled_effects() {
        let candidate = candidate(
            "backup",
            false,
            Some(32_000),
            Some(true),
            Some(4),
            Some(100),
        );
        let mut input = fallback_request(vec![candidate.clone()]);
        input.safe_checkpoint_ref = None;
        assert!(!evaluate_fallback(input).accepted);

        let mut input = fallback_request(vec![candidate.clone()]);
        input.source_run_terminal = false;
        assert!(!evaluate_fallback(input).accepted);

        let mut input = fallback_request(vec![candidate.clone()]);
        input.effects_reconciled = false;
        assert!(!evaluate_fallback(input).accepted);

        let mut input = fallback_request(vec![candidate]);
        input.trigger = FallbackTrigger::QuotaUnknown;
        let decision = evaluate_fallback(input);
        assert!(!decision.accepted);
        assert!(decision.reason.contains("unknown quota"));
    }

    #[test]
    fn fallback_preserves_local_only_and_eligible_manual_override() {
        let mut remote = candidate("remote", false, Some(32_000), Some(true), Some(5), Some(1));
        remote.model_id = "remote-model".into();
        let mut local = candidate("local", true, Some(32_000), Some(true), Some(3), Some(100));
        local.model_id = "local-model".into();
        let mut input = fallback_request(vec![remote, local]);
        input.policy.local_only = true;
        input.routing.manual_provider_id = Some("local".into());
        input.routing.manual_model_id = Some("local-model".into());
        let decision = evaluate_fallback(input);
        assert!(decision.accepted);
        assert_eq!(decision.target_provider_id.as_deref(), Some("local"));
        assert_eq!(decision.target_model_id.as_deref(), Some("local-model"));
        assert_eq!(decision.policy_version, FALLBACK_POLICY_VERSION);
        assert_eq!(decision.handoff_count, 1);

        let mut input = fallback_request(vec![candidate(
            "backup",
            false,
            None,
            Some(true),
            Some(4),
            Some(100),
        )]);
        input.routing.manual_provider_id = Some("backup".into());
        input.routing.manual_model_id = Some("model".into());
        let decision = evaluate_fallback(input);
        assert!(!decision.accepted);
        assert!(decision
            .reason
            .contains("manual fallback override is not eligible"));
    }

    #[test]
    fn fallback_excludes_visited_providers_and_stops_when_candidates_are_exhausted() {
        let mut input = fallback_request(vec![
            candidate("primary", false, Some(32_000), Some(true), Some(5), Some(1)),
            candidate("visited", false, Some(32_000), Some(true), Some(5), Some(1)),
        ]);
        input.visited_provider_ids = vec!["visited".into()];
        let decision = evaluate_fallback(input);
        assert!(!decision.accepted);
        assert!(decision.reason.contains("No eligible unvisited provider"));

        let mut input = fallback_request(vec![candidate(
            "backup",
            false,
            Some(32_000),
            Some(true),
            Some(5),
            Some(1),
        )]);
        input.handoff_count = 1;
        assert!(!evaluate_fallback(input).accepted);
    }

    #[test]
    fn quota_fallback_requires_fresh_measured_exhaustion_for_the_source_provider() {
        let mut input = fallback_request(vec![
            candidate(
                "backup",
                false,
                Some(32_000),
                Some(true),
                Some(4),
                Some(100),
            ),
            candidate(
                "primary",
                false,
                Some(32_000),
                Some(true),
                Some(3),
                Some(200),
            ),
        ]);
        input.trigger = FallbackTrigger::QuotaBoundary;
        input.policy.on_provider_failure = false;
        let mut signal = UsageSignal {
            id: "source-limit".into(),
            provider_id: Some("primary".into()),
            model_id: None,
            category: UsageCategory::SubscriptionWindow,
            metric: "usagePercent".into(),
            label: "Primary window".into(),
            value: Some(100.0),
            unit: "% used".into(),
            window_duration_seconds: Some(3600),
            reset_at_ms: Some(50_000),
            collected_at_ms: 10_000,
            source: "fixture".into(),
            confidence: "measured".into(),
            status: UsageStatus::Available,
            note: None,
        };
        input.routing.usage_signals = vec![signal.clone()];
        assert!(evaluate_fallback(input.clone()).accepted);
        signal.collected_at_ms = 1;
        input.routing.usage_signals = vec![signal];
        input.routing.now_ms = 2_000_000;
        assert!(!has_confirmed_quota_boundary(&input.routing, "primary"));
    }

    #[test]
    fn unknown_usage_stays_unknown_in_fallback_ranking() {
        let mut input = fallback_request(vec![
            candidate(
                "measured",
                false,
                Some(32_000),
                Some(true),
                Some(3),
                Some(200),
            ),
            candidate("unknown", false, Some(32_000), Some(true), Some(3), None),
        ]);
        input.routing.pace = Pace::Economy;
        let decision = evaluate_fallback(input);
        assert!(decision.accepted);
        let routing = decision.routing.unwrap();
        assert!(routing.candidates.iter().any(|row| {
            row.provider_id == "unknown"
                && row
                    .reasons
                    .iter()
                    .any(|reason| reason.contains("No fresh comparable USD API spend"))
        }));
    }

    #[test]
    fn calibration_summarizes_representative_outcomes_latency_and_measured_usage() {
        let summary = calibrate(vec![
            CalibrationSample {
                provider_id: "p".into(),
                model_id: "m".into(),
                succeeded: true,
                latency_ms: Some(100),
                measured_usage: Some(10.0),
                usage_unit: Some("tokens".into()),
            },
            CalibrationSample {
                provider_id: "p".into(),
                model_id: "m".into(),
                succeeded: false,
                latency_ms: Some(300),
                measured_usage: None,
                usage_unit: None,
            },
            CalibrationSample {
                provider_id: "p".into(),
                model_id: "m".into(),
                succeeded: true,
                latency_ms: None,
                measured_usage: Some(30.0),
                usage_unit: Some("tokens".into()),
            },
        ]);
        assert_eq!(summary.len(), 1);
        assert_eq!(summary[0].sample_count, 3);
        assert!((summary[0].success_rate - (2.0 / 3.0)).abs() < f64::EPSILON);
        assert_eq!(summary[0].median_latency_ms, Some(100));
        assert_eq!(summary[0].measured_usage_by_unit[0].unit, "tokens");
        assert_eq!(summary[0].measured_usage_by_unit[0].sample_count, 2);
        assert_eq!(summary[0].measured_usage_by_unit[0].median_value, 10.0);
        assert_eq!(summary[0].unknown_usage_count, 1);
    }

    fn facts(provider: &str, model: &str, local: bool, price: u64) -> CandidateFacts {
        CandidateFacts {
            provider_id: provider.into(),
            model_id: model.into(),
            runtime_id: format!("runtime-{provider}"),
            runtime_installed: true,
            authenticated: true,
            healthy: true,
            health_checked_at_ms: Some(10_100),
            model_available: true,
            local_only: local,
            billing: if local {
                BillingPolicy::Local
            } else {
                BillingPolicy::Paid
            },
            input_micros_per_million_tokens: Some(price),
            output_micros_per_million_tokens: Some(price),
            pricing_source: (!local).then(|| "fixture-pricing".into()),
            pricing_verified_at_ms: Some(10_100),
            economy_policy: None,
            economy_admission: None,
            paid_endpoint_quote: None,
            context_tokens: Some(32_768),
            supports_tools: Some(true),
            evidence_ready: true,
            evidence_reasons: Vec::new(),
        }
    }

    #[test]
    fn unpinned_paid_cost_estimate_keeps_using_the_catalog_quote() {
        let mut fact = facts("openrouter", "deepseek/deepseek-v4.1-flash", false, 2_490);
        fact.output_micros_per_million_tokens = Some(305_000);
        fact.paid_endpoint_quote = Some(PaidEndpointQuoteObservation {
            canonical_model_id: "deepseek/deepseek-v4.1-flash-20260910".into(),
            endpoint_id: "parasail/fp8".into(),
            input_micros_per_million_tokens: 300_000,
            output_micros_per_million_tokens: 1_200_000,
            request_fee_micros: 0,
            context_tokens: 1_048_576,
            max_prompt_tokens: None,
            max_completion_tokens: 943_718,
            source: "OpenRouter authenticated model endpoint".into(),
            observed_at_ms: 1_000,
        });
        assert_eq!(estimate_cost_micros(&fact, 16_384, 8_192), Some(2_540));
    }

    #[test]
    fn job_route_filters_unhealthy_missing_pricing_and_local_privacy_before_ranking() {
        let mut r = request(vec![
            candidate("remote", false, Some(32_000), Some(true), Some(5), Some(20)),
            candidate("local", true, Some(32_000), Some(true), Some(3), Some(100)),
        ]);
        r.pace = Pace::Economy;
        r.local_only = true;
        let mut remote = facts("remote", "model", false, 1);
        remote.healthy = false;
        let mut local = facts("local", "model", true, 0);
        local.billing = BillingPolicy::Local;
        local.pricing_verified_at_ms = None;
        local.input_micros_per_million_tokens = None;
        local.output_micros_per_million_tokens = None;
        let result = route_job(JobRoutingRequest {
            implementer: AssignmentRequest {
                task_id: "implementation".into(),
                request: r,
                required_runtime_id: None,
                max_input_tokens: Some(100),
                max_output_tokens: Some(100),
            },
            reviewer: None,
            candidate_facts: vec![remote, local],
        });
        assert_eq!(
            result.implementer.decision.selected_provider_id.as_deref(),
            Some("local")
        );
        assert!(result
            .implementer
            .decision
            .candidates
            .iter()
            .any(|row| row.provider_id == "remote" && !row.eligible));
        assert_eq!(result.implementer.estimated_cost_micros, Some(0));
    }

    #[test]
    fn paid_route_requires_fresh_prices_and_both_token_bounds_and_override_is_strict() {
        let mut r = request(vec![
            candidate("paid", false, Some(32_000), Some(true), Some(5), Some(20)),
            candidate("other", false, Some(32_000), Some(true), Some(5), Some(20)),
        ]);
        r.manual_provider_id = Some("paid".into());
        r.manual_model_id = Some("model".into());
        let mut paid = facts("paid", "model", false, 5);
        paid.pricing_verified_at_ms = None;
        let other = facts("other", "model", false, 9);
        let request = JobRoutingRequest {
            implementer: AssignmentRequest {
                task_id: "implementation".into(),
                request: r,
                required_runtime_id: None,
                max_input_tokens: Some(100),
                max_output_tokens: Some(100),
            },
            reviewer: None,
            candidate_facts: vec![paid, other],
        };
        let result = route_job(request);
        assert_eq!(result.implementer.decision.selected_provider_id, None);
        assert!(result
            .implementer
            .decision
            .candidates
            .iter()
            .any(|row| row.provider_id == "paid" && row.reasons.join(" ").contains("pricing")));
    }

    #[test]
    fn economy_never_selects_a_zero_price_remote_model_without_backend_evaluation_and_privacy_evidence(
    ) {
        let mut request = request(vec![candidate(
            "openrouter-main",
            false,
            Some(32_000),
            Some(true),
            Some(5),
            Some(20),
        )]);
        request.candidates[0].model_id = "vendor/model:free".into();
        request.pace = Pace::Economy;
        let mut free = facts("openrouter-main", "vendor/model:free", false, 0);
        free.input_micros_per_million_tokens = Some(0);
        free.output_micros_per_million_tokens = Some(0);
        let result = route_job(JobRoutingRequest {
            implementer: AssignmentRequest {
                task_id: "implementation".into(),
                request,
                required_runtime_id: Some("runtime-openrouter-main".into()),
                max_input_tokens: Some(100),
                max_output_tokens: Some(100),
            },
            reviewer: None,
            candidate_facts: vec![free],
        });
        assert_eq!(result.implementer.decision.selected_model_id, None);
        let row = &result.implementer.decision.candidates[0];
        assert!(!row.eligible);
        assert_eq!(row.model_id, "vendor/model:free");
        assert!(row
            .reasons
            .join(" ")
            .contains("backend verified Economy admission"));
    }

    #[test]
    fn paid_route_without_output_token_ceiling_is_rejected_even_with_fresh_pricing() {
        let r = request(vec![candidate(
            "paid",
            false,
            Some(32_000),
            Some(true),
            Some(5),
            Some(20),
        )]);
        let result = route_job(JobRoutingRequest {
            implementer: AssignmentRequest {
                task_id: "implementation".into(),
                request: r,
                max_input_tokens: Some(100),
                max_output_tokens: None,
                required_runtime_id: None,
            },
            reviewer: None,
            candidate_facts: vec![facts("paid", "model", false, 5)],
        });
        assert_eq!(result.implementer.decision.selected_model_id, None);
        assert!(result.implementer.decision.candidates[0]
            .reasons
            .join(" ")
            .contains("token"));
    }

    #[test]
    fn job_economy_routes_implementer_and_reviewer_separately_and_deterministically() {
        let mut implementer = request(vec![
            candidate(
                "expensive",
                false,
                Some(32_000),
                Some(true),
                Some(5),
                Some(20),
            ),
            candidate("cheap", false, Some(32_000), Some(true), Some(2), Some(200)),
        ]);
        implementer.pace = Pace::Economy;
        let mut reviewer = request(vec![
            candidate(
                "expensive",
                false,
                Some(32_000),
                Some(true),
                Some(5),
                Some(20),
            ),
            candidate("cheap", false, Some(32_000), Some(true), Some(2), Some(200)),
        ]);
        reviewer.pace = Pace::Balanced;
        reviewer.manual_provider_id = Some("expensive".into());
        reviewer.manual_model_id = Some("model".into());
        let facts = vec![
            facts("expensive", "model", false, 100),
            facts("cheap", "model", false, 1),
        ];
        let result = route_job(JobRoutingRequest {
            implementer: AssignmentRequest {
                task_id: "implement".into(),
                request: implementer,
                required_runtime_id: None,
                max_input_tokens: Some(100),
                max_output_tokens: Some(100),
            },
            reviewer: Some(AssignmentRequest {
                task_id: "review".into(),
                request: reviewer,
                required_runtime_id: None,
                max_input_tokens: Some(100),
                max_output_tokens: Some(100),
            }),
            candidate_facts: facts,
        });
        assert_eq!(
            result.implementer.decision.selected_provider_id.as_deref(),
            Some("cheap")
        );
        assert_eq!(
            result
                .reviewer
                .as_ref()
                .unwrap()
                .decision
                .selected_provider_id
                .as_deref(),
            Some("expensive")
        );
        assert!(result.reviewer.as_ref().unwrap().explicit_override);
    }
}
