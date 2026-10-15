use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, HashSet};
use std::io::Read;
use std::time::Duration;

pub const JEV_MODEL: &str = "jev-1.14.0";
pub const JEV_SYSTEMONE_URL: &str = "https://api.typesafe.ai/v1/systemone";
pub const MAX_SUMMARY_BYTES: usize = 1_024;
pub const MAX_CAPABILITIES: usize = 8;
pub const MAX_ROUTES: usize = 32;
pub const MAX_REQUEST_BYTES: usize = 8 * 1024;
pub const MAX_RESPONSE_BYTES: usize = 32 * 1024;
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RequiredCapability {
    Tools,
    Reasoning,
    LongContext,
    LocalExecution,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EligibleRoute {
    pub id: String,
    pub provider: String,
    pub model: String,
    pub context_tokens: Option<u32>,
    pub supports_tools: Option<bool>,
    pub is_local: bool,
}

/// Deliberately excludes repository paths, files, diffs, credentials, and issue bodies.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TaskTriageInput {
    pub task_summary: String,
    pub required_capabilities: Vec<RequiredCapability>,
    pub eligible_routes: Vec<EligibleRoute>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JevChoice {
    pub route_id: String,
    /// Probability for each submitted route. `confidence` is the selected route's probability.
    pub probabilities: BTreeMap<String, f64>,
    pub confidence: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriageError {
    InvalidRequest,
    RequestTooLarge,
    InvalidResponse,
    UnknownRoute,
    InvalidProbabilities,
    Unauthorized,
    RateLimited,
    Unavailable,
}

impl TriageError {
    pub fn sanitized_message(self) -> &'static str {
        match self {
            Self::InvalidRequest => "The Jev triage request is invalid.",
            Self::RequestTooLarge => "The Jev triage request exceeds its size limit.",
            Self::InvalidResponse => "Jev returned an invalid triage response.",
            Self::UnknownRoute => "Jev selected a route that was not eligible.",
            Self::InvalidProbabilities => "Jev returned invalid route probabilities.",
            Self::Unauthorized => "TypeSafe rejected the saved API key.",
            Self::RateLimited => "TypeSafe rate limited the triage request.",
            Self::Unavailable => "The Jev triage request failed or timed out.",
        }
    }
}

fn valid_route_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 96
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_.:/".contains(&byte))
}

fn valid_label(value: &str) -> bool {
    !value.trim().is_empty() && value.len() <= 80 && !value.chars().any(char::is_control)
}

fn validate_input(input: &TaskTriageInput) -> Result<(), TriageError> {
    if input.task_summary.trim().is_empty()
        || input.task_summary.len() > MAX_SUMMARY_BYTES
        || input.task_summary.chars().any(char::is_control)
        || input.required_capabilities.len() > MAX_CAPABILITIES
        || input.eligible_routes.len() < 2
        || input.eligible_routes.len() > MAX_ROUTES
    {
        return Err(TriageError::InvalidRequest);
    }

    let mut capabilities = HashSet::new();
    if input
        .required_capabilities
        .iter()
        .any(|capability| !capabilities.insert(*capability))
    {
        return Err(TriageError::InvalidRequest);
    }

    let mut route_ids = HashSet::new();
    if input.eligible_routes.iter().any(|route| {
        !valid_route_id(&route.id)
            || !valid_label(&route.provider)
            || !valid_label(&route.model)
            || !route_ids.insert(route.id.as_str())
    }) {
        return Err(TriageError::InvalidRequest);
    }
    Ok(())
}

