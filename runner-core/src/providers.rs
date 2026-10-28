use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ProviderKind {
    Codex,
    Antigravity,
    Ollama,
    OpenRouter,
    Custom,
}

impl ProviderKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Codex => "Codex",
            Self::Antigravity => "Antigravity",
            Self::Ollama => "Ollama",
            Self::OpenRouter => "OpenRouter",
            Self::Custom => "Custom runtime",
        }
    }

    pub fn default_executable(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Antigravity => "agy",
            Self::Ollama => "aider",
            Self::OpenRouter => "aider",
            Self::Custom => "",
        }
    }

    pub fn telemetry_id(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Antigravity => "antigravity",
            Self::Ollama => "ollama",
            Self::OpenRouter => "openrouter",
            Self::Custom => "custom",
        }
    }

    pub fn supports_workflow_run(self) -> bool {
        matches!(
            self,
            Self::Codex | Self::Antigravity | Self::Ollama | Self::OpenRouter
        )
    }
}

pub const OPENROUTER_API_BASE: &str = "https://openrouter.ai/api/v1";
pub const OPENROUTER_MAX_MODELS: usize = 500;
pub const OPENROUTER_MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

pub const TYPESAFE_API_BASE: &str = "https://api.typesafe.ai/v1";
pub const TYPESAFE_JEV_MODEL: &str = "jev-1.14.0";
pub const TYPESAFE_MAX_RESPONSE_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TypeSafeProfile {
    pub id: String,
    pub enabled: bool,
    pub credential_ref: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TypeSafeProbe {
    pub state: String,
    pub model: String,
    pub note: String,
}

pub fn typesafe_credential_target(profile_id: &str) -> Result<String, String> {
    if profile_id.is_empty()
        || profile_id.len() > 80
        || !profile_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_".contains(c))
    {
        return Err("The TypeSafe profile ID is invalid.".into());
    }
    Ok(format!("navis-fabrica-typesafe-{profile_id}"))
}

pub fn typesafe_credential_reference_matches(profile_id: &str, reference: &str) -> bool {
    typesafe_credential_target(profile_id)
        .map(|target| reference == format!("credential-store:{target}"))
        .unwrap_or(false)
}

pub fn valid_typesafe_key(value: &str) -> bool {
    let value = value.trim();
    !value.is_empty() && value.len() <= 4096 && !value.chars().any(char::is_control)
}

pub fn typesafe_probe_with<F>(
    enabled: bool,
    profile_id: &str,
    credential_ref: Option<&str>,
    api_key: Option<&str>,
    request: F,
) -> TypeSafeProbe
where
    F: FnOnce(&str) -> Result<Value, u16>,
{
    let result = |state: &str, note: &str| TypeSafeProbe {
        state: state.into(),
        model: TYPESAFE_JEV_MODEL.into(),
        note: note.into(),
    };
    if !enabled {
        return result(
            "disabled",
            "Enable the TypeSafe profile to check its connection.",
        );
    }
    let Some(reference) = credential_ref else {
        return result(
            "unconfigured",
            "Save a TypeSafe API key in Windows Credential Manager first.",
        );
    };
    if !typesafe_credential_reference_matches(profile_id, reference) {
        return result(
            "unconfigured",
            "The TypeSafe credential reference is invalid for this profile. Save the key again.",
        );
    }
    let Some(key) = api_key.filter(|value| valid_typesafe_key(value)) else {
        return result(
            "unconfigured",
            "The TypeSafe credential is unavailable. Save the key again.",
        );
    };
    match request(key) {
        Ok(value) => {
            let catalog_is_valid = value.get("models").and_then(Value::as_array).is_some();
            if catalog_is_valid {
                result(
                    "healthy",
                    "TypeSafe is reachable. The configured Jev version is pinned; versioned IDs may not appear in this alias catalog.",
                )
            } else {
                result(
                    "unavailable",
                    "TypeSafe returned an invalid model catalog response.",
                )
            }
        }
        Err(401 | 403) => result(
            "authenticationFailed",
            "TypeSafe rejected the saved API key. Check its validity and access.",
        ),
        Err(429) => result(
            "rateLimited",
            "TypeSafe rate limited the health check. Try again later.",
        ),
        Err(_) => result(
            "unavailable",
            "TypeSafe health check failed or timed out. The response was omitted to protect credentials.",
        ),
    }
}

fn blocking_http_client_builder() -> reqwest::blocking::ClientBuilder {
    crate::initialize_crypto_provider();
    reqwest::blocking::Client::builder()
}

#[cfg(test)]
mod tls_provider_tests {
    #[test]
    fn direct_provider_clients_build_repeatedly_without_network() {
        for _ in 0..2 {
            super::blocking_http_client_builder()
                .build()
                .expect("blocking provider HTTP client should initialize TLS");
        }
    }
}

pub fn typesafe_http_probe(profile: &TypeSafeProfile, api_key: Option<&str>) -> TypeSafeProbe {
    typesafe_probe_with(
        profile.enabled,
        &profile.id,
        profile.credential_ref.as_deref(),
        api_key,
        |key| {
            let client = blocking_http_client_builder()
                .connect_timeout(Duration::from_secs(3))
                .timeout(Duration::from_secs(10))
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .map_err(|_| 0u16)?;
            let response = client
                .get(format!("{TYPESAFE_API_BASE}/models"))
                .bearer_auth(key)
                .send()
                .map_err(|_| 0u16)?;
            if !response.status().is_success() {
                return Err(response.status().as_u16());
            }
            if response
                .content_length()
                .is_some_and(|length| length > TYPESAFE_MAX_RESPONSE_BYTES as u64)
            {
                return Err(0);
            }
            let mut bytes = Vec::new();
            response
                .take(TYPESAFE_MAX_RESPONSE_BYTES as u64 + 1)
                .read_to_end(&mut bytes)
                .map_err(|_| 0u16)?;
            if bytes.len() > TYPESAFE_MAX_RESPONSE_BYTES {
                return Err(0);
            }
            serde_json::from_slice(&bytes).map_err(|_| 0u16)
        },
    )
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenRouterModel {
    pub id: String,
    pub canonical_slug: Option<String>,
    pub name: String,
    pub context_length: Option<u64>,
    pub prompt_price: Option<String>,
    pub completion_price: Option<String>,
    pub input_modalities: Option<Vec<String>>,
    pub output_modalities: Option<Vec<String>>,
    pub supported_parameters: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OpenRouterEndpoint {
    pub model_id: String,
    pub tag: String,
    pub context_length: Option<u64>,
    pub max_prompt_tokens: Option<u64>,
    pub max_completion_tokens: Option<u64>,
    pub prompt_price: Option<String>,
    pub completion_price: Option<String>,
    pub request_price: Option<String>,
    pub status: Option<i64>,
    pub zdr: Option<bool>,
    pub supported_parameters: Option<Vec<String>>,
}

pub fn parse_openrouter_endpoints(value: &Value, model_id: &str) -> Vec<OpenRouterEndpoint> {
    value
        .get("data")
        .and_then(Value::as_object)
        .and_then(|data| data.get("endpoints"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|endpoint| {
            let tag = endpoint.get("tag")?.as_str()?.trim();
            let endpoint_model = endpoint.get("model_id").and_then(Value::as_str)?;
            if endpoint_model != model_id || !valid_endpoint_tag(tag) {
                return None;
            }
            let pricing = endpoint.get("pricing");
            Some(OpenRouterEndpoint {
                model_id: endpoint_model.to_owned(),
                tag: tag.to_owned(),
                context_length: endpoint.get("context_length").and_then(Value::as_u64),
                max_prompt_tokens: endpoint.get("max_prompt_tokens").and_then(Value::as_u64),
                max_completion_tokens: endpoint
                    .get("max_completion_tokens")
                    .and_then(Value::as_u64),
                prompt_price: pricing
                    .and_then(|v| v.get("prompt"))
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                completion_price: pricing
                    .and_then(|v| v.get("completion"))
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                request_price: pricing
                    .and_then(|v| v.get("request"))
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                status: endpoint.get("status").and_then(Value::as_i64),
                zdr: endpoint.get("zdr").and_then(Value::as_bool),
                supported_parameters: endpoint
                    .get("supported_parameters")
                    .and_then(Value::as_array)
                    .map(|values| {
                        values
                            .iter()
                            .filter_map(Value::as_str)
                            .map(str::to_owned)
                            .collect()
                    }),
            })
        })
        .take(256)
        .collect()
}

/// Exact endpoint identities in OpenRouter's current ZDR inventory. The
/// response is consumed in memory; no provider payload is retained.
pub fn parse_openrouter_zdr_endpoints(
    value: &Value,
) -> Option<std::collections::BTreeSet<(String, String)>> {
    let rows = value.get("data")?.as_array()?;
    if rows.len() > 10_000 {
        return None;
    }
    let mut endpoints = std::collections::BTreeSet::new();
    for row in rows {
        let model_id = row.get("model_id")?.as_str()?.trim();
        let tag = row.get("tag")?.as_str()?.trim();
        if model_id.is_empty() || model_id.len() > 160 || !valid_endpoint_tag(tag) {
            return None;
        }
        endpoints.insert((model_id.to_owned(), tag.to_owned()));
    }
    Some(endpoints)
}

pub fn openrouter_zdr_endpoints_http_probe_with_timeout(
    api_key: &str,
    timeout: Duration,
) -> Result<std::collections::BTreeSet<(String, String)>, String> {
    if !valid_openrouter_key(api_key) {
        return Err("OpenRouter key is invalid.".into());
    }
    let client = blocking_http_client_builder()
        .connect_timeout(timeout.min(Duration::from_secs(3)))
        .timeout(timeout)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| "OpenRouter ZDR metadata client is unavailable.".to_string())?;
    let response = client
        .get(format!("{OPENROUTER_API_BASE}/endpoints/zdr"))
        .bearer_auth(api_key)
        .send()
        .map_err(|_| "OpenRouter ZDR endpoint inventory could not be refreshed.".to_string())?;
    if response.status().as_u16() == 429 {
        return Err("OpenRouter rate limit was reached during ZDR inventory refresh.".into());
    }
    if !response.status().is_success() {
        return Err("OpenRouter ZDR endpoint inventory was rejected.".into());
    }
    if response
        .content_length()
        .is_some_and(|length| length > OPENROUTER_MAX_RESPONSE_BYTES as u64)
    {
        return Err("OpenRouter ZDR endpoint inventory exceeded its response limit.".into());
    }
    let mut bytes = Vec::new();
    response
        .take(OPENROUTER_MAX_RESPONSE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "OpenRouter ZDR endpoint inventory could not be read.".to_string())?;
    if bytes.len() > OPENROUTER_MAX_RESPONSE_BYTES {
        return Err("OpenRouter ZDR endpoint inventory exceeded its response limit.".into());
    }
    let value: Value = serde_json::from_slice(&bytes)
        .map_err(|_| "OpenRouter ZDR endpoint inventory was malformed.".to_string())?;
    parse_openrouter_zdr_endpoints(&value)
        .ok_or_else(|| "OpenRouter ZDR endpoint inventory contained invalid records.".into())
}

fn valid_endpoint_tag(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value.split('/').all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
        })
}

pub fn openrouter_endpoints_http_probe_with_timeout(
    api_key: &str,
    model_id: &str,
    timeout: Duration,
) -> Result<Vec<OpenRouterEndpoint>, String> {
    if !valid_openrouter_key(api_key)
        || !model_id.contains('/')
        || model_id.len() > 160
        || model_id
            .chars()
            .any(|ch| !(ch.is_ascii_alphanumeric() || matches!(ch, '/' | '-' | '_' | '.' | ':')))
    {
        return Err("OpenRouter endpoint request identity is invalid.".into());
    }
    let client = blocking_http_client_builder()
        .connect_timeout(timeout.min(Duration::from_secs(3)))
        .timeout(timeout)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| "OpenRouter endpoint metadata client is unavailable.".to_string())?;
    let response = client
        .get(format!("{OPENROUTER_API_BASE}/models/{model_id}/endpoints"))
        .bearer_auth(api_key)
        .send()
        .map_err(|_| "OpenRouter endpoint metadata could not be refreshed.".to_string())?;
    if response.status().as_u16() == 429 {
        return Err("OpenRouter rate limit was reached during endpoint refresh.".into());
    }
    if !response.status().is_success() {
        return Err("OpenRouter endpoint metadata was rejected.".into());
    }
    if response
        .content_length()
        .is_some_and(|length| length > OPENROUTER_MAX_RESPONSE_BYTES as u64)
    {
        return Err("OpenRouter endpoint metadata exceeded its response limit.".into());
    }
    let mut bytes = Vec::new();
    response
        .take(OPENROUTER_MAX_RESPONSE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "OpenRouter endpoint metadata could not be read.".to_string())?;
    if bytes.len() > OPENROUTER_MAX_RESPONSE_BYTES {
        return Err("OpenRouter endpoint metadata exceeded its response limit.".into());
    }
    let value: Value = serde_json::from_slice(&bytes)
        .map_err(|_| "OpenRouter endpoint metadata was malformed.".to_string())?;
    let endpoints = parse_openrouter_endpoints(&value, model_id);
    if endpoints.is_empty() {
        return Err("No exact OpenRouter endpoint is available for this model.".into());
    }
    Ok(endpoints)
}

