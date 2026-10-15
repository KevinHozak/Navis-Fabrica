use crate::usage::{UsageCategory, UsageSignal, UsageStatus};
use serde::{Deserialize, Serialize};

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
    fn context_floor(self) -> u64 {
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
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Request {
    pub complexity: Complexity,
    pub pace: Pace,
    pub require_tools: bool,
    pub local_only: bool,
    pub desired_completion_minutes: Option<u32>,
    pub manual_provider_id: Option<String>,
    pub manual_model_id: Option<String>,
    pub candidates: Vec<Candidate>,
    pub usage_signals: Vec<UsageSignal>,
    pub now_ms: u128,
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
    let required_context_tokens = request.complexity.context_floor();
    let mut rows = request.candidates.iter().cloned().map(|candidate| {
        let mut reasons = Vec::new();
        let mut eligible = true;
        match candidate.context_tokens {
            Some(tokens) if tokens >= required_context_tokens => reasons.push(format!("Context {tokens} tokens meets the {required_context_tokens}-token complexity floor.")),
            Some(tokens) => { eligible = false; reasons.push(format!("Rejected: context is {tokens} tokens; {required_context_tokens} required for {:?} work.", request.complexity)); },
            None => { eligible = false; reasons.push("Rejected: model context capacity is unknown.".into()); },
        }
        if request.require_tools {
            match candidate.supports_tools {
                Some(true) => reasons.push("Required tool support is reported.".into()),
                Some(false) => { eligible = false; reasons.push("Rejected: model reports no tool support.".into()); },
                None => { eligible = false; reasons.push("Rejected: tool support is unknown.".into()); },
            }
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
        }
    }

    fn request(candidates: Vec<Candidate>) -> Request {
        Request {
            complexity: Complexity::Standard,
            pace: Pace::Balanced,
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
}