/// Build the minimal TypeSafe System One Choice payload. No arbitrary JSON or issue body is accepted.
pub fn build_request(input: &TaskTriageInput) -> Result<Value, TriageError> {
    validate_input(input)?;
    let mut criteria = Map::new();
    for route in &input.eligible_routes {
        let mut metadata = format!("{} / {}", route.provider, route.model);
        if let Some(context) = route.context_tokens {
            metadata.push_str(&format!("; context {context} tokens"));
        }
        if let Some(supports_tools) = route.supports_tools {
            metadata.push_str(if supports_tools {
                "; tool support available"
            } else {
                "; tool support unavailable"
            });
        }
        metadata.push_str(if route.is_local {
            "; local"
        } else {
            "; remote"
        });
        criteria.insert(route.id.clone(), Value::String(metadata));
    }

    let payload = json!({
        "model": JEV_MODEL,
        "state": {
            "taskSummary": input.task_summary,
            "requiredCapabilities": input.required_capabilities,
        },
        "questions": {
            "route": {
                "type": "choice",
                "instructions": "Which eligible provider and model route best fits this task and its required capabilities?",
                "criteria": criteria,
            }
        }
    });
    if serde_json::to_vec(&payload)
        .map_err(|_| TriageError::InvalidRequest)?
        .len()
        > MAX_REQUEST_BYTES
    {
        return Err(TriageError::RequestTooLarge);
    }
    Ok(payload)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SystemOneResponse {
    model: String,
    answers: Answers,
    #[serde(default)]
    usage: Option<Usage>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Answers {
    route: ChoiceAnswer,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ChoiceAnswer {
    #[serde(rename = "type")]
    answer_type: String,
    choice: String,
    probabilities: BTreeMap<String, f64>,
    #[serde(default)]
    confidence: Option<f64>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Usage {
    input_tokens: u64,
    output_tokens: u64,
}

pub fn parse_response(body: &[u8], input: &TaskTriageInput) -> Result<JevChoice, TriageError> {
    if body.is_empty() || body.len() > MAX_RESPONSE_BYTES {
        return Err(TriageError::InvalidResponse);
    }
    validate_input(input)?;
    let response: SystemOneResponse =
        serde_json::from_slice(body).map_err(|_| TriageError::InvalidResponse)?;
    if response.model != JEV_MODEL {
        return Err(TriageError::InvalidResponse);
    }
    let _usage = response
        .usage
        .map(|usage| (usage.input_tokens, usage.output_tokens));
    let answer = response.answers.route;
    if answer.answer_type != "choice" {
        return Err(TriageError::InvalidResponse);
    }
    if !input
        .eligible_routes
        .iter()
        .any(|route| route.id == answer.choice)
    {
        return Err(TriageError::UnknownRoute);
    }

    let eligible: HashSet<&str> = input
        .eligible_routes
        .iter()
        .map(|route| route.id.as_str())
        .collect();
    if answer.probabilities.len() != eligible.len()
        || answer
            .probabilities
            .keys()
            .any(|route_id| !eligible.contains(route_id.as_str()))
        || answer
            .probabilities
            .values()
            .any(|probability| !probability.is_finite() || !(0.0..=1.0).contains(probability))
    {
        return Err(TriageError::InvalidProbabilities);
    }
    let total: f64 = answer.probabilities.values().sum();
    if (total - 1.0).abs() > 0.01 {
        return Err(TriageError::InvalidProbabilities);
    }
    let confidence = *answer
        .probabilities
        .get(&answer.choice)
        .ok_or(TriageError::InvalidProbabilities)?;
    // TypeSafe requires the selected choice to have the highest probability.
    // Equality is intentional: any choice tied for the maximum is valid.
    let highest_probability = answer
        .probabilities
        .values()
        .copied()
        .fold(f64::NEG_INFINITY, f64::max);
    if confidence < highest_probability {
        return Err(TriageError::InvalidProbabilities);
    }
    if answer
        .confidence
        .is_some_and(|value| !value.is_finite() || !(0.0..=1.0).contains(&value))
    {
        return Err(TriageError::InvalidProbabilities);
    }

    Ok(JevChoice {
        route_id: answer.choice,
        probabilities: answer.probabilities,
        confidence,
    })
}

/// Testable transport boundary. The closure must return an already size-bounded JSON response.
pub fn triage_with<F>(input: &TaskTriageInput, request: F) -> Result<JevChoice, TriageError>
where
    F: FnOnce(&Value) -> Result<Vec<u8>, TriageError>,
{
    let payload = build_request(input)?;
    let body = request(&payload)?;
    parse_response(&body, input)
}

pub fn http_triage(input: &TaskTriageInput, api_key: &str) -> Result<JevChoice, TriageError> {
    if api_key.trim().is_empty() || api_key.len() > 4096 || api_key.chars().any(char::is_control) {
        return Err(TriageError::Unavailable);
    }
    let payload = build_request(input)?;
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(3))
        .timeout(REQUEST_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| TriageError::Unavailable)?;
    let response = client
        .post(JEV_SYSTEMONE_URL)
        .bearer_auth(api_key)
        .json(&payload)
        .send()
        .map_err(|_| TriageError::Unavailable)?;
    if !response.status().is_success() {
        return Err(match response.status().as_u16() {
            401 | 403 => TriageError::Unauthorized,
            429 => TriageError::RateLimited,
            _ => TriageError::Unavailable,
        });
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
    {
        return Err(TriageError::InvalidResponse);
    }
    let mut body = Vec::new();
    response
        .take(MAX_RESPONSE_BYTES as u64 + 1)
        .read_to_end(&mut body)
        .map_err(|_| TriageError::Unavailable)?;
    if body.len() > MAX_RESPONSE_BYTES {
        return Err(TriageError::InvalidResponse);
    }
    parse_response(&body, input)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input() -> TaskTriageInput {
        TaskTriageInput {
            task_summary: "Add bounded project scanning".into(),
            required_capabilities: vec![RequiredCapability::Tools],
            eligible_routes: vec![
                EligibleRoute {
                    id: "codex:gpt-6.1-sol".into(),
                    provider: "Codex".into(),
                    model: "gpt-6.1-sol".into(),
                    context_tokens: Some(32_000),
                    supports_tools: Some(true),
                    is_local: false,
                },
                EligibleRoute {
                    id: "ollama:qwen".into(),
                    provider: "Ollama".into(),
                    model: "qwen".into(),
                    context_tokens: Some(16_000),
                    supports_tools: Some(true),
                    is_local: true,
                },
            ],
        }
    }

    fn response(choice: &str, probabilities: Value) -> Vec<u8> {
        serde_json::to_vec(&json!({
            "model": JEV_MODEL,
            "answers": { "route": {
                "type": "choice",
                "choice": choice,
                "probabilities": probabilities,
                "confidence": 0.91,
            }},
            "usage": { "input_tokens": 30, "output_tokens": 5 },
        }))
        .unwrap()
    }

    #[test]
    fn builds_small_choice_request_with_only_allowed_fields() {
        let input = input();
        let payload = build_request(&input).unwrap();
        assert_eq!(payload["model"], JEV_MODEL);
        assert_eq!(payload["state"]["taskSummary"], input.task_summary);
        assert_eq!(
            payload["questions"]["route"]["criteria"]["codex:gpt-6.1-sol"],
            "Codex / gpt-6.1-sol; context 32000 tokens; tool support available; remote"
        );
        let encoded = serde_json::to_string(&payload).unwrap();
        for forbidden in [
            "repository",
            "file",
            "diff",
            "credential",
            "acceptanceCriteria",
            "issueBody",
        ] {
            assert!(!encoded.to_lowercase().contains(&forbidden.to_lowercase()));
        }
        assert!(encoded.len() <= MAX_REQUEST_BYTES);
        let extra_input_field = json!({
            "taskSummary": "summary",
            "requiredCapabilities": [],
            "eligibleRoutes": [],
            "repositoryFiles": ["secret.rs"],
        });
        assert!(serde_json::from_value::<TaskTriageInput>(extra_input_field).is_err());
    }

    #[test]
    fn parses_valid_choice_and_selected_probability_as_confidence() {
        let input = input();
        let choice = parse_response(
            &response(
                "codex:gpt-6.1-sol",
                json!({"codex:gpt-6.1-sol": 0.9, "ollama:qwen": 0.1}),
            ),
            &input,
        )
        .unwrap();
        assert_eq!(choice.route_id, "codex:gpt-6.1-sol");
        assert_eq!(choice.confidence, 0.9);

        let tied_choice = parse_response(
            &response(
                "ollama:qwen",
                json!({"codex:gpt-6.1-sol": 0.5, "ollama:qwen": 0.5}),
            ),
            &input,
        )
        .unwrap();
        assert_eq!(tied_choice.route_id, "ollama:qwen");
        assert_eq!(tied_choice.confidence, 0.5);
    }

    #[test]
    fn rejects_unknown_or_malformed_choices() {
        let input = input();
        assert_eq!(
            parse_response(&response("unknown", json!({"unknown": 1.0})), &input),
            Err(TriageError::UnknownRoute)
        );
        assert_eq!(
            parse_response(b"{}", &input),
            Err(TriageError::InvalidResponse)
        );
        let mut malformed = json!({
            "model": JEV_MODEL,
            "answers": {"route": {"type": "score", "choice": "codex:gpt-6.1-sol", "probabilities": {"codex:gpt-6.1-sol": 0.9, "ollama:qwen": 0.1}}}
        });
        assert_eq!(
            parse_response(&serde_json::to_vec(&malformed).unwrap(), &input),
            Err(TriageError::InvalidResponse)
        );
        malformed["answers"]["route"]["type"] = json!("choice");
        malformed["answers"]["route"]
            .as_object_mut()
            .unwrap()
            .remove("probabilities");
        assert_eq!(
            parse_response(&serde_json::to_vec(&malformed).unwrap(), &input),
            Err(TriageError::InvalidResponse)
        );
    }

    #[test]
    fn rejects_invalid_probability_sets() {
        let input = input();
        for probs in [
            json!({"codex:gpt-6.1-sol": 0.9}),
            json!({"codex:gpt-6.1-sol": 0.9, "ollama:qwen": 0.2}),
            json!({"codex:gpt-6.1-sol": -0.1, "ollama:qwen": 1.1}),
            json!({"codex:gpt-6.1-sol": 0.5, "ollama:qwen": 0.5, "extra": 0.0}),
        ] {
            assert_eq!(
                parse_response(&response("codex:gpt-6.1-sol", probs), &input),
                Err(TriageError::InvalidProbabilities)
            );
        }
        assert_eq!(
            parse_response(
                &response(
                    "ollama:qwen",
                    json!({"codex:gpt-6.1-sol": 0.9, "ollama:qwen": 0.1}),
                ),
                &input
            ),
            Err(TriageError::InvalidProbabilities)
        );
        let non_finite = br#"{"model":"jev-1.14.0","answers":{"route":{"type":"choice","choice":"codex:gpt-6.1-sol","probabilities":{"codex:gpt-6.1-sol":NaN,"ollama:qwen":0.1}}}}"#;
        assert_eq!(
            parse_response(non_finite, &input),
            Err(TriageError::InvalidResponse)
        );
    }

    #[test]
    fn enforces_input_and_response_bounds_and_sanitizes_transport_errors() {
        let mut oversized_input = input();
        oversized_input.task_summary = "x".repeat(MAX_SUMMARY_BYTES + 1);
        assert_eq!(
            build_request(&oversized_input),
            Err(TriageError::InvalidRequest)
        );

        let input = input();
        assert_eq!(
            parse_response(&vec![b' '; MAX_RESPONSE_BYTES + 1], &input),
            Err(TriageError::InvalidResponse)
        );
        assert_eq!(
            triage_with(&input, |_| Err(TriageError::Unavailable)),
            Err(TriageError::Unavailable)
        );
        assert_eq!(
            TriageError::Unavailable.sanitized_message(),
            "The Jev triage request failed or timed out."
        );
        assert_eq!(REQUEST_TIMEOUT, Duration::from_secs(10));
    }

    #[test]
    fn rejects_a_serialized_request_over_the_payload_limit() {
        let mut input = input();
        input.eligible_routes = (0..MAX_ROUTES)
            .map(|index| EligibleRoute {
                id: format!("{}-{index}", "r".repeat(92)),
                provider: "p".repeat(80),
                model: "m".repeat(80),
                context_tokens: Some(u32::MAX),
                supports_tools: Some(true),
                is_local: false,
            })
            .collect();
        assert_eq!(build_request(&input), Err(TriageError::RequestTooLarge));
    }
}