pub fn validate_openrouter_free_endpoint(
    api_key: &str,
    model_id: &str,
    endpoint_tag: &str,
    minimum_context: u64,
    require_zdr: bool,
    timeout: Duration,
) -> Result<bool, String> {
    let deadline = Instant::now() + timeout;
    let endpoints = openrouter_endpoints_http_probe_with_timeout(
        api_key,
        model_id,
        deadline.saturating_duration_since(Instant::now()),
    )?;
    let zdr_inventory = if require_zdr {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err("OpenRouter endpoint validation deadline expired.".into());
        }
        Some(openrouter_zdr_endpoints_http_probe_with_timeout(
            api_key, remaining,
        )?)
    } else {
        None
    };
    let matches = endpoints
        .iter()
        .filter(|endpoint| {
            endpoint.tag == endpoint_tag
                && endpoint.model_id == model_id
                && endpoint.status == Some(0)
                && endpoint
                    .context_length
                    .is_some_and(|context| context >= minimum_context)
                && endpoint
                    .prompt_price
                    .as_deref()
                    .and_then(openrouter_price_micros_per_million)
                    == Some(0)
                && endpoint
                    .completion_price
                    .as_deref()
                    .and_then(openrouter_price_micros_per_million)
                    == Some(0)
                && endpoint
                    .request_price
                    .as_deref()
                    .is_none_or(|price| openrouter_price_micros_per_million(price) == Some(0))
                && (!require_zdr
                    || (endpoint.zdr != Some(false)
                        && zdr_inventory.as_ref().is_some_and(|inventory| {
                            inventory.contains(&(model_id.to_owned(), endpoint.tag.clone()))
                        })))
        })
        .count();
    Ok(matches == 1)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenRouterProbe {
    pub state: String,
    pub models: Vec<OpenRouterModel>,
    pub note: String,
}

pub fn parse_openrouter_models(value: &Value) -> Vec<OpenRouterModel> {
    value
        .get("data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| {
            let id = item.get("id")?.as_str()?.trim();
            let name = item.get("name")?.as_str()?.trim();
            if id.is_empty()
                || id.len() > 160
                || name.is_empty()
                || name.len() > 160
                || id.chars().any(char::is_control)
                || name.chars().any(char::is_control)
            {
                return None;
            }
            let architecture = item.get("architecture");
            let pricing = item.get("pricing");
            Some(OpenRouterModel {
                id: id.to_owned(),
                canonical_slug: item
                    .get("canonical_slug")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                name: name.to_owned(),
                context_length: item.get("context_length").and_then(Value::as_u64),
                prompt_price: pricing
                    .and_then(|v| v.get("prompt"))
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                completion_price: pricing
                    .and_then(|v| v.get("completion"))
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                input_modalities: architecture
                    .and_then(|v| v.get("input_modalities"))
                    .and_then(string_array),
                output_modalities: architecture
                    .and_then(|v| v.get("output_modalities"))
                    .and_then(string_array),
                supported_parameters: item.get("supported_parameters").and_then(string_array),
            })
        })
        .take(OPENROUTER_MAX_MODELS)
        .collect()
}

/// Convert an OpenRouter catalog price in USD per token into conservative
/// integer micro-USD per million tokens. Decimal arithmetic avoids rounding a
/// provider quote down; malformed, exponent, negative, and overflowing values
/// remain unknown and must not be used for paid admission.
pub fn openrouter_price_micros_per_million(price: &str) -> Option<u64> {
    let price = price.trim();
    let mut parts = price.split('.');
    let whole = parts.next()?;
    let fraction = parts.next().unwrap_or("");
    if parts.next().is_some()
        || whole.is_empty()
        || !whole.bytes().all(|byte| byte.is_ascii_digit())
        || !fraction.bytes().all(|byte| byte.is_ascii_digit())
        || (price.contains('.') && fraction.is_empty())
    {
        return None;
    }
    let digits = format!("{whole}{fraction}");
    if digits.len() > 38 {
        return None;
    }
    let mantissa = digits.parse::<u128>().ok()?;
    let scale = fraction.len();
    let micros = if scale <= 12 {
        mantissa.checked_mul(10u128.checked_pow((12 - scale) as u32)?)?
    } else {
        let divisor = 10u128.checked_pow((scale - 12) as u32)?;
        mantissa / divisor + u128::from(mantissa % divisor != 0)
    };
    u64::try_from(micros).ok()
}

fn string_array(value: &Value) -> Option<Vec<String>> {
    Some(
        value
            .as_array()?
            .iter()
            .map(|item| item.as_str().map(str::to_owned))
            .collect::<Option<Vec<_>>>()?,
    )
}

pub fn openrouter_probe_with<F>(api_key: &str, request: F) -> OpenRouterProbe
where
    F: FnOnce(&str) -> Result<Value, u16>,
{
    if !valid_openrouter_key(api_key) {
        return OpenRouterProbe {
            state: "authenticationRequired".into(),
            models: Vec::new(),
            note: "Save a valid OpenRouter API key in Windows Credential Manager first.".into(),
        };
    }
    match request(api_key) {
        Ok(value) => {
            let models = parse_openrouter_models(&value);
            if models.is_empty() { return OpenRouterProbe { state: "unavailable".into(), models, note: "OpenRouter returned no valid model catalog entries.".into() }; }
            OpenRouterProbe { state: "ready".into(), note: format!("OpenRouter returned {} models. Missing metadata remains unknown.", models.len()), models }
        }
        Err(401 | 403) => OpenRouterProbe { state: "authenticationRequired".into(), models: Vec::new(), note: "OpenRouter rejected the saved API key. Check its validity and access.".into() },
        Err(429) => OpenRouterProbe { state: "rateLimited".into(), models: Vec::new(), note: "OpenRouter rate limited model discovery. Try again later.".into() },
        Err(_) => OpenRouterProbe { state: "unavailable".into(), models: Vec::new(), note: "OpenRouter model discovery failed. The response was omitted to protect credentials.".into() },
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct OpenRouterMetadataObservation {
    /// None means the key endpoint omitted a required execution-eligibility field.
    pub authenticated_for_execution: Option<bool>,
    pub models: Vec<OpenRouterModel>,
    /// Account-specific free-model request counters only; balances and spend
    /// metadata are intentionally never retained.
    pub free_model_daily_requests: Option<FreeModelDailyRequests>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FreeModelDailyRequests {
    pub used: u64,
    pub limit: u64,
    pub remaining: u64,
}

fn parse_free_model_daily_requests(data: &Value) -> Option<FreeModelDailyRequests> {
    let value = data.get("free_model_daily_requests")?.as_object()?;
    let used = value.get("used")?.as_u64()?;
    let limit = value.get("limit")?.as_u64()?;
    let remaining = value.get("remaining")?.as_u64()?;
    (used <= limit
        && remaining <= limit
        && used.checked_add(remaining) == Some(limit)
        && limit <= 1_000_000)
        .then_some(FreeModelDailyRequests {
            used,
            limit,
            remaining,
        })
}

/// Verify the current key separately from the user-scoped model catalog. Only
/// the two metadata endpoints are requested; their raw responses are consumed
/// in memory and never returned or persisted.
pub fn openrouter_metadata_probe_with<F>(
    api_key: &str,
    mut request: F,
) -> Result<OpenRouterMetadataObservation, String>
where
    F: FnMut(&str, &str) -> Result<Value, u16>,
{
    if !valid_openrouter_key(api_key) {
        return Ok(OpenRouterMetadataObservation {
            authenticated_for_execution: Some(false),
            models: Vec::new(),
            free_model_daily_requests: None,
        });
    }
    let key = match request("/key", api_key) {
        Ok(value) => value,
        Err(401 | 403) => {
            return Ok(OpenRouterMetadataObservation {
                authenticated_for_execution: Some(false),
                models: Vec::new(),
                free_model_daily_requests: None,
            });
        }
        Err(429) => return Err("OpenRouter rate limit was reached during metadata refresh.".into()),
        Err(_) => return Err("OpenRouter key metadata could not be verified.".into()),
    };
    let data = key
        .get("data")
        .and_then(Value::as_object)
        .ok_or_else(|| "OpenRouter key metadata response was invalid.".to_string())?;
    // Management and provisioning keys cannot be treated as completion keys.
    // Missing flags stay unknown; a successful HTTP status alone is not enough.
    let management = data.get("is_management_key").and_then(Value::as_bool);
    let provisioning = data.get("is_provisioning_key").and_then(Value::as_bool);
    let authenticated_for_execution = match (management, provisioning) {
        (Some(false), Some(false)) => true,
        (Some(true), _) | (_, Some(true)) => false,
        _ => {
            return Ok(OpenRouterMetadataObservation {
                authenticated_for_execution: None,
                models: Vec::new(),
                free_model_daily_requests: None,
            });
        }
    };
    if !authenticated_for_execution {
        return Ok(OpenRouterMetadataObservation {
            authenticated_for_execution: Some(false),
            models: Vec::new(),
            free_model_daily_requests: None,
        });
    }
    let catalog = request("/models/user", api_key).map_err(|status| {
        if status == 429 {
            "OpenRouter rate limit was reached during metadata refresh.".to_string()
        } else {
            "OpenRouter user-scoped model availability could not be verified.".to_string()
        }
    })?;
    let rows = catalog
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| "OpenRouter user-scoped model catalog response was invalid.".to_string())?;
    if rows.len() > OPENROUTER_MAX_MODELS {
        return Err(
            "OpenRouter user-scoped model inventory exceeds its verification bound.".into(),
        );
    }
    let models = parse_openrouter_models(&catalog);
    if models.len() != rows.len() {
        return Err("OpenRouter user-scoped model inventory contained invalid records.".into());
    }
    Ok(OpenRouterMetadataObservation {
        authenticated_for_execution: Some(true),
        models,
        free_model_daily_requests: parse_free_model_daily_requests(&key["data"]),
    })
}

pub fn openrouter_metadata_http_probe_with_timeout(
    api_key: &str,
    timeout: Duration,
) -> Result<OpenRouterMetadataObservation, String> {
    openrouter_metadata_http_probe_until(api_key, Instant::now() + timeout)
}

pub fn openrouter_metadata_http_probe_until(
    api_key: &str,
    deadline: Instant,
) -> Result<OpenRouterMetadataObservation, String> {
    let result = openrouter_metadata_probe_with(api_key, |path, key| {
        let timeout = deadline.saturating_duration_since(Instant::now());
        if timeout.is_zero() {
            return Err(0u16);
        }
        let client = blocking_http_client_builder()
            .connect_timeout(timeout.min(Duration::from_secs(3)))
            .timeout(timeout)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| 0u16)?;
        let response = client
            .get(format!("{OPENROUTER_API_BASE}{path}"))
            .bearer_auth(key)
            .send()
            .map_err(|_| 0u16)?;
        if !response.status().is_success() {
            return Err(response.status().as_u16());
        }
        if response
            .content_length()
            .is_some_and(|length| length > OPENROUTER_MAX_RESPONSE_BYTES as u64)
        {
            return Err(0);
        }
        let mut bytes = Vec::new();
        response
            .take(OPENROUTER_MAX_RESPONSE_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| 0u16)?;
        if bytes.len() > OPENROUTER_MAX_RESPONSE_BYTES {
            return Err(0);
        }
        serde_json::from_slice(&bytes).map_err(|_| 0u16)
    })?;
    if Instant::now() >= deadline {
        return Err("OpenRouter metadata refresh exceeded its dispatch deadline.".into());
    }
    Ok(result)
}

pub fn valid_openrouter_key(value: &str) -> bool {
    let value = value.trim();
    !value.is_empty() && value.len() <= 4096 && !value.chars().any(char::is_control)
}

pub fn openrouter_credential_target(provider_id: &str) -> Result<String, String> {
    if provider_id.is_empty()
        || provider_id.len() > 80
        || !provider_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_".contains(c))
    {
        return Err("The OpenRouter provider ID is invalid.".into());
    }
    Ok(format!("navis-fabrica-openrouter-{provider_id}"))
}

pub fn openrouter_credential_reference_matches(provider_id: &str, reference: &str) -> bool {
    openrouter_credential_target(provider_id)
        .map(|target| reference == format!("credential-store:{target}"))
        .unwrap_or(false)
}

pub fn aider_openrouter_arguments(
    model_id: &str,
    prompt_file: &str,
    test_command: Option<&str>,
    empty_env_file: &str,
) -> Vec<String> {
    let mut args = vec![
        "--model".into(),
        format!("openrouter/{model_id}"),
        "--message".into(),
        prompt_file.into(),
        "--yes".into(),
        "--no-auto-commits".into(),
        "--no-gitignore".into(),
        "--no-add-gitignore-files".into(),
        "--no-analytics".into(),
        "--no-check-update".into(),
        "--no-show-release-notes".into(),
        "--no-restore-chat-history".into(),
        "--no-suggest-shell-commands".into(),
        "--no-stream".into(),
        "--no-pretty".into(),
        "--env-file".into(),
        empty_env_file.into(),
    ];
    if let Some(command) = test_command.filter(|value| !value.trim().is_empty()) {
        args.extend(["--test-cmd".into(), command.into(), "--auto-test".into()]);
    }
    args
}

/// Arguments for OpenRouter-backed Aider through Fabrica's local gateway.
/// The OpenAI-compatible model IDs keep Aider's provider transport pointed at
/// the injected loopback endpoint; the gateway, not Aider, owns billing.
pub fn aider_paid_gateway_arguments(
    model_id: &str,
    message_file: &str,
    metadata_file: &str,
    model_settings_file: &str,
    config_file: &str,
    chat_history_file: &str,
    input_history_file: &str,
    empty_env_file: &str,
    test_command: Option<&str>,
    source_files: &[String],
) -> Vec<String> {
    let model = format!("openai/{model_id}");
    let mut args = vec![
        "--model".into(),
        model.clone(),
        "--weak-model".into(),
        model.clone(),
        "--editor-model".into(),
        model,
        "--edit-format".into(),
        "diff".into(),
        "--message-file".into(),
        message_file.into(),
        "--model-metadata-file".into(),
        metadata_file.into(),
        "--model-settings-file".into(),
        model_settings_file.into(),
        "--config".into(),
        config_file.into(),
        "--chat-history-file".into(),
        chat_history_file.into(),
        "--input-history-file".into(),
        input_history_file.into(),
        "--no-auto-commits".into(),
        "--no-git".into(),
        "--no-auto-lint".into(),
        "--no-gitignore".into(),
        "--no-add-gitignore-files".into(),
        "--no-analytics".into(),
        "--map-tokens".into(),
        "0".into(),
        "--no-check-update".into(),
        "--no-show-release-notes".into(),
        "--no-restore-chat-history".into(),
        "--no-suggest-shell-commands".into(),
        "--no-stream".into(),
        "--no-pretty".into(),
        // `--no-pretty` does not disable prompt_toolkit's interactive session
        // in Aider 0.86.2; headless Fabrica workers must opt out explicitly.
        "--no-fancy-input".into(),
        "--no-show-model-warnings".into(),
        "--no-browser".into(),
        "--env-file".into(),
        empty_env_file.into(),
    ];
    if let Some(command) = test_command.filter(|value| !value.trim().is_empty()) {
        args.extend(["--test-cmd".into(), command.into(), "--auto-test".into()]);
    } else {
        args.push("--no-auto-test".into());
    }
    args.extend(source_files.iter().cloned());
    args
}

pub fn openrouter_http_probe(api_key: &str) -> OpenRouterProbe {
    openrouter_http_probe_with_timeout(api_key, Duration::from_secs(15))
}

pub fn openrouter_http_probe_with_timeout(api_key: &str, timeout: Duration) -> OpenRouterProbe {
    openrouter_probe_with(api_key, |key| {
        let client = blocking_http_client_builder()
            .connect_timeout(timeout.min(Duration::from_secs(3)))
            .timeout(timeout)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| 0u16)?;
        let response = client
            .get(format!("{OPENROUTER_API_BASE}/models").as_str())
            .bearer_auth(key)
            .send()
            .map_err(|_| 0u16)?;
        if !response.status().is_success() {
            return Err(response.status().as_u16());
        }
        if response
            .content_length()
            .is_some_and(|length| length > OPENROUTER_MAX_RESPONSE_BYTES as u64)
        {
            return Err(0);
        }
        let mut bytes = Vec::new();
        response
            .take(OPENROUTER_MAX_RESPONSE_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| 0u16)?;
        if bytes.len() > OPENROUTER_MAX_RESPONSE_BYTES {
            return Err(0);
        }
        serde_json::from_slice(&bytes).map_err(|_| 0u16)
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelProfile {
    pub id: String,
    pub name: String,
    pub reasoning_efforts: Vec<String>,
    #[serde(default)]
    pub context_window_tokens: Option<u64>,
    #[serde(default)]
    pub supports_tools: Option<bool>,
    #[serde(default)]
    pub quality_rating: Option<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderProfile {
    pub id: String,
    pub kind: ProviderKind,
    pub name: String,
    pub executable: String,
    #[serde(default)]
    pub endpoint: Option<String>,
    pub secret_ref: Option<String>,
    pub models: Vec<ModelProfile>,
}

/// Returns a stable digest of the currently configured runtime entrypoint.
/// The digest includes its canonical path and bytes but never returns or
/// persists the path. It detects entrypoint changes; it does not attest to
/// dependencies or behavior behind a configured script wrapper.
pub fn runtime_identity_fingerprint(profile: &ProviderProfile) -> Result<String, String> {
    runtime_identity_fingerprint_until(profile, None)
}

pub fn runtime_identity_fingerprint_until(
    profile: &ProviderProfile,
    deadline: Option<Instant>,
) -> Result<String, String> {
    const MAX_EXECUTABLE_BYTES: u64 = 512 * 1024 * 1024;

    let ensure_time = || {
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            Err("Configured runtime identity inspection exceeded its deadline.".to_string())
        } else {
            Ok(())
        }
    };
    ensure_time()?;

    let configured = Path::new(profile.executable.trim());
    if profile.kind == ProviderKind::Codex {
        if !configured.is_absolute()
            || matches!(
                configured.extension().and_then(|value| value.to_str()),
                Some(extension)
                    if extension.eq_ignore_ascii_case("cmd")
                        || extension.eq_ignore_ascii_case("bat")
            )
        {
            return Err(
                "Codex observations require an explicit absolute entrypoint supported by the configured process adapter.".into(),
            );
        }
    }
    let availability = inspect(profile, std::env::var_os("PATH").as_deref());
    ensure_time()?;
    let executable = availability
        .executable_path
        .ok_or_else(|| "Configured provider executable is unavailable.".to_string())?;
    let path = PathBuf::from(executable);
    let canonical = path
        .canonicalize()
        .map_err(|_| "Configured provider executable could not be resolved.".to_string())?;
    ensure_time()?;
    let metadata = std::fs::metadata(&canonical)
        .map_err(|_| "Configured provider executable could not be inspected.".to_string())?;
    if !metadata.is_file() || metadata.len() == 0 || metadata.len() > MAX_EXECUTABLE_BYTES {
        return Err("Configured provider executable is not a bounded regular file.".into());
    }
    use std::io::Read;
    let mut hash_a = 0xcbf29ce484222325u64;
    let mut hash_b = 0x84222325cbf29ce4u64;
    let mut absorb = |bytes: &[u8]| {
        for byte in bytes {
            hash_a = (hash_a ^ u64::from(*byte)).wrapping_mul(0x100000001b3);
            hash_b = (hash_b ^ u64::from(*byte)).wrapping_mul(0x100000001b3);
        }
    };
    absorb(profile.kind.telemetry_id().as_bytes());
    absorb(&[0]);
    absorb(canonical.to_string_lossy().as_bytes());
    absorb(&[0]);
    let mut file = std::fs::File::open(&canonical)
        .map_err(|_| "Configured provider executable could not be read.".to_string())?;
    let mut buffer = [0u8; 64 * 1024];
    let mut total = 0u64;
    loop {
        ensure_time()?;
        let count = file
            .read(&mut buffer)
            .map_err(|_| "Configured provider executable could not be read.".to_string())?;
        if count == 0 {
            break;
        }
        total = total.saturating_add(count as u64);
        if total > MAX_EXECUTABLE_BYTES {
            return Err("Configured provider executable exceeds its size limit.".into());
        }
        absorb(&buffer[..count]);
    }
    ensure_time()?;
    Ok(format!("{hash_a:016x}{hash_b:016x}"))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderSelection {
    pub provider_id: Option<String>,
    pub model_id: Option<String>,
    pub reasoning_effort: Option<String>,
    pub runtime_id: Option<String>,
    #[serde(default)]
    pub route_source: Option<String>,
    #[serde(default)]
    pub route_confidence_percent: Option<u8>,
    #[serde(default)]
    pub route_explanation: Option<String>,
}

impl Default for ProviderSelection {
    fn default() -> Self {
        Self {
            provider_id: None,
            model_id: None,
            reasoning_effort: None,
            runtime_id: None,
            route_source: None,
            route_confidence_percent: None,
            route_explanation: None,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AdapterCapabilities {
    pub discovery: bool,
    pub health: bool,
    pub model_listing: bool,
    pub skill_support: bool,
    pub run_start: bool,
    pub run_cancel: bool,
    pub event_stream: bool,
    pub approval_requests: bool,
    pub interactive_input: bool,
    pub checkpoints: bool,
}

impl AdapterCapabilities {
    pub fn local_preflight() -> Self {
        Self {
            discovery: true,
            ..Self::default()
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderAvailability {
    pub provider_id: String,
    pub executable_found: bool,
    pub executable_path: Option<String>,
    pub authentication_reference: AuthenticationReferenceStatus,
    pub capabilities: AdapterCapabilities,
    pub note: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AntigravityProbe {
    pub version: Option<String>,
    pub state: String,
    pub models: Vec<String>,
    pub note: String,
}

pub const OLLAMA_DEFAULT_ENDPOINT: &str = "http://127.0.0.1:11434";
pub const OLLAMA_MIN_CONTEXT_TOKENS: u64 = 8_192;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OllamaModel {
    pub id: String,
    pub size_bytes: u64,
    pub context_tokens: Option<u64>,
    pub tools_supported: bool,
    pub eligible: bool,
    pub note: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OllamaProbe {
    pub state: String,
    pub endpoint: String,
    pub models: Vec<OllamaModel>,
    pub available_memory_bytes: Option<u64>,
    pub note: String,
}

pub fn validate_ollama_endpoint(endpoint: &str) -> Result<reqwest::Url, String> {
    let url = reqwest::Url::parse(endpoint.trim())
        .map_err(|_| "Ollama must use a valid local HTTP endpoint.".to_string())?;
    let loopback = url
        .host_str()
        .and_then(|host| {
            host.strip_prefix('[')
                .and_then(|value| value.strip_suffix(']'))
                .unwrap_or(host)
                .parse::<std::net::IpAddr>()
                .ok()
        })
        .is_some_and(|address| address.is_loopback());
    if url.scheme() != "http"
        || !loopback
        || url.port().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || (url.path() != "" && url.path() != "/")
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err("Ollama profiles are local-only: use HTTP with a numeric loopback address and port, without credentials or a URL path.".into());
    }
    Ok(url)
}

/// Inspect only the selected Ollama model. The endpoint validator enforces a
/// numeric loopback target; per-request timeouts share the caller's deadline.
pub fn ollama_selected_model_until(
    endpoint: &str,
    model_id: &str,
    deadline: Instant,
) -> Result<Option<OllamaModel>, String> {
    if model_id.trim().is_empty() || model_id.len() > 160 || model_id.chars().any(char::is_control)
    {
        return Err("Selected Ollama model ID is invalid.".into());
    }
    let normalized = validate_ollama_endpoint(endpoint)?;
    let base = normalized.as_str().trim_end_matches('/').to_owned();
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err("Ollama metadata refresh exceeded its dispatch deadline.".into());
    }
    let client = blocking_http_client_builder()
        .connect_timeout(remaining.min(Duration::from_millis(500)))
        .timeout(remaining)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| "Could not initialize the local Ollama health check.".to_string())?;
    let tags = request_bounded_json(
        client.get(format!("{base}/api/tags")).timeout(remaining),
        2 * 1024 * 1024,
        deadline,
    )?;
    let listed = tags
        .get("models")
        .and_then(Value::as_array)
        .is_some_and(|models| {
            models.iter().any(|entry| {
                entry
                    .get("name")
                    .or_else(|| entry.get("model"))
                    .and_then(Value::as_str)
                    == Some(model_id)
            })
        });
    if !listed {
        return Ok(None);
    }
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err("Ollama metadata refresh exceeded its dispatch deadline.".into());
    }
    let details = request_bounded_json(
        client
            .post(format!("{base}/api/show"))
            .json(&json!({"model": model_id, "verbose": false}))
            .timeout(remaining),
        1024 * 1024,
        deadline,
    )?;
    let parsed = ollama_models_from_responses(&tags, &[(model_id.to_owned(), details)]);
    Ok(parsed.into_iter().find(|model| model.id == model_id))
}

fn request_bounded_json(
    request: reqwest::blocking::RequestBuilder,
    max_bytes: usize,
    deadline: Instant,
) -> Result<Value, String> {
    if Instant::now() >= deadline {
        return Err("Provider metadata refresh exceeded its dispatch deadline.".into());
    }
    let response = request
        .send()
        .and_then(reqwest::blocking::Response::error_for_status)
        .map_err(|_| "Provider metadata endpoint could not be reached.".to_string())?;
    if response
        .content_length()
        .is_some_and(|length| length > max_bytes as u64)
    {
        return Err("Provider metadata response exceeds its size bound.".into());
    }
    let mut bytes = Vec::new();
    response
        .take(max_bytes as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "Provider metadata response could not be read.".to_string())?;
    if Instant::now() >= deadline {
        return Err("Provider metadata refresh exceeded its dispatch deadline.".into());
    }
    if bytes.len() > max_bytes {
        return Err("Provider metadata response exceeds its size bound.".into());
    }
    serde_json::from_slice(&bytes)
        .map_err(|_| "Provider metadata response was not valid JSON.".into())
}

pub fn ollama_models_from_responses(tags: &Value, details: &[(String, Value)]) -> Vec<OllamaModel> {
    let detail_by_id = details
        .iter()
        .map(|(id, value)| (id.as_str(), value))
        .collect::<std::collections::HashMap<_, _>>();
    tags.get("models")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| {
            let id = item
                .get("name")
                .or_else(|| item.get("model"))
                .and_then(Value::as_str)?;
            if id.trim().is_empty() || id.len() > 160 || id.chars().any(char::is_control) {
                return None;
            }
            let detail = detail_by_id.get(id).copied();
            let info = detail.and_then(|value| value.get("model_info"));
            let context_tokens = info.and_then(|value| {
                value.as_object()?.iter().find_map(|(key, value)| {
                    key.ends_with(".context_length")
                        .then(|| value.as_u64())
                        .flatten()
                })
            });
            let tools_supported = detail
                .and_then(|value| value.get("capabilities"))
                .and_then(Value::as_array)
                .is_some_and(|caps| caps.iter().any(|cap| cap.as_str() == Some("tools")));
            let eligible = tools_supported
                && context_tokens.is_some_and(|value| value >= OLLAMA_MIN_CONTEXT_TOKENS);
            let note = if !tools_supported {
                "Ollama did not report tool-calling support for this model.".into()
            } else if context_tokens.is_none() {
                "Model context length is unknown; this model cannot be selected for agent work."
                    .into()
            } else if context_tokens.is_some_and(|value| value < OLLAMA_MIN_CONTEXT_TOKENS) {
                "Model context is below the 8,192-token minimum for agent work.".into()
            } else {
                "Tool calling and the minimum agent context were reported by Ollama.".into()
            };
            Some(OllamaModel {
                id: id.into(),
                size_bytes: item.get("size").and_then(Value::as_u64).unwrap_or(0),
                context_tokens,
                tools_supported,
                eligible,
                note,
            })
        })
        .take(50)
        .collect()
}

pub fn ollama_probe(endpoint: &str) -> OllamaProbe {
    let normalized = match validate_ollama_endpoint(endpoint) {
        Ok(url) => url,
        Err(message) => {
            return OllamaProbe {
                state: "unavailable".into(),
                endpoint: endpoint.into(),
                models: Vec::new(),
                available_memory_bytes: available_memory_bytes(),
                note: message,
            };
        }
    };
    let endpoint = normalized.as_str().trim_end_matches('/').to_owned();
    let client = match blocking_http_client_builder()
        .connect_timeout(Duration::from_millis(500))
        .timeout(Duration::from_secs(3))
        .redirect(reqwest::redirect::Policy::none())
        .build()
    {
        Ok(client) => client,
        Err(_) => {
            return OllamaProbe {
                state: "unavailable".into(),
                endpoint,
                models: Vec::new(),
                available_memory_bytes: available_memory_bytes(),
                note: "Could not initialize the local Ollama health check.".into(),
            };
        }
    };
    let tags: Value = match client.get(format!("{endpoint}/api/tags")).send().and_then(|response| response.error_for_status()).and_then(|response| response.json()) {
        Ok(value) => value,
        Err(_) => return OllamaProbe { state: "unavailable".into(), endpoint, models: Vec::new(), available_memory_bytes: available_memory_bytes(), note: "Ollama is not reachable at the configured local endpoint, or its model-list request failed.".into() },
    };
    let model_ids = tags
        .get("models")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .take(50)
        .filter_map(|item| {
            item.get("name")
                .or_else(|| item.get("model"))
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .collect::<Vec<_>>();
    let details = std::thread::scope(|scope| {
        let workers = model_ids.chunks(8).map(|models| {
            let models = models.to_vec();
            let client = client.clone();
            let endpoint = endpoint.clone();
            scope.spawn(move || {
                models
                    .into_iter()
                    .filter_map(|id| {
                        let value = client
                            .post(format!("{endpoint}/api/show"))
                            .json(&json!({ "model": id, "verbose": false }))
                            .send()
                            .and_then(|response| response.error_for_status())
                            .and_then(|response| response.json::<Value>());
                        value.ok().map(|value| (id, value))
                    })
                    .collect::<Vec<_>>()
            })
        });
        workers
            .map(|worker| worker.join().unwrap_or_default())
            .flatten()
            .collect::<Vec<_>>()
    });
    let parsed = ollama_models_from_responses(&tags, &details);
    let eligible = parsed.iter().filter(|model| model.eligible).count();
    OllamaProbe {
        state: "ready".into(),
        endpoint,
        models: parsed,
        available_memory_bytes: available_memory_bytes(),
        note: format!(
            "Ollama is reachable. Up to 50 local models were inspected; {eligible} report tool support and at least {OLLAMA_MIN_CONTEXT_TOKENS} context tokens. Unknown capabilities remain ineligible."
        ),
    }
}

#[cfg(windows)]
fn available_memory_bytes() -> Option<u64> {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    let mut status = MEMORYSTATUSEX {
        dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32,
        ..unsafe { std::mem::zeroed() }
    };
    (unsafe { GlobalMemoryStatusEx(&mut status) } != 0).then_some(status.ullAvailPhys)
}

#[cfg(not(windows))]
fn available_memory_bytes() -> Option<u64> {
    None
}

pub fn ollama_timeout_seconds(
    model_size_bytes: u64,
    available_memory_bytes: Option<u64>,
) -> Option<u64> {
    let available = available_memory_bytes?;
    if model_size_bytes == 0 || available == 0 {
        return None;
    }
    let ratio = available as f64 / model_size_bytes as f64;
    Some((1_200.0 / ratio.sqrt()).round().clamp(300.0, 3_600.0) as u64)
}

pub fn aider_ollama_arguments(
    model_id: &str,
    prompt_file: &str,
    test_command: Option<&str>,
    empty_env_file: &str,
) -> Vec<String> {
    let mut args = vec![
        "--model".into(),
        format!("ollama_chat/{model_id}"),
        "--message".into(),
        prompt_file.into(),
        "--yes".into(),
        "--no-auto-commits".into(),
        "--no-gitignore".into(),
        "--no-add-gitignore-files".into(),
        "--no-analytics".into(),
        "--no-check-update".into(),
        "--no-show-release-notes".into(),
        "--no-restore-chat-history".into(),
        "--no-suggest-shell-commands".into(),
        "--no-stream".into(),
        "--no-pretty".into(),
        "--env-file".into(),
        empty_env_file.into(),
    ];
    if let Some(test_command) = test_command.filter(|value| !value.trim().is_empty()) {
        args.extend([
            "--test-cmd".into(),
            test_command.into(),
            "--auto-test".into(),
        ]);
    }
    args
}

pub fn probe_antigravity(executable: &Path) -> AntigravityProbe {
    let temp = match tempfile::tempdir() {
        Ok(temp) => temp,
        Err(_) => {
            return AntigravityProbe {
                version: None,
                state: "unavailable".into(),
                models: Vec::new(),
                note: "Could not create a temporary directory for the provider probe.".into(),
            };
        }
    };
    let version_output = crate::process::capture(
        executable,
        &["--version".into()],
        temp.path(),
        Duration::from_secs(5),
        64 * 1024,
    );
    let version = version_output.ok().and_then(|output| {
        (!output.truncated && output.status.is_some_and(|status| status.success()))
            .then(|| safe_version(&output.stdout))
            .flatten()
    });
    if version.is_none() {
        return AntigravityProbe {
            version: None,
            state: "unavailable".into(),
            models: Vec::new(),
            note: "The Antigravity CLI version probe could not complete.".into(),
        };
    }
    let models = crate::process::capture(
        executable,
        &["models".into()],
        temp.path(),
        Duration::from_secs(10),
        256 * 1024,
    );
    match models {
        Ok(output)
            if output.status.is_some_and(|status| status.success())
                && !output.timed_out
                && !output.truncated =>
        {
            let models = String::from_utf8_lossy(&output.stdout)
                .lines()
                .filter_map(|line| line.split_whitespace().next())
                .filter(|name| name.starts_with("gemini-"))
                .map(str::to_owned)
                .take(200)
                .collect::<Vec<_>>();
            AntigravityProbe {
                version,
                state: "ready".into(),
                note: if models.is_empty() {
                    "The CLI is available, but it returned no model identifiers.".into()
                } else {
                    "Authenticated CLI; model IDs were read from agy models.".into()
                },
                models,
            }
        }
        Ok(output) => {
            let diagnostic = format!(
                "{} {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            )
            .to_ascii_lowercase();
            let authentication_required = [
                "sign in",
                "not logged",
                "authentication required",
                "auth required",
            ]
            .iter()
            .any(|marker| diagnostic.contains(marker));
            AntigravityProbe {
                version,
                state: if authentication_required {
                    "authenticationRequired"
                } else {
                    "unavailable"
                }
                .into(),
                models: Vec::new(),
                note: if authentication_required {
                    "The CLI is installed, but model discovery requires an Antigravity sign-in. No credentials were read into Navis-Fabrica.".into()
                } else {
                    "The CLI is installed, but model discovery is unavailable. Diagnostics were omitted to avoid exposing credential data.".into()
                },
            }
        }
        Err(_) => AntigravityProbe {
            version,
            state: "unavailable".into(),
            models: Vec::new(),
            note: "The Antigravity model-list command could not start.".into(),
        },
    }
}

fn safe_version(bytes: &[u8]) -> Option<String> {
    let value = String::from_utf8_lossy(bytes)
        .lines()
        .next()?
        .trim()
        .to_owned();
    let token = value.split_whitespace().last()?;
    if token.len() <= 48
        && token.chars().any(|c| c.is_ascii_digit())
        && token
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || ".+-_".contains(c))
        && !token.to_ascii_lowercase().starts_with("sk-")
    {
        Some(token.to_owned())
    } else {
        None
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "state")]
pub enum ProviderHealth {
    Unknown,
    Ready,
    MissingExecutable,
    AuthenticationRequired,
    Unhealthy { message: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AuthenticationReferenceStatus {
    Missing,
    ReferenceConfigured,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "kind")]
pub enum ProviderEvent {
    Output {
        sequence: u64,
        text: String,
    },
    StateChanged {
        sequence: u64,
        state: String,
    },
    NeedsInput {
        sequence: u64,
        prompt: String,
    },
    Checkpoint {
        sequence: u64,
        checkpoint: RunCheckpoint,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunCheckpoint {
    pub provider_id: String,
    pub runtime_id: String,
    pub checkpoint_ref: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderRunRequest {
    pub run_id: String,
    pub model_id: String,
    pub reasoning_effort: Option<String>,
    pub runtime_id: String,
    pub prompt: String,
    pub checkpoint: Option<RunCheckpoint>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexEvent {
    pub kind: String,
    pub text: String,
    pub terminal_state: Option<String>,
}

pub fn antigravity_arguments(model_id: &str, prompt: &str, timeout_seconds: u64) -> Vec<String> {
    let arguments = vec![
        "--sandbox".into(),
        "--print-timeout".into(),
        format!("{timeout_seconds}s"),
        "--model".into(),
        model_id.into(),
        "--print".into(),
        prompt.into(),
    ];
    arguments
}

pub fn app_server_initialize(request_id: u64, app_version: &str) -> Value {
    json!({
        "id": request_id,
        "method": "initialize",
        "params": {
            "clientInfo": { "name": "navis_fabrica", "title": "Navis-Fabrica", "version": app_version },
            "capabilities": { "experimentalApi": true, "requestAttestation": false }
        }
    })
}

pub fn app_server_thread_start(request_id: u64, cwd: &str, model_id: &str) -> Value {
    json!({
        "id": request_id,
        "method": "thread/start",
        "params": {
            "cwd": cwd,
            "runtimeWorkspaceRoots": [cwd],
            "model": model_id,
            "allowProviderModelFallback": false,
            "approvalPolicy": "on-request",
            "sandbox": "workspace-write",
            "ephemeral": false
        }
    })
}

pub fn app_server_read_only_thread_start(request_id: u64, cwd: &str, model_id: &str) -> Value {
    let mut message = app_server_thread_start(request_id, cwd, model_id);
    message["params"]["runtimeWorkspaceRoots"] = json!([]);
    message["params"]["approvalPolicy"] = json!("never");
    message["params"]["sandbox"] = json!("read-only");
    message["params"]["ephemeral"] = json!(true);
    message["params"]["dynamicTools"] = json!([]);
    message["params"]["config"] = json!({
        "apps": { "_default": { "enabled": false } },
        "tools": { "web_search": { "enabled": false } },
        "mcp_servers": {}
    });
    message
}

pub fn app_server_turn_start(
    request_id: u64,
    thread_id: &str,
    cwd: &str,
    model_id: &str,
    effort: Option<&str>,
    prompt: &str,
    skill: Option<(&str, &str)>,
) -> Value {
    app_server_turn_start_with_output_schema(
        request_id, thread_id, cwd, model_id, effort, prompt, skill, None, false,
    )
}

pub(crate) fn app_server_turn_start_with_output_schema(
    request_id: u64,
    thread_id: &str,
    cwd: &str,
    model_id: &str,
    effort: Option<&str>,
    prompt: &str,
    skill: Option<(&str, &str)>,
    output_schema: Option<&Value>,
    read_only: bool,
) -> Value {
    let mut input = Vec::new();
    if let Some((name, path)) = skill {
        input.push(json!({ "type": "skill", "name": name, "path": path }));
    }
    input.push(json!({ "type": "text", "text": prompt, "text_elements": [] }));
    let mut params = json!({
        "threadId": thread_id,
        "cwd": cwd,
        "runtimeWorkspaceRoots": [cwd],
        "model": model_id,
        "approvalPolicy": if read_only { "never" } else { "on-request" },
        "input": input
    });
    if let Some(effort) = effort {
        params["effort"] = json!(effort);
    }
    if let Some(output_schema) = output_schema {
        params["outputSchema"] = output_schema.clone();
    }
    if read_only {
        params["runtimeWorkspaceRoots"] = json!([]);
        params["config"] = json!({
            "apps": { "_default": { "enabled": false } },
            "tools": { "web_search": { "enabled": false } },
            "mcp_servers": {}
        });
    }
    json!({ "id": request_id, "method": "turn/start", "params": params })
}

pub fn app_server_response(request_id: Value, result: Value) -> Value {
    json!({ "id": request_id, "result": result })
}

pub fn parse_app_server_message(line: &str) -> Result<Value, String> {
    let value: Value = serde_json::from_str(line)
        .map_err(|_| "Codex app-server emitted malformed JSON.".to_string())?;
    if !value.is_object()
        || value
            .get("jsonrpc")
            .is_some_and(|version| version.as_str() != Some("2.0"))
    {
        return Err("Codex app-server emitted an invalid protocol message.".into());
    }
    if value
        .get("method")
        .is_some_and(|method| !method.is_string())
    {
        return Err("Codex app-server emitted an invalid protocol method.".into());
    }
    Ok(value)
}

pub fn normalize_app_server_message(message: &Value) -> Option<CodexEvent> {
    // JSON-RPC request errors are responses, not app-server notifications. A
    // caller waiting for a specific request id must still receive a bounded,
    // secret-safe event classification from the response body.
    if message.get("error").is_some() {
        let error = message.get("error").unwrap_or(&Value::Null);
        let detail = error.get("message").and_then(Value::as_str).unwrap_or("");
        return Some(CodexEvent {
            kind: "providerError".into(),
            text: if is_authentication_error(detail) {
                "Codex authentication is required or expired.".into()
            } else if is_rate_limit_error(detail) {
                "Codex rate limit was reached.".into()
            } else if is_unsupported_model_error(detail) {
                "Codex model is unsupported.".into()
            } else {
                "Codex app-server reported an error.".into()
            },
            terminal_state: Some("failed".into()),
        });
    }
    let method = message.get("method")?.as_str()?;
    let params = message.get("params").unwrap_or(&Value::Null);
    match method {
        "item/agentMessage/delta" => Some(CodexEvent {
            kind: "output".into(),
            text: params.get("delta")?.as_str()?.into(),
            terminal_state: None,
        }),
        "item/completed" => {
            let item = params.get("item")?;
            if item.get("type")?.as_str()? != "agentMessage" {
                return None;
            }
            Some(CodexEvent {
                kind: "finalOutput".into(),
                text: item.get("text")?.as_str()?.into(),
                terminal_state: None,
            })
        }
        "turn/completed" => {
            let status = params.get("turn")?.get("status")?.as_str()?;
            let state = match status {
                "completed" => "completed",
                "interrupted" => "cancelled",
                _ => "failed",
            };
            let message = params
                .get("turn")
                .and_then(|turn| turn.get("error"))
                .and_then(|error| error.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("");
            Some(CodexEvent {
                kind: if state == "failed" {
                    "providerError"
                } else {
                    "state"
                }
                .into(),
                text: if state == "failed" {
                    if is_authentication_error(message) {
                        "Codex authentication is required or expired.".into()
                    } else if is_rate_limit_error(message) {
                        "Codex rate limit was reached.".into()
                    } else if is_unsupported_model_error(message) {
                        "Codex model is unsupported.".into()
                    } else {
                        "Codex reported that the turn failed.".into()
                    }
                } else {
                    state.into()
                },
                terminal_state: Some(state.into()),
            })
        }
        "error" => {
            let message = params.get("message").and_then(Value::as_str).unwrap_or("");
            Some(CodexEvent {
                kind: "providerError".into(),
                text: if is_authentication_error(message) {
                    "Codex authentication is required or expired.".into()
                } else if is_rate_limit_error(message) {
                    "Codex rate limit was reached.".into()
                } else if is_unsupported_model_error(message) {
                    "Codex model is unsupported.".into()
                } else {
                    "Codex app-server reported an error.".into()
                },
                terminal_state: Some("failed".into()),
            })
        }
        "item/commandExecution/requestApproval"
        | "item/fileChange/requestApproval"
        | "item/permissions/requestApproval" => Some(CodexEvent {
            kind: "approvalRequest".into(),
            text: serde_json::to_string(&json!({
                "requestId": message.get("id"),
                "method": method,
                "params": {
                    "kind": params.get("kind"),
                    "reason": params.get("reason"),
                    "command": params.get("command"),
                    "cwd": params.get("cwd"),
                    "permissions": params.get("permissions"),
                    "availableDecisions": params.get("availableDecisions")
                }
            }))
            .ok()?,
            terminal_state: None,
        }),
        "item/tool/requestUserInput" => Some(CodexEvent {
            kind: "userInputRequest".into(),
            text: serde_json::to_string(&json!({
                "requestId": message.get("id"),
                "method": method,
                "questions": params.get("questions")
            }))
            .ok()?,
            terminal_state: None,
        }),
        _ => None,
    }
}

fn is_authentication_error(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    [
        "unauthorized",
        "authentication",
        "sign in",
        "login required",
        "401",
        "invalid api key",
    ]
    .iter()
    .any(|needle| message.contains(needle))
}

fn is_rate_limit_error(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    ["429", "rate limit", "too many requests"]
        .iter()
        .any(|needle| message.contains(needle))
}

fn is_unsupported_model_error(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    ["unsupported model", "model not found", "unknown model"]
        .iter()
        .any(|needle| message.contains(needle))
}

pub fn normalize_codex_stderr(message: &str) -> Option<CodexEvent> {
    is_authentication_error(message).then(|| CodexEvent {
        kind: "providerError".into(),
        text: "Codex authentication is required or expired.".into(),
        terminal_state: Some("failed".into()),
    })
}

/// Provider implementations must explicitly declare each operation they support.
/// The initial phase ships the contract and local executable preflight only; no
/// provider process is launched until a concrete adapter is implemented.
pub trait ProviderAdapter: Send + Sync {
    fn kind(&self) -> ProviderKind;
    fn capabilities(&self) -> AdapterCapabilities;
    fn discover(&self, profile: &ProviderProfile) -> Result<ProviderAvailability, String>;
    fn health(&self, profile: &ProviderProfile) -> Result<ProviderHealth, String>;
    fn list_models(&self, profile: &ProviderProfile) -> Result<Vec<ModelProfile>, String>;
    fn start_run(
        &self,
        profile: &ProviderProfile,
        request: ProviderRunRequest,
    ) -> Result<(), String>;
    fn cancel_run(&self, profile: &ProviderProfile, run_id: &str) -> Result<bool, String>;
    fn stream_events(
        &self,
        profile: &ProviderProfile,
        run_id: &str,
    ) -> Result<Receiver<ProviderEvent>, String>;
    fn resume_from_checkpoint(
        &self,
        profile: &ProviderProfile,
        checkpoint: RunCheckpoint,
    ) -> Result<(), String>;
}

pub fn find_executable(executable: &str, path_value: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    if executable.trim().is_empty() {
        return None;
    }
    let candidate = Path::new(executable);
    if candidate.components().count() > 1 || candidate.is_absolute() {
        return candidate.is_file().then(|| candidate.to_path_buf());
    }
    let path_value = path_value?;
    for directory in std::env::split_paths(path_value) {
        let direct = directory.join(executable);
        if direct.is_file() {
            return Some(direct);
        }
        #[cfg(windows)]
        for extension in [".exe", ".cmd", ".bat"] {
            let with_extension = directory.join(format!("{executable}{extension}"));
            if with_extension.is_file() {
                return Some(with_extension);
            }
        }
    }
    None
}

pub fn inspect(
    profile: &ProviderProfile,
    path_value: Option<&std::ffi::OsStr>,
) -> ProviderAvailability {
    let executable_path = find_executable(&profile.executable, path_value);
    let capabilities = match profile.kind {
        ProviderKind::Codex => AdapterCapabilities {
            run_start: true,
            run_cancel: true,
            event_stream: true,
            skill_support: true,
            approval_requests: true,
            interactive_input: true,
            ..AdapterCapabilities::local_preflight()
        },
        ProviderKind::Antigravity => AdapterCapabilities {
            health: true,
            model_listing: true,
            run_start: true,
            run_cancel: true,
            event_stream: true,
            skill_support: true,
            ..AdapterCapabilities::local_preflight()
        },
        ProviderKind::Ollama => AdapterCapabilities {
            health: true,
            model_listing: true,
            run_start: true,
            run_cancel: true,
            event_stream: true,
            checkpoints: true,
            ..AdapterCapabilities::local_preflight()
        },
        ProviderKind::OpenRouter => AdapterCapabilities {
            health: true,
            model_listing: true,
            run_start: true,
            run_cancel: true,
            event_stream: true,
            checkpoints: true,
            ..AdapterCapabilities::local_preflight()
        },
        _ => AdapterCapabilities::local_preflight(),
    };
    ProviderAvailability {
        provider_id: profile.id.clone(),
        executable_found: executable_path.is_some(),
        executable_path: executable_path.map(|path| path.to_string_lossy().into_owned()),
        authentication_reference: if profile
            .secret_ref
            .as_deref()
            .is_some_and(|value| !value.trim().is_empty())
        {
            AuthenticationReferenceStatus::ReferenceConfigured
        } else {
            AuthenticationReferenceStatus::Missing
        },
        capabilities,
        note: if profile.kind == ProviderKind::Codex {
            "Codex app-server runs are supported with streamed output, on-request approvals, and user input. Authentication is checked when a run starts; this check never launches the provider.".into()
        } else if profile.kind == ProviderKind::Antigravity {
            "Antigravity uses the installed agy --print CLI with process output and cancellation. This version does not expose structured progress or reasoning-effort flags; model listing requires sign-in. Headless approvals, interactive input, and resumable checkpoints are unavailable.".into()
        } else if profile.kind == ProviderKind::Ollama {
            "Ollama discovery is restricted to a numeric loopback endpoint. Aider provides model-driven edits and configured test execution; model tool-call support and context are checked before runs. Portable handoff is explicit and does not resume completed side effects.".into()
        } else if profile.kind == ProviderKind::OpenRouter {
            "OpenRouter provides a model catalog and API access. Aider is the separately configured coding harness; shell and file-edit behavior comes from that harness. Authentication is stored in Windows Credential Manager.".into()
        } else {
            "Executable presence is checked without launching it. Authentication validity and provider operations are not implemented by this adapter.".into()
        },
    }
}

/// Probe a configured adapter without exposing credentials or raw process output.
/// OpenRouter network discovery is opt-in; other checks are local executable or
/// loopback checks supported by their provider contracts.
pub fn probe_installed(profile: &ProviderProfile, allow_live_probe: bool) -> Value {
    if profile.kind == ProviderKind::Custom {
        return json!({"providerId":profile.id,"kind":profile.kind,"state":"unsupportedProvider","note":"This runtime has no supported coding adapter in protocol v1."});
    }
    let path_value = std::env::var_os("PATH");
    let availability = inspect(profile, path_value.as_deref());
    let Some(executable) = availability.executable_path.as_deref().map(Path::new) else {
        return json!({"providerId":profile.id,"kind":profile.kind,"state":"missingRuntime","authentication":"unverified","note":"Configured executable was not found on PATH or at its saved path."});
    };
    match profile.kind {
        ProviderKind::Antigravity => {
            let probe = probe_antigravity(executable);
            json!({"providerId":profile.id,"kind":profile.kind,"state":probe.state,"version":probe.version,"models":probe.models,"note":probe.note,"capabilities":availability.capabilities})
        }
        ProviderKind::Ollama => {
            let probe = ollama_probe(
                profile
                    .endpoint
                    .as_deref()
                    .unwrap_or(OLLAMA_DEFAULT_ENDPOINT),
            );
            json!({"providerId":profile.id,"kind":profile.kind,"state":probe.state,"models":probe.models,"availableMemoryBytes":probe.available_memory_bytes,"note":probe.note,"capabilities":availability.capabilities})
        }
        ProviderKind::OpenRouter => {
            let version = probe_aider(executable);
            let reference = profile.secret_ref.as_deref();
            let credential = reference
                .filter(|value| openrouter_credential_reference_matches(&profile.id, value))
                .and_then(crate::credentials::resolve);
            let state = if !version.supported {
                "unsupportedRuntime"
            } else if credential.is_none() {
                "authenticationRequired"
            } else if allow_live_probe {
                "checking"
            } else {
                "liveProbeRequired"
            };
            let result = if allow_live_probe {
                credential.as_deref().map(openrouter_http_probe)
            } else {
                None
            };
            let state = result
                .as_ref()
                .map(|value| value.state.clone())
                .unwrap_or_else(|| state.to_owned());
            json!({"providerId":profile.id,"kind":profile.kind,"state":state,"version":version.version,"aiderSupported":version.supported,"runtimeState":version.state,"authenticationReferenceConfigured":credential.is_some(),"models":result.map(|p|p.models).unwrap_or_default(),"note":version.note,"capabilities":availability.capabilities})
        }
        ProviderKind::Codex => {
            let probe = probe_codex(executable);
            json!({"providerId":profile.id,"kind":profile.kind,"state":probe.state,"version":probe.version,"authentication":"providerManagedUnverified","note":probe.note,"capabilities":availability.capabilities})
        }
        ProviderKind::Custom => {
            json!({"providerId":profile.id,"kind":profile.kind,"state":"unsupportedProvider","note":"This runtime has no supported coding adapter in protocol v1."})
        }
    }
}

#[derive(Debug)]
struct RuntimeProbe {
    version: Option<String>,
    supported: bool,
    state: &'static str,
    note: &'static str,
}

pub fn probe_aider_runtime_version(executable: &Path) -> Option<String> {
    let probe = probe_aider(executable);
    probe.supported.then_some(probe.version).flatten()
}

fn probe_aider(executable: &Path) -> RuntimeProbe {
    let Ok(temp) = tempfile::tempdir() else {
        return RuntimeProbe {
            version: None,
            supported: false,
            state: "unavailable",
            note: "Could not create a temporary folder for Aider discovery.",
        };
    };
    let Some(environment) = aider_probe_environment(temp.path()) else {
        return RuntimeProbe {
            version: None,
            supported: false,
            state: "unavailable",
            note: "Could not create an isolated profile for Aider discovery.",
        };
    };
    let version = crate::process::capture_with_environment(
        executable,
        &["--version".into()],
        temp.path(),
        Duration::from_secs(15),
        64 * 1024,
        Some(&environment),
    )
    .ok()
    .and_then(|output| {
        (!output.truncated && output.status.is_some_and(|s| s.success()))
            .then(|| safe_version(&output.stdout))
            .flatten()
    });
    let help = crate::process::capture_with_environment(
        executable,
        &["--help".into()],
        temp.path(),
        Duration::from_secs(15),
        256 * 1024,
        Some(&environment),
    )
    .ok();
    let flags = help.as_ref().is_some_and(|output| {
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        [
            "--model",
            "--message",
            "--env-file",
            "--no-auto-lint",
            "--no-auto-test",
            "--no-git",
        ]
        .iter()
        .all(|flag| text.contains(flag))
            && output.status.is_some_and(|s| s.success())
            && !output.timed_out
            && !output.truncated
    });
    let supported = version.is_some() && flags;
    RuntimeProbe {
        version,
        supported,
        state: if supported {
            "ready"
        } else if help.is_some() {
            "unsupportedRuntime"
        } else {
            "unavailable"
        },
        note: if supported {
            "Aider exposes the required noninteractive model, message, and credential-env options."
        } else {
            "Aider is missing or does not expose the required noninteractive coding interface."
        },
    }
}

fn aider_probe_environment(root: &Path) -> Option<Vec<(std::ffi::OsString, std::ffi::OsString)>> {
    let profile = root.join("profile");
    let appdata = profile.join("AppData").join("Roaming");
    let local_appdata = profile.join("AppData").join("Local");
    for path in [&profile, &appdata, &local_appdata] {
        std::fs::create_dir_all(path).ok()?;
    }
    let mut env = Vec::new();
    for name in ["PATH", "PATHEXT", "SYSTEMROOT", "WINDIR", "TEMP", "TMP"] {
        if let Some(value) = std::env::var_os(name) {
            env.push((name.into(), value));
        }
    }
    env.extend([
        ("USERPROFILE".into(), profile.clone().into()),
        ("HOME".into(), profile.clone().into()),
        (
            "HOMEDRIVE".into(),
            profile
                .to_string_lossy()
                .chars()
                .take(2)
                .collect::<String>()
                .into(),
        ),
        (
            "HOMEPATH".into(),
            profile
                .to_string_lossy()
                .chars()
                .skip(2)
                .collect::<String>()
                .into(),
        ),
        ("APPDATA".into(), appdata.into()),
        ("LOCALAPPDATA".into(), local_appdata.into()),
        ("XDG_CONFIG_HOME".into(), profile.into()),
        ("PYTHONNOUSERSITE".into(), "1".into()),
        ("LITELLM_LOCAL_MODEL_COST_MAP".into(), "True".into()),
        ("OPENAI_API_BASE".into(), "http://127.0.0.1:1/v1".into()),
        ("OPENAI_API_KEY".into(), "fabrica-dummy".into()),
        ("NO_PROXY".into(), "*".into()),
        ("no_proxy".into(), "*".into()),
    ]);
    Some(env)
}

fn probe_codex(executable: &Path) -> RuntimeProbe {
    let Ok(temp) = tempfile::tempdir() else {
        return RuntimeProbe {
            version: None,
            supported: false,
            state: "unavailable",
            note: "Could not create a temporary folder for Codex discovery.",
        };
    };
    let version = crate::process::capture(
        executable,
        &["--version".into()],
        temp.path(),
        Duration::from_secs(5),
        64 * 1024,
    )
    .ok()
    .and_then(|output| {
        (!output.truncated && output.status.is_some_and(|s| s.success()))
            .then(|| safe_version(&output.stdout))
            .flatten()
    });
    let app_server = crate::process::capture(
        executable,
        &["app-server".into(), "--help".into()],
        temp.path(),
        Duration::from_secs(5),
        128 * 1024,
    )
    .ok()
    .is_some_and(|output| {
        output.status.is_some_and(|s| s.success()) && !output.timed_out && !output.truncated
    });
    let supported = version.is_some() && app_server;
    RuntimeProbe {
        version,
        supported,
        state: if supported {
            "providerManagedUnverified"
        } else {
            "unsupportedRuntime"
        },
        note: if supported {
            "Codex version and app-server interface are installed; provider-managed authentication is checked when a run starts."
        } else {
            "The configured executable does not expose the expected Codex app-server interface."
        },
    }
}

pub fn validate_profiles(
    profiles: &[ProviderProfile],
    selection: &ProviderSelection,
) -> Result<(), String> {
    if selection
        .route_source
        .as_deref()
        .is_some_and(|source| !matches!(source, "manual" | "deterministic" | "jev"))
        || selection
            .route_confidence_percent
            .is_some_and(|value| value > 100)
        || selection
            .route_explanation
            .as_deref()
            .is_some_and(|value| value.len() > 512 || value.chars().any(char::is_control))
    {
        return Err("The saved route decision metadata is invalid.".into());
    }
    if profiles.len() > 16 {
        return Err("Configure 16 or fewer providers.".into());
    }
    let mut ids = Vec::new();
    for profile in profiles {
        if profile.id.trim().is_empty()
            || profile.id.len() > 80
            || profile.name.trim().is_empty()
            || profile.name.len() > 100
        {
            return Err(
                "Provider IDs and names must be non-empty and within their length limits.".into(),
            );
        }
        if profile.id.chars().any(char::is_control) || profile.name.chars().any(char::is_control) {
            return Err("Provider IDs and names cannot contain control characters.".into());
        }
        if ids.iter().any(|id: &String| id == &profile.id) {
            return Err("Provider IDs must be unique.".into());
        }
        ids.push(profile.id.clone());
        if profile.kind == ProviderKind::Ollama {
            validate_ollama_endpoint(
                profile
                    .endpoint
                    .as_deref()
                    .unwrap_or(OLLAMA_DEFAULT_ENDPOINT),
            )?;
            if profile.secret_ref.is_some() {
                return Err(
                    "Local-only Ollama profiles cannot store or use remote provider credentials."
                        .into(),
                );
            }
        } else if profile.endpoint.is_some() {
            return Err("Only Ollama profiles may configure an endpoint.".into());
        }
        if profile.executable.len() > 260 || profile.executable.chars().any(char::is_control) {
            return Err("Executable paths must be 260 characters or fewer and contain no control characters.".into());
        }
        if profile.secret_ref.as_ref().is_some_and(|value| {
            let reference_key = value
                .strip_prefix("credential-store:")
                .or_else(|| value.strip_prefix("provider-managed:"));
            value.len() > 260
                || value.chars().any(char::is_control)
                || reference_key.map_or(true, |key| {
                    key.is_empty()
                        || !key.chars().all(|character| {
                            character.is_ascii_alphanumeric() || ".:/_-".contains(character)
                        })
                })
        }) {
            return Err("Use an opaque reference such as credential-store:provider-name; secret values are not accepted.".into());
        }
        let (model_limit, model_name_limit) = if profile.kind == ProviderKind::OpenRouter {
            (OPENROUTER_MAX_MODELS, 160)
        } else {
            (200, 100)
        };
        if profile.models.len() > model_limit {
            return Err(format!(
                "Configure {model_limit} or fewer model profiles per provider."
            ));
        }
        let mut model_ids = Vec::new();
        for model in &profile.models {
            if model.id.trim().is_empty()
                || model.id.len() > 160
                || model.name.trim().is_empty()
                || model.name.len() > model_name_limit
                || model.id.chars().any(char::is_control)
                || model.name.chars().any(char::is_control)
            {
                return Err(
                    "Model IDs and names must be valid and within their length limits.".into(),
                );
            }
            if model_ids.iter().any(|id: &String| id == &model.id) {
                return Err("Model IDs must be unique within a provider.".into());
            }
            model_ids.push(model.id.clone());
            if model.reasoning_efforts.len() > 8
                || model.reasoning_efforts.iter().any(|effort| {
                    effort.is_empty() || effort.len() > 40 || effort.chars().any(char::is_control)
                })
            {
                return Err(
                    "Reasoning effort options must be valid and limited to eight per model.".into(),
                );
            }
            if model
                .quality_rating
                .is_some_and(|rating| !(1..=5).contains(&rating))
            {
                return Err("Model quality ratings must be from 1 to 5.".into());
            }
            if model
                .context_window_tokens
                .is_some_and(|tokens| tokens == 0 || tokens > 10_000_000)
            {
                return Err(
                    "Model context windows must be between 1 and 10,000,000 tokens.".into(),
                );
            }
        }
    }
    if let Some(provider_id) = &selection.provider_id {
        let provider = profiles
            .iter()
            .find(|profile| &profile.id == provider_id)
            .ok_or("Select a configured provider.")?;
        if selection
            .model_id
            .as_ref()
            .is_some_and(|model_id| !provider.models.iter().any(|model| &model.id == model_id))
        {
            return Err("Select a model configured for the selected provider.".into());
        }
        if selection.reasoning_effort.as_ref().is_some_and(|effort| {
            !provider.models.iter().any(|model| {
                selection
                    .model_id
                    .as_ref()
                    .is_some_and(|model_id| &model.id == model_id)
                    && model.reasoning_efforts.contains(effort)
            })
        }) {
            return Err("Select a reasoning effort supported by the selected model.".into());
        }
    } else if selection.model_id.is_some() || selection.reasoning_effort.is_some() {
        return Err("Choose a provider before selecting its model or reasoning effort.".into());
    }
    for value in [
        &selection.provider_id,
        &selection.model_id,
        &selection.reasoning_effort,
        &selection.runtime_id,
    ]
    .into_iter()
    .flatten()
    {
        if value.len() > 160 || value.chars().any(char::is_control) {
            return Err("Provider selections must be 160 characters or fewer and contain no control characters.".into());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aider_runtime_version_normalizes_prefixed_output() {
        assert_eq!(safe_version(b"aider 0.86.2\n"), Some("0.86.2".into()));
        assert_eq!(safe_version(b"0.86.2\n"), Some("0.86.2".into()));
    }

    #[test]
    fn openrouter_free_quota_parser_requires_consistent_bounded_counters() {
        let valid = serde_json::json!({"free_model_daily_requests":{"used":7,"limit":50,"remaining":43}, "rate_limit":{"limit":999}});
        assert_eq!(parse_free_model_daily_requests(&valid["data"]), None);
        let data = serde_json::json!({"free_model_daily_requests":{"used":7,"limit":50,"remaining":43}, "rate_limit":{"limit":999}});
        assert_eq!(
            parse_free_model_daily_requests(&data),
            Some(FreeModelDailyRequests {
                used: 7,
                limit: 50,
                remaining: 43
            })
        );
        let inconsistent =
            serde_json::json!({"free_model_daily_requests":{"used":7,"limit":50,"remaining":44}});
        assert_eq!(parse_free_model_daily_requests(&inconsistent), None);
        let oversized = serde_json::json!({"free_model_daily_requests":{"used":1,"limit":1_000_001,"remaining":1_000_000}});
        assert_eq!(parse_free_model_daily_requests(&oversized), None);
    }

    #[test]
    fn aider_probe_environment_isolated_from_user_config_and_external_model_cost_fetches() {
        let dir = tempfile::tempdir().unwrap();
        let env = aider_probe_environment(dir.path()).unwrap();
        let values = env
            .iter()
            .map(|(name, value)| (name.to_string_lossy(), value.to_string_lossy()))
            .collect::<std::collections::HashMap<_, _>>();
        assert_eq!(
            values.get("OPENAI_API_BASE").map(|value| value.as_ref()),
            Some("http://127.0.0.1:1/v1")
        );
        assert_eq!(
            values.get("OPENAI_API_KEY").map(|value| value.as_ref()),
            Some("fabrica-dummy")
        );
        assert_eq!(
            values
                .get("LITELLM_LOCAL_MODEL_COST_MAP")
                .map(|value| value.as_ref()),
            Some("True")
        );
        assert_eq!(
            values.get("PYTHONNOUSERSITE").map(|value| value.as_ref()),
            Some("1")
        );
        assert!(!values.contains_key("OPENROUTER_API_KEY"));
        assert!(!values.contains_key("ANTHROPIC_API_KEY"));
    }
    use std::ffi::OsStr;

    fn profile() -> ProviderProfile {
        ProviderProfile {
            id: "codex-local".into(),
            kind: ProviderKind::Codex,
            name: "Codex Local".into(),
            executable: "codex".into(),
            endpoint: None,
            secret_ref: Some("credential-store:codex-session".into()),
            models: vec![ModelProfile {
                id: "model-a".into(),
                name: "Model A".into(),
                reasoning_efforts: vec!["low".into(), "high".into()],
                context_window_tokens: None,
                supports_tools: None,
                quality_rating: None,
            }],
        }
    }

    #[test]
    fn runtime_identity_tracks_configured_powershell_entrypoint_bytes_without_running_it() {
        let directory = tempfile::tempdir().unwrap();
        let entrypoint = directory.path().join("streaming codex.ps1");
        std::fs::write(&entrypoint, b"synthetic script entrypoint bytes").unwrap();
        let mut configured = profile();
        configured.executable = entrypoint.to_string_lossy().into_owned();

        let original = runtime_identity_fingerprint(&configured).unwrap();
        assert_eq!(original, runtime_identity_fingerprint(&configured).unwrap());

        std::fs::write(&entrypoint, b"changed script entrypoint bytes").unwrap();
        assert_ne!(original, runtime_identity_fingerprint(&configured).unwrap());
    }

    #[test]
    fn openrouter_metadata_requires_execution_key_and_user_scoped_catalog() {
        let mut calls = Vec::new();
        let result = openrouter_metadata_probe_with("sk-or-v1-synthetic", |path, _| {
            calls.push(path.to_string());
            match path {
                "/key" => Ok(json!({"data": {
                    "is_management_key": false,
                    "is_provisioning_key": false,
                    "label": "must-not-escape",
                    "limit": 12345
                }})),
                "/models/user" => Ok(json!({"data": [{
                    "id": "vendor/model-a",
                    "name": "Synthetic Model A",
                    "context_length": 32768,
                    "supported_parameters": ["tools"],
                    "pricing": {"prompt": "0.000001", "completion": "0.000002"}
                }]})),
                _ => panic!("unexpected metadata endpoint: {path}"),
            }
        })
        .unwrap();

        assert_eq!(calls, ["/key", "/models/user"]);
        assert_eq!(result.authenticated_for_execution, Some(true));
        assert_eq!(result.models.len(), 1);
        assert_eq!(result.models[0].id, "vendor/model-a");
        let serialized = format!("{result:?}");
        assert!(!serialized.contains("must-not-escape"));
        assert!(!serialized.contains("12345"));
    }

    #[test]
    fn openrouter_management_and_provisioning_keys_never_query_models() {
        for (management, provisioning) in [(true, false), (false, true)] {
            let mut calls = Vec::new();
            let result = openrouter_metadata_probe_with("sk-or-v1-synthetic", |path, _| {
                calls.push(path.to_string());
                Ok(json!({"data": {
                    "is_management_key": management,
                    "is_provisioning_key": provisioning
                }}))
            })
            .unwrap();
            assert_eq!(result.authenticated_for_execution, Some(false));
            assert!(result.models.is_empty());
            assert_eq!(calls, ["/key"]);
        }
    }

    #[test]
    fn openrouter_metadata_missing_key_flags_stay_unknown_and_do_not_query_models() {
        let mut calls = Vec::new();
        let result = openrouter_metadata_probe_with("sk-or-v1-synthetic", |path, _| {
            calls.push(path.to_string());
            Ok(json!({"data": {"label": "synthetic"}}))
        })
        .unwrap();
        assert_eq!(result.authenticated_for_execution, None);
        assert!(result.models.is_empty());
        assert_eq!(calls, ["/key"]);
    }

    #[test]
    fn runtime_identity_deadline_is_fail_closed_before_file_inspection() {
        let mut configured = profile();
        configured.executable = "C:/path/that/must/not/be/inspected.exe".into();
        let deadline = Instant::now();
        assert!(runtime_identity_fingerprint_until(&configured, Some(deadline)).is_err());
    }

    #[test]
    fn old_model_settings_default_to_unknown_routing_metadata() {
        let model: ModelProfile = serde_json::from_value(serde_json::json!({
            "id": "legacy-model",
            "name": "Legacy model",
            "reasoningEfforts": ["high"]
        }))
        .unwrap();
        assert_eq!(model.context_window_tokens, None);
        assert_eq!(model.supports_tools, None);
        assert_eq!(model.quality_rating, None);
    }

    #[test]
    fn executable_discovery_uses_only_the_supplied_path() {
        let folder = tempfile::tempdir().unwrap();
        let executable = folder.path().join("codex");
        std::fs::write(&executable, "fixture executable").unwrap();
        let path = std::env::join_paths([folder.path()]).unwrap();
        let result = find_executable("codex", Some(path.as_os_str()));
        assert_eq!(result.as_deref(), Some(executable.as_path()));
        assert!(find_executable("missing", Some(path.as_os_str())).is_none());
        assert!(find_executable("codex", Some(OsStr::new(""))).is_none());
    }

    #[test]
    fn inspection_reports_reference_and_unsupported_operations_without_running_a_process() {
        let profile = profile();
        let status = inspect(&profile, None);
        assert!(!status.executable_found);
        assert_eq!(
            status.authentication_reference,
            AuthenticationReferenceStatus::ReferenceConfigured
        );
        assert!(status.capabilities.discovery);
        assert!(status.capabilities.run_start);
        assert!(status.capabilities.run_cancel);
        assert!(status.capabilities.event_stream);
        assert!(status.capabilities.approval_requests);
        assert!(status.capabilities.interactive_input);
        assert!(!status.capabilities.checkpoints);
        assert!(status.note.contains("never launches"));
    }

    #[test]
    fn profiles_reject_secret_values_and_invalid_selection() {
        let mut profile = profile();
        profile.secret_ref = Some("sk-secret-token-value".into());
        assert!(validate_profiles(&[profile.clone()], &ProviderSelection::default()).is_err());
        profile.secret_ref = Some("credential-store:codex-session".into());
        let invalid = ProviderSelection {
            provider_id: Some(profile.id.clone()),
            model_id: Some("missing".into()),
            ..ProviderSelection::default()
        };
        assert!(validate_profiles(&[profile], &invalid).is_err());
    }

    #[test]
    fn route_decision_provenance_is_validated_and_persistable() {
        let mut selection = ProviderSelection {
            route_source: Some("jev".into()),
            route_confidence_percent: Some(92),
            route_explanation: Some("High-confidence route.".into()),
            ..ProviderSelection::default()
        };
        assert!(validate_profiles(&[], &selection).is_ok());
        let round_trip: ProviderSelection =
            serde_json::from_value(serde_json::to_value(&selection).unwrap()).unwrap();
        assert_eq!(round_trip, selection);
        selection.route_confidence_percent = Some(101);
        assert!(validate_profiles(&[], &selection).is_err());
    }

    #[test]
    fn provider_profiles_serialize_only_a_secret_reference_field() {
        let value = serde_json::to_value(profile()).unwrap();
        assert_eq!(value["secretRef"], "credential-store:codex-session");
        assert!(value.get("secret").is_none());
    }

    #[test]
    fn antigravity_command_uses_documented_flags_and_keeps_prompt_as_one_argument() {
        let args = antigravity_arguments("gemini-test", "echo `danger`", 90);
        assert_eq!(
            args,
            vec![
                "--sandbox",
                "--print-timeout",
                "90s",
                "--model",
                "gemini-test",
                "--print",
                "echo `danger`"
            ]
        );
    }

    #[test]
    fn antigravity_capabilities_do_not_claim_interaction_support() {
        let mut profile = profile();
        profile.kind = ProviderKind::Antigravity;
        let status = inspect(&profile, None);
        assert!(status.capabilities.health);
        assert!(status.capabilities.model_listing);
        assert!(status.capabilities.skill_support);
        assert!(
            status.capabilities.run_start
                && status.capabilities.run_cancel
                && status.capabilities.event_stream
        );
        assert!(
            !status.capabilities.approval_requests
                && !status.capabilities.interactive_input
                && !status.capabilities.checkpoints
        );
    }

    #[test]
    fn only_implemented_provider_adapters_are_valid_fallback_targets() {
        assert!(ProviderKind::Codex.supports_workflow_run());
        assert!(ProviderKind::Antigravity.supports_workflow_run());
        assert!(ProviderKind::Ollama.supports_workflow_run());
        assert!(ProviderKind::OpenRouter.supports_workflow_run());
        assert!(!ProviderKind::Custom.supports_workflow_run());
    }

    #[test]
    fn ollama_profiles_accept_only_credential_free_numeric_loopback_endpoints() {
        assert!(validate_ollama_endpoint("http://127.0.0.1:11434").is_ok());
        assert!(validate_ollama_endpoint("http://[::1]:11434").is_ok());
        for endpoint in [
            "https://127.0.0.1:11434",
            "http://localhost:11434",
            "http://192.168.1.5:11434",
            "http://user:pass@127.0.0.1:11434",
            "http://127.0.0.1:11434/api",
        ] {
            assert!(
                validate_ollama_endpoint(endpoint).is_err(),
                "accepted {endpoint}"
            );
        }
        let mut local = profile();
        local.kind = ProviderKind::Ollama;
        local.secret_ref = None;
        local.endpoint = Some("http://127.0.0.1:11434".into());
        assert!(validate_profiles(&[local.clone()], &ProviderSelection::default()).is_ok());
        local.secret_ref = Some("credential-store:cloud-key".into());
        assert!(validate_profiles(&[local], &ProviderSelection::default()).is_err());
    }

    #[test]
    fn ollama_model_discovery_requires_reported_tool_support_and_context() {
        let tags = json!({"models": [
            {"name": "agent-ready", "size": 8_000_000_000u64},
            {"name": "no-tools", "size": 2_000_000_000u64},
            {"name": "unknown-context", "size": 2_000_000_000u64}
        ]});
        let details = vec![
            (
                "agent-ready".into(),
                json!({"capabilities": ["completion", "tools"], "model_info": {"model.context_length": 32768}}),
            ),
            (
                "no-tools".into(),
                json!({"capabilities": ["completion"], "model_info": {"model.context_length": 32768}}),
            ),
            (
                "unknown-context".into(),
                json!({"capabilities": ["completion", "tools"], "model_info": {}}),
            ),
        ];
        let models = ollama_models_from_responses(&tags, &details);
        assert_eq!(models.len(), 3);
        assert!(models[0].eligible);
        assert!(!models[1].eligible && !models[1].tools_supported);
        assert!(!models[2].eligible && models[2].context_tokens.is_none());
    }

    #[test]
    fn ollama_timeout_scales_with_available_memory_and_model_size() {
        assert_eq!(
            ollama_timeout_seconds(8_000_000_000, Some(8_000_000_000)),
            Some(1200)
        );
        assert!(ollama_timeout_seconds(8_000_000_000, Some(2_000_000_000)).unwrap() > 1200);
        assert_eq!(ollama_timeout_seconds(0, Some(8_000_000_000)), None);
        assert_eq!(ollama_timeout_seconds(8_000_000_000, None), None);
    }

    #[test]
    fn aider_invocation_pins_local_chat_model_and_disables_remote_side_effects() {
        let args = aider_ollama_arguments("qwen3", "Change the module", Some("cargo test"), "NUL");
        assert!(args
            .windows(2)
            .any(|pair| pair == ["--model", "ollama_chat/qwen3"]));
        assert!(args
            .windows(2)
            .any(|pair| pair == ["--message", "Change the module"]));
        assert!(!args.contains(&"--message-file".into()));
        assert!(args
            .windows(2)
            .any(|pair| pair == ["--test-cmd", "cargo test"]));
        assert!(args.contains(&"--no-auto-commits".into()));
        assert!(args.contains(&"--no-analytics".into()));
        assert!(args.windows(2).any(|pair| pair == ["--env-file", "NUL"]));
    }

    #[test]
    fn app_server_requests_pin_project_model_effort_and_skill() {
        let start = app_server_thread_start(2, r"C:\Projects\sample", "model-a");
        assert_eq!(start["method"], "thread/start");
        assert!(start.get("jsonrpc").is_none());
        assert_eq!(start["params"]["cwd"], r"C:\Projects\sample");
        assert_eq!(
            start["params"]["runtimeWorkspaceRoots"][0],
            r"C:\Projects\sample"
        );
        assert_eq!(start["params"]["model"], "model-a");
        assert_eq!(start["params"]["approvalPolicy"], "on-request");
        let turn = app_server_turn_start(
            3,
            "thread-id",
            r"C:\Projects\sample",
            "model-a",
            Some("high"),
            "Make this change",
            Some(("dev-work", r"C:\Users\me\.codex\skills\dev-work\SKILL.md")),
        );
        assert!(turn.get("jsonrpc").is_none());
        assert_eq!(turn["params"]["effort"], "high");
        assert_eq!(turn["params"]["input"][0]["type"], "skill");
        assert_eq!(turn["params"]["input"][1]["text"], "Make this change");
    }

    #[test]
    fn app_server_stream_normalizes_deltas_final_output_failures_and_requests() {
        let delta = serde_json::json!({
            "method": "item/agentMessage/delta",
            "params": { "delta": "Hello" }
        });
        let event = normalize_app_server_message(&delta).unwrap();
        assert_eq!(event.kind, "output");
        assert_eq!(event.text, "Hello");

        let final_message = serde_json::json!({
            "method": "item/completed",
            "params": { "item": { "type": "agentMessage", "text": "Final answer" } }
        });
        assert_eq!(
            normalize_app_server_message(&final_message).unwrap().kind,
            "finalOutput"
        );

        let auth_error = serde_json::json!({
            "method": "turn/completed",
            "params": { "turn": { "status": "failed", "error": { "message": "401 unauthorized" } } }
        });
        let event = normalize_app_server_message(&auth_error).unwrap();
        assert_eq!(event.terminal_state.as_deref(), Some("failed"));
        assert_eq!(event.text, "Codex authentication is required or expired.");

        let request = serde_json::json!({
            "id": 42,
            "method": "item/commandExecution/requestApproval",
            "params": { "command": "git status", "availableDecisions": ["accept", "decline"] }
        });
        let event = normalize_app_server_message(&request).unwrap();
        assert_eq!(event.kind, "approvalRequest");
        let display: serde_json::Value = serde_json::from_str(&event.text).unwrap();
        assert_eq!(display["requestId"], 42);
        assert_eq!(display["params"]["availableDecisions"][0], "accept");

        let permission_request = serde_json::json!({
            "id": 43,
            "method": "item/permissions/requestApproval",
            "params": { "reason": "Allow a workspace root", "permissions": { "fileSystem": { "write": ["/workspace"] } } }
        });
        let event = normalize_app_server_message(&permission_request).unwrap();
        let display: serde_json::Value = serde_json::from_str(&event.text).unwrap();
        assert_eq!(display["method"], "item/permissions/requestApproval");
        assert_eq!(
            display["params"]["permissions"]["fileSystem"]["write"][0],
            "/workspace"
        );
    }

    #[test]
    fn codex_json_rpc_request_errors_are_safely_classified() {
        for (detail, expected) in [
            (
                "Unauthorized: sign in",
                "Codex authentication is required or expired.",
            ),
            ("429 rate limit", "Codex rate limit was reached."),
            ("Model not found", "Codex model is unsupported."),
            (
                "private server detail",
                "Codex app-server reported an error.",
            ),
        ] {
            let response = serde_json::json!({
                "id": 7,
                "error": {"code": -32602, "message": detail}
            });
            let event = normalize_app_server_message(&response).unwrap();
            assert_eq!(event.kind, "providerError");
            assert_eq!(event.text, expected);
            assert_eq!(event.terminal_state.as_deref(), Some("failed"));
            assert!(!event.text.contains("private server detail"));
        }
    }

    #[test]
    fn app_server_parser_accepts_codex_wire_messages_without_jsonrpc() {
        assert!(parse_app_server_message("not-json").is_err());
        assert!(parse_app_server_message("[]").is_err());
        assert!(
            parse_app_server_message(r#"{"jsonrpc":"1.0","method":"turn/completed"}"#).is_err()
        );
        assert!(
            parse_app_server_message(r#"{"id":1,"result":{"userAgent":"codex_cli/0.162.1"}}"#)
                .is_ok()
        );
        assert!(parse_app_server_message(
            r#"{"method":"turn/completed","params":{"turn":{"status":"completed"}}}"#
        )
        .is_ok());
        assert!(parse_app_server_message(
            r#"{"jsonrpc":"2.0","method":"turn/completed","params":{}}"#
        )
        .is_ok());
    }

    #[test]
    fn app_server_messages_follow_codex_wire_shape() {
        let initialize = app_server_initialize(1, "0.14.0");
        let thread = app_server_thread_start(2, "/workspace", "model-a");
        let response = app_server_response(serde_json::json!(3), json!({}));
        assert!(initialize.get("jsonrpc").is_none());
        assert!(thread.get("jsonrpc").is_none());
        assert!(response.get("jsonrpc").is_none());
        assert_eq!(response["id"], 3);
    }

    #[test]
    fn normalizes_authentication_failures_from_runtime_stderr() {
        let event = normalize_codex_stderr("Unauthorized: please sign in").unwrap();
        assert_eq!(event.kind, "providerError");
        assert_eq!(event.terminal_state.as_deref(), Some("failed"));
        assert_eq!(event.text, "Codex authentication is required or expired.");
        assert!(normalize_codex_stderr("temporary filesystem warning").is_none());
    }

    #[test]
    fn openrouter_catalog_preserves_reported_metadata_and_unknowns() {
        let value = json!({"data": [
            {"id":"vendor/model-a", "name":"Model A", "context_length":32768,
             "pricing":{"prompt":"0.000001", "completion":"0.000002"},
             "architecture":{"input_modalities":["text"], "output_modalities":["text"]},
             "supported_parameters":["tools"]},
            {"id":"vendor/model-b", "name":"Model B", "pricing":{}, "architecture":{}},
            {"id":"bad", "name":""}
        ]});
        let models = parse_openrouter_models(&value);
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].context_length, Some(32768));
        assert_eq!(models[0].prompt_price.as_deref(), Some("0.000001"));
        assert_eq!(
            models[0].supported_parameters.as_deref(),
            Some(["tools".to_string()].as_slice())
        );
        assert_eq!(models[1].context_length, None);
        assert_eq!(models[1].prompt_price, None);
        assert_eq!(models[1].input_modalities, None);
    }

    #[test]
    fn openrouter_prices_convert_with_conservative_decimal_rounding() {
        assert_eq!(
            openrouter_price_micros_per_million("0.000001"),
            Some(1_000_000)
        );
        assert_eq!(
            openrouter_price_micros_per_million("0.0000000000001"),
            Some(1)
        );
        assert_eq!(openrouter_price_micros_per_million("0"), Some(0));
        for invalid in [
            "",
            "-0.1",
            "+0.1",
            ".1",
            "1.",
            "1e-6",
            "NaN",
            "18446744073709551616",
        ] {
            assert_eq!(
                openrouter_price_micros_per_million(invalid),
                None,
                "{invalid}"
            );
        }
    }

    #[test]
    fn openrouter_probe_maps_auth_rate_limit_and_malformed_catalog_without_leaking() {
        let rejected = openrouter_probe_with("sk-or-valid", |_| Err(401));
        assert_eq!(rejected.state, "authenticationRequired");
        let limited = openrouter_probe_with("sk-or-valid", |_| Err(429));
        assert_eq!(limited.state, "rateLimited");
        let malformed =
            openrouter_probe_with("sk-or-valid", |_| Ok(json!({"error":"sk-or-secret"})));
        assert_eq!(malformed.state, "unavailable");
        assert!(!malformed.note.contains("sk-or-secret"));
        assert_eq!(
            openrouter_probe_with("", |_| panic!("must not request")).state,
            "authenticationRequired"
        );
    }

    #[test]
    fn openrouter_credentials_are_validated_and_aider_receives_no_key_in_arguments() {
        assert_eq!(
            openrouter_credential_target("openRouter-a1b2").unwrap(),
            "navis-fabrica-openrouter-openRouter-a1b2"
        );
        assert!(openrouter_credential_target("../key").is_err());
        assert!(openrouter_credential_reference_matches(
            "openRouter-a1b2",
            "credential-store:navis-fabrica-openrouter-openRouter-a1b2"
        ));
        assert!(!openrouter_credential_reference_matches(
            "openRouter-a1b2",
            "credential-store:github-session"
        ));
        assert!(!openrouter_credential_reference_matches(
            "openRouter-a1b2",
            "provider-managed:navis-fabrica-openrouter-openRouter-a1b2"
        ));
        assert!(valid_openrouter_key("sk-or-v1-value"));
        assert!(!valid_openrouter_key("bad\nkey"));
        let args =
            aider_openrouter_arguments("vendor/model", "change files", Some("npm test"), "NUL");
        assert!(args
            .windows(2)
            .any(|pair| pair == ["--model", "openrouter/vendor/model"]));
        assert!(args.windows(2).any(|pair| pair == ["--env-file", "NUL"]));
        assert!(!args.iter().any(|arg| arg.contains("sk-or-")));
    }

    #[test]
    fn paid_gateway_aider_args_pin_one_openai_model_and_nonstreaming_metadata() {
        let args = aider_paid_gateway_arguments(
            "vendor/model",
            "C:/temp/prompt.txt",
            "C:/temp/model-metadata.json",
            "C:/temp/model-settings.yml",
            "C:/temp/empty-config.yml",
            "C:/temp/chat-history.md",
            "C:/temp/input-history",
            "NUL",
            None,
            &["C:/worktree/src/lib.rs".into(), "C:/worktree/new.rs".into()],
        );
        assert_eq!(
            args[0..6],
            [
                "--model",
                "openai/vendor/model",
                "--weak-model",
                "openai/vendor/model",
                "--editor-model",
                "openai/vendor/model"
            ]
        );
        assert!(args
            .windows(2)
            .any(|items| items == ["--message-file", "C:/temp/prompt.txt"]));
        assert!(args
            .windows(2)
            .any(|items| items == ["--model-metadata-file", "C:/temp/model-metadata.json"]));
        assert!(args
            .windows(2)
            .any(|items| items == ["--model-settings-file", "C:/temp/model-settings.yml"]));
        assert!(args
            .windows(2)
            .any(|items| items == ["--config", "C:/temp/empty-config.yml"]));
        assert!(args.iter().any(|argument| argument == "--no-fancy-input"));
        assert!(args.iter().any(|argument| argument == "--no-pretty"));
        assert_eq!(
            &args[args.len() - 2..],
            ["C:/worktree/src/lib.rs", "C:/worktree/new.rs"]
        );
        assert!(args
            .windows(2)
            .any(|items| items == ["--chat-history-file", "C:/temp/chat-history.md"]));
        assert!(args
            .windows(2)
            .any(|items| items == ["--input-history-file", "C:/temp/input-history"]));
        assert!(args.contains(&"--no-stream".into()));
        assert!(!args.contains(&"--yes".into()));
        assert!(args.contains(&"--no-git".into()));
        assert!(args.contains(&"--no-auto-lint".into()));
        assert!(args.contains(&"--no-auto-test".into()));
        assert!(args.windows(2).any(|pair| pair == ["--map-tokens", "0"]));
        assert!(args
            .windows(2)
            .any(|pair| pair == ["--edit-format", "diff"]));
        assert!(args.contains(&"--no-show-model-warnings".into()));
        assert!(args.contains(&"--no-browser".into()));
        assert!(args.contains(&"--no-show-release-notes".into()));
        assert!(!args
            .iter()
            .any(|argument| argument.starts_with("openrouter/")));
        assert!(!args.iter().any(|argument| argument.contains("sk-or-")));
    }

    #[test]
    fn openrouter_catalog_limit_matches_settings_validation() {
        let models: Vec<ModelProfile> = (0..OPENROUTER_MAX_MODELS)
            .map(|index| ModelProfile {
                id: format!("model-{index}"),
                name: format!("Model {index}"),
                reasoning_efforts: Vec::new(),
                context_window_tokens: None,
                supports_tools: None,
                quality_rating: None,
            })
            .collect();
        let mut provider = profile();
        provider.id = "openRouter-a1b2".into();
        provider.kind = ProviderKind::OpenRouter;
        provider.secret_ref =
            Some("credential-store:navis-fabrica-openrouter-openRouter-a1b2".into());
        provider.models = models;
        assert!(validate_profiles(&[provider.clone()], &ProviderSelection::default()).is_ok());
        provider.models.push(ModelProfile {
            id: "model-over-limit".into(),
            name: "Model over limit".into(),
            reasoning_efforts: Vec::new(),
            context_window_tokens: None,
            supports_tools: None,
            quality_rating: None,
        });
        assert!(validate_profiles(&[provider], &ProviderSelection::default()).is_err());
    }

    #[test]
    fn typesafe_credentials_are_bound_to_profile_and_never_serialized_as_keys() {
        let reference = "credential-store:navis-fabrica-typesafe-jev-main";
        assert_eq!(
            typesafe_credential_target("jev-main").unwrap(),
            "navis-fabrica-typesafe-jev-main"
        );
        assert!(typesafe_credential_reference_matches("jev-main", reference));
        assert!(!typesafe_credential_reference_matches("other", reference));
        assert!(!typesafe_credential_reference_matches("../key", reference));
        assert!(valid_typesafe_key("ts_live_example"));
        assert!(!valid_typesafe_key("key\nvalue"));
        let profile = TypeSafeProfile {
            id: "jev-main".into(),
            enabled: true,
            credential_ref: Some(reference.into()),
        };
        let serialized = serde_json::to_string(&profile).unwrap();
        assert!(serialized.contains(reference));
        assert!(!serialized.contains("ts_live_example"));
    }

    #[test]
    fn typesafe_probe_covers_profile_and_service_states_without_leaking() {
        let profile_id = "jev-main";
        let reference = "credential-store:navis-fabrica-typesafe-jev-main";
        let disabled =
            typesafe_probe_with(false, profile_id, Some(reference), Some("secret"), |_| {
                panic!("disabled profile must not make a request")
            });
        assert_eq!(disabled.state, "disabled");
        let missing = typesafe_probe_with(true, profile_id, None, None, |_| {
            panic!("missing credential must not make a request")
        });
        assert_eq!(missing.state, "unconfigured");
        let mismatched = typesafe_probe_with(
            true,
            profile_id,
            Some("credential-store:github-token"),
            Some("secret"),
            |_| panic!("mismatched credential must not be looked up or requested"),
        );
        assert_eq!(mismatched.state, "unconfigured");

        let healthy = typesafe_probe_with(
            true,
            profile_id,
            Some(reference),
            Some("ts_live_example"),
            |key| {
                assert_eq!(key, "ts_live_example");
                Ok(json!({"models":[{"name":"jev-latest"}]}))
            },
        );
        assert_eq!(healthy.state, "healthy");
        assert_eq!(healthy.model, "jev-1.14.0");

        for (status, expected) in [
            (401, "authenticationFailed"),
            (403, "authenticationFailed"),
            (429, "rateLimited"),
            (0, "unavailable"),
        ] {
            let probe = typesafe_probe_with(
                true,
                profile_id,
                Some(reference),
                Some("ts_live_example"),
                |_| Err(status),
            );
            assert_eq!(probe.state, expected);
            assert!(!probe.note.contains("ts_live_example"));
        }
        let malformed = typesafe_probe_with(
            true,
            profile_id,
            Some(reference),
            Some("ts_live_example"),
            |_| Ok(json!({"error":"ts_live_example"})),
        );
        assert_eq!(malformed.state, "unavailable");
        assert!(!malformed.note.contains("ts_live_example"));
        let malformed_catalog = typesafe_probe_with(
            true,
            profile_id,
            Some(reference),
            Some("ts_live_example"),
            |_| Ok(json!({"error":"invalid catalog"})),
        );
        assert_eq!(malformed_catalog.state, "unavailable");
    }
}
