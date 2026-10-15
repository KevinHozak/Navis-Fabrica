use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_SIGNALS: usize = 500;
const SUBSCRIPTION_MAX_AGE_MS: u128 = 30 * 60 * 1000;
const CONTEXT_MAX_AGE_MS: u128 = 4 * 60 * 60 * 1000;
const LOCAL_MAX_AGE_MS: u128 = 5 * 60 * 1000;
const MAX_JEV_ROUTING_HISTORY: usize = 100;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JevRoutingHistoryEntry {
    pub evaluated_at_ms: u128,
    pub source: String,
    pub selected_provider_id: Option<String>,
    pub selected_model_id: Option<String>,
    pub deterministic_provider_id: Option<String>,
    pub deterministic_model_id: Option<String>,
    pub confidence: Option<f64>,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub estimated_cost_usd: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum UsageCategory {
    SubscriptionWindow,
    ApiSpend,
    ContextLimit,
    LocalResource,
}

impl UsageCategory {
    fn as_str(self) -> &'static str {
        match self {
            Self::SubscriptionWindow => "subscriptionWindow",
            Self::ApiSpend => "apiSpend",
            Self::ContextLimit => "contextLimit",
            Self::LocalResource => "localResource",
        }
    }

    fn max_age_ms(self) -> u128 {
        match self {
            Self::SubscriptionWindow => SUBSCRIPTION_MAX_AGE_MS,
            Self::ApiSpend => 24 * 60 * 60 * 1000,
            Self::ContextLimit => CONTEXT_MAX_AGE_MS,
            Self::LocalResource => LOCAL_MAX_AGE_MS,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum UsageStatus {
    Available,
    Estimated,
    Unknown,
    Stale,
    Unavailable,
    RateLimited,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageSignal {
    pub id: String,
    pub provider_id: Option<String>,
    pub model_id: Option<String>,
    pub category: UsageCategory,
    pub metric: String,
    pub label: String,
    pub value: Option<f64>,
    pub unit: String,
    pub window_duration_seconds: Option<u64>,
    pub reset_at_ms: Option<u128>,
    pub collected_at_ms: u128,
    pub source: String,
    pub confidence: String,
    pub status: UsageStatus,
    pub note: Option<String>,
}

pub fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

pub fn latency_ms(started_at_ms: u128, finished_at_ms: u128) -> u128 {
    finished_at_ms.saturating_sub(started_at_ms)
}

fn number(value: Option<&Value>) -> Option<f64> {
    value
        .and_then(Value::as_f64)
        .filter(|value| value.is_finite() && *value >= 0.0)
}

fn reset_epoch_ms(value: Option<&Value>) -> Option<u128> {
    let seconds = value?.as_u64()?;
    (seconds > 0).then(|| u128::from(seconds) * 1000)
}

fn window_duration_seconds(value: Option<&Value>) -> Option<u64> {
    let minutes = value?.as_u64()?;
    (minutes > 0 && minutes <= 60 * 24 * 366).then(|| minutes.saturating_mul(60))
}

pub fn signals_from_codex_rate_limits(
    response: &Value,
    provider_id: &str,
    model_id: &str,
    collected_at_ms: u128,
) -> Vec<UsageSignal> {
    let result = response.get("result").unwrap_or(response);
    let snapshots = result
        .get("rateLimitsByLimitId")
        .and_then(Value::as_object)
        .cloned()
        .or_else(|| {
            result
                .get("rateLimits")
                .and_then(Value::as_object)
                .map(|snapshot| {
                    let key = snapshot
                        .get("limitId")
                        .and_then(Value::as_str)
                        .unwrap_or("codex");
                    [(key.to_owned(), Value::Object(snapshot.clone()))]
                        .into_iter()
                        .collect()
                })
        });
    let Some(snapshots) = snapshots else {
        let (status, note) = if response.get("error").is_some() {
            classify_error(response)
        } else {
            (
                UsageStatus::Unavailable,
                "Codex did not return rate-limit windows.".to_owned(),
            )
        };
        return vec![UsageSignal {
            id: format!("{}:subscription:status", safe_identity(provider_id)),
            provider_id: Some(safe_identity(provider_id)),
            model_id: Some(safe_identity(model_id)),
            category: UsageCategory::SubscriptionWindow,
            metric: "usagePercent".into(),
            label: "Codex subscription window".into(),
            value: None,
            unit: "% used".into(),
            window_duration_seconds: None,
            reset_at_ms: None,
            collected_at_ms,
            source: "codex.app-server.account/rateLimits/read".into(),
            confidence: "measured".into(),
            status,
            note: Some(note),
        }];
    };

    let mut signals = Vec::new();
    for (limit_id, snapshot) in snapshots {
        let safe_limit_id = safe_id_part(&limit_id);
        if safe_limit_id.is_empty() {
            continue;
        }
        for slot in ["primary", "secondary"] {
            let Some(window) = snapshot.get(slot).filter(|value| value.is_object()) else {
                continue;
            };
            let suffix = format!("{safe_limit_id}:{slot}");
            signals.push(UsageSignal {
                id: format!("{}:subscription:{suffix}", safe_identity(provider_id)),
                provider_id: Some(safe_identity(provider_id)),
                model_id: Some(safe_identity(model_id)),
                category: UsageCategory::SubscriptionWindow,
                metric: format!("{}UsagePercent", slot),
                label: format!("{safe_limit_id} {slot} window"),
                value: number(window.get("usedPercent")).filter(|value| *value <= 100.0),
                unit: "% used".into(),
                window_duration_seconds: window_duration_seconds(window.get("windowDurationMins")),
                reset_at_ms: reset_epoch_ms(window.get("resetsAt")),
                collected_at_ms,
                source: "codex.app-server.account/rateLimits/read".into(),
                confidence: "measured".into(),
                status: if number(window.get("usedPercent")).is_some_and(|value| value <= 100.0) {
                    UsageStatus::Available
                } else {
                    UsageStatus::Unknown
                },
                note: None,
            });
        }
    }
    if signals.is_empty() {
        signals.push(UsageSignal {
            id: format!("{provider_id}:subscription:unknown"),
            provider_id: Some(safe_identity(provider_id)),
            model_id: Some(safe_identity(model_id)),
            category: UsageCategory::SubscriptionWindow,
            metric: "usagePercent".into(),
            label: "Codex subscription window".into(),
            value: None,
            unit: "% used".into(),
            window_duration_seconds: None,
            reset_at_ms: None,
            collected_at_ms,
            source: "codex.app-server.account/rateLimits/read".into(),
            confidence: "measured".into(),
            status: UsageStatus::Unknown,
            note: Some("The provider returned no populated windows; usage remains unknown.".into()),
        });
    }
    signals
}

fn classify_error(response: &Value) -> (UsageStatus, String) {
    let message = response
        .get("error")
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_ascii_lowercase();
    if message.contains("rate limit") || message.contains("too many requests") {
        (
            UsageStatus::RateLimited,
            "Codex rate-limit telemetry request was rate limited.".into(),
        )
    } else {
        (
            UsageStatus::Unavailable,
            "Codex rate-limit telemetry is unavailable for this session.".into(),
        )
    }
}

fn safe_id_part(value: &str) -> String {
    value
        .chars()
        .filter(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
        .take(64)
        .collect()
}

pub fn safe_identity(value: &str) -> String {
    let lowered = value.to_ascii_lowercase();
    let looks_secret = ["sk-", "ghp_", "gho_", "github_pat_", "bearer "]
        .iter()
        .any(|prefix| lowered.starts_with(prefix));
    if looks_secret
        || value.is_empty()
        || value.len() > 160
        || !value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "._-".contains(character))
    {
        "unknown".into()
    } else {
        value.to_owned()
    }
}

pub fn signals_from_codex_token_usage(
    message: &Value,
    provider_id: &str,
    model_id: &str,
    collected_at_ms: u128,
) -> Vec<UsageSignal> {
    let params = message.get("params").unwrap_or(message);
    let usage = params.get("tokenUsage").unwrap_or(params);
    let last = usage.get("last").unwrap_or(usage);
    let mut signals = Vec::new();
    if let Some(value) = number(last.get("totalTokens")) {
        signals.push(token_signal(
            provider_id,
            model_id,
            "usedTokens",
            "Context tokens used",
            value,
            collected_at_ms,
        ));
    }
    if let Some(value) = number(usage.get("modelContextWindow")) {
        signals.push(token_signal(
            provider_id,
            model_id,
            "contextWindowTokens",
            "Context window limit",
            value,
            collected_at_ms,
        ));
    }
    signals
}

fn token_signal(
    provider_id: &str,
    model_id: &str,
    metric: &str,
    label: &str,
    value: f64,
    collected_at_ms: u128,
) -> UsageSignal {
    UsageSignal {
        id: format!(
            "{}:context:{}:{metric}",
            safe_identity(provider_id),
            safe_identity(model_id)
        ),
        provider_id: Some(safe_identity(provider_id)),
        model_id: Some(safe_identity(model_id)),
        category: UsageCategory::ContextLimit,
        metric: metric.into(),
        label: label.into(),
        value: Some(value),
        unit: "tokens".into(),
        window_duration_seconds: None,
        reset_at_ms: None,
        collected_at_ms,
        source: "codex.app-server.thread/tokenUsage/updated".into(),
        confidence: "measured".into(),
        status: UsageStatus::Available,
        note: None,
    }
}

pub fn validate_manual_estimate(signal: &UsageSignal) -> Result<(), String> {
    if signal.id.trim().is_empty()
        || signal.id.len() > 160
        || signal.metric.trim().is_empty()
        || signal.metric.len() > 80
        || signal.label.trim().is_empty()
        || signal.label.len() > 120
        || signal.unit.trim().is_empty()
        || signal.unit.len() > 40
        || !signal
            .unit
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "% /._-".contains(character))
        || signal
            .value
            .is_none_or(|value| !value.is_finite() || value < 0.0)
        || signal
            .provider_id
            .as_ref()
            .is_some_and(|value| value.len() > 100 || value.chars().any(char::is_control))
        || signal
            .model_id
            .as_ref()
            .is_some_and(|value| value.len() > 160 || value.chars().any(char::is_control))
        || signal.collected_at_ms > now_ms().saturating_add(60_000)
    {
        return Err(
            "Enter a valid, non-negative usage estimate with a unit and short label.".into(),
        );
    }
    Ok(())
}

pub fn manual_estimate(
    category: UsageCategory,
    value: f64,
    unit: String,
    window_duration_seconds: Option<u64>,
    reset_at_ms: Option<u128>,
    collected_at_ms: u128,
) -> Result<UsageSignal, String> {
    let label = match category {
        UsageCategory::SubscriptionWindow => "Subscription window estimate",
        UsageCategory::ApiSpend => "API spend estimate",
        UsageCategory::ContextLimit => "Context limit estimate",
        UsageCategory::LocalResource => "Local resource estimate",
    };
    let signal = UsageSignal {
        id: format!("manual:{}", category.as_str()),
        provider_id: None,
        model_id: None,
        category,
        metric: "manualEstimate".into(),
        label: label.into(),
        value: Some(value),
        unit,
        window_duration_seconds,
        reset_at_ms,
        collected_at_ms,
        source: "User-entered manual estimate".into(),
        confidence: "estimated".into(),
        status: UsageStatus::Estimated,
        note: Some("Estimate entered manually; it is not provider-measured.".into()),
    };
    validate_manual_estimate(&signal)?;
    Ok(signal)
}

pub fn record_signals(path: &Path, signals: &[UsageSignal]) -> Result<(), String> {
    if signals.is_empty() {
        return Ok(());
    }
    let connection =
        Connection::open(path).map_err(|_| "Could not open usage history.".to_string())?;
    connection
        .busy_timeout(std::time::Duration::from_secs(2))
        .map_err(|_| "Could not configure usage history.".to_string())?;
    connection
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS usage_signals (
               id TEXT PRIMARY KEY,
               collected_at_ms INTEGER NOT NULL,
               category TEXT NOT NULL,
               record_json TEXT NOT NULL
             );",
        )
        .map_err(|_| "Could not initialize usage history.".to_string())?;
    let transaction = connection
        .unchecked_transaction()
        .map_err(|_| "Could not begin usage history update.".to_string())?;
    for signal in signals {
        let json = serde_json::to_string(signal)
            .map_err(|_| "Could not encode usage signal.".to_string())?;
        transaction
            .execute(
                "INSERT OR REPLACE INTO usage_signals (id, collected_at_ms, category, record_json) VALUES (?1, ?2, ?3, ?4)",
                params![signal.id, signal.collected_at_ms as i64, signal.category.as_str(), json],
            )
            .map_err(|_| "Could not save usage signal.".to_string())?;
    }
    transaction
        .execute(
            "DELETE FROM usage_signals WHERE id NOT IN (SELECT id FROM usage_signals ORDER BY collected_at_ms DESC LIMIT ?1)",
            [MAX_SIGNALS as i64],
        )
        .map_err(|_| "Could not trim usage history.".to_string())?;
    transaction
        .commit()
        .map_err(|_| "Could not commit usage history.".to_string())
}

pub fn record_jev_routing(path: &Path, entry: &JevRoutingHistoryEntry) -> Result<(), String> {
    let connection =
        Connection::open(path).map_err(|_| "Could not open routing history.".to_string())?;
    connection
        .busy_timeout(std::time::Duration::from_secs(2))
        .map_err(|_| "Could not configure routing history.".to_string())?;
    connection
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS jev_routing_history (
               id INTEGER PRIMARY KEY AUTOINCREMENT,
               evaluated_at_ms INTEGER NOT NULL,
               record_json TEXT NOT NULL
             );",
        )
        .map_err(|_| "Could not initialize routing history.".to_string())?;
    let json = serde_json::to_string(entry)
        .map_err(|_| "Could not encode routing history.".to_string())?;
    connection
        .execute(
            "INSERT INTO jev_routing_history (evaluated_at_ms, record_json) VALUES (?1, ?2)",
            params![entry.evaluated_at_ms as i64, json],
        )
        .map_err(|_| "Could not save routing history.".to_string())?;
    connection
        .execute(
            "DELETE FROM jev_routing_history WHERE id NOT IN (SELECT id FROM jev_routing_history ORDER BY evaluated_at_ms DESC, id DESC LIMIT ?1)",
            [MAX_JEV_ROUTING_HISTORY as i64],
        )
        .map_err(|_| "Could not trim routing history.".to_string())?;
    Ok(())
}

pub fn list_jev_routing_history(path: &Path) -> Result<Vec<JevRoutingHistoryEntry>, String> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let connection =
        Connection::open(path).map_err(|_| "Could not open routing history.".to_string())?;
    let table_exists = connection
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name='jev_routing_history'",
            [],
            |_| Ok(()),
        )
        .is_ok();
    if !table_exists {
        return Ok(Vec::new());
    }
    let mut statement = connection
        .prepare("SELECT record_json FROM jev_routing_history ORDER BY evaluated_at_ms DESC, id DESC LIMIT ?1")
        .map_err(|_| "Could not read routing history.".to_string())?;
    let rows = statement
        .query_map([MAX_JEV_ROUTING_HISTORY as i64], |row| {
            row.get::<_, String>(0)
        })
        .map_err(|_| "Could not read routing history.".to_string())?;
    rows.map(|row| {
        let json = row.map_err(|_| "Could not read routing history row.".to_string())?;
        serde_json::from_str(&json)
            .map_err(|_| "Routing history contains a malformed record.".to_string())
    })
    .collect()
}

pub fn list_signals(path: &Path, at_ms: u128) -> Result<Vec<UsageSignal>, String> {
    if !path.exists() {
        return Ok(default_unknown_signals(at_ms));
    }
    let connection =
        Connection::open(path).map_err(|_| "Could not open usage history.".to_string())?;
    connection
        .busy_timeout(std::time::Duration::from_secs(2))
        .map_err(|_| "Could not configure usage history.".to_string())?;
    let table_exists = connection
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name='usage_signals'",
            [],
            |_| Ok(()),
        )
        .is_ok();
    if !table_exists {
        return Ok(default_unknown_signals(at_ms));
    }
    let mut statement = connection
        .prepare("SELECT record_json FROM usage_signals ORDER BY collected_at_ms DESC LIMIT ?1")
        .map_err(|_| "Could not read usage history.".to_string())?;
    let rows = statement
        .query_map([MAX_SIGNALS as i64], |row| row.get::<_, String>(0))
        .map_err(|_| "Could not read usage history.".to_string())?;
    let mut signals = Vec::new();
    for row in rows {
        let json = row.map_err(|_| "Could not read usage history row.".to_string())?;
        let mut signal: UsageSignal = serde_json::from_str(&json)
            .map_err(|_| "Usage history contains a malformed record.".to_string())?;
        if matches!(
            signal.status,
            UsageStatus::Available | UsageStatus::Estimated
        ) && at_ms.saturating_sub(signal.collected_at_ms) > signal.category.max_age_ms()
        {
            signal.status = UsageStatus::Stale;
            signal.note =
                Some("The last reading is older than this signal's freshness window.".into());
        }
        signals.push(signal);
    }
    let present = signals
        .iter()
        .map(|signal| signal.category)
        .collect::<Vec<_>>();
    for missing in default_unknown_signals(at_ms)
        .into_iter()
        .filter(|signal| !present.contains(&signal.category))
    {
        signals.push(missing);
    }
    Ok(signals)
}

fn default_unknown_signals(at_ms: u128) -> Vec<UsageSignal> {
    [
        (UsageCategory::SubscriptionWindow, "Subscription windows"),
        (UsageCategory::ApiSpend, "API spend"),
        (UsageCategory::ContextLimit, "Context limits"),
        (UsageCategory::LocalResource, "Local resources"),
    ]
    .into_iter()
    .map(|(category, label)| UsageSignal {
        id: format!("unknown:{}", category.as_str()),
        provider_id: None,
        model_id: None,
        category,
        metric: "value".into(),
        label: label.into(),
        value: None,
        unit: "unknown".into(),
        window_duration_seconds: None,
        reset_at_ms: None,
        collected_at_ms: at_ms,
        source: "No supported signal collected".into(),
        confidence: "unknown".into(),
        status: UsageStatus::Unknown,
        note: Some("This value is not inferred from another usage category.".into()),
    })
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn normalizes_subscription_reset_times_and_keeps_categories_separate() {
        let response = serde_json::json!({
            "result": { "rateLimitsByLimitId": { "codex": {
                "primary": { "usedPercent": 38.5, "windowDurationMins": 300, "resetsAt": 1_800_000_000 }
            }}}
        });
        let signals = signals_from_codex_rate_limits(&response, "codex-home", "model-x", 1000);
        assert_eq!(signals.len(), 1);
        assert_eq!(signals[0].category, UsageCategory::SubscriptionWindow);
        assert_eq!(signals[0].value, Some(38.5));
        assert_eq!(signals[0].window_duration_seconds, Some(18_000));
        assert_eq!(signals[0].reset_at_ms, Some(1_800_000_000_000));
    }

    #[test]
    fn unavailable_rate_limited_and_missing_windows_do_not_become_zero() {
        let limited = serde_json::json!({"error":{"message":"rate limit reached"}});
        let unavailable = serde_json::json!({"error":{"message":"not supported"}});
        let missing = serde_json::json!({"result":{"rateLimits":{"limitId":"codex"}}});
        assert_eq!(
            signals_from_codex_rate_limits(&limited, "p", "m", 1)[0].status,
            UsageStatus::RateLimited
        );
        assert_eq!(
            signals_from_codex_rate_limits(&unavailable, "p", "m", 1)[0].status,
            UsageStatus::Unavailable
        );
        let unknown = signals_from_codex_rate_limits(&missing, "p", "m", 1);
        assert_eq!(unknown[0].status, UsageStatus::Unknown);
        assert_eq!(unknown[0].value, None);
    }

    #[test]
    fn saved_signals_age_to_stale_and_unsupplied_categories_remain_unknown() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("usage.sqlite3");
        let measured = UsageSignal {
            id: "codex:primary".into(),
            provider_id: Some("codex".into()),
            model_id: Some("m".into()),
            category: UsageCategory::SubscriptionWindow,
            metric: "usagePercent".into(),
            label: "Primary".into(),
            value: Some(50.0),
            unit: "% used".into(),
            window_duration_seconds: Some(300),
            reset_at_ms: Some(50_000),
            collected_at_ms: 1_000,
            source: "fixture".into(),
            confidence: "measured".into(),
            status: UsageStatus::Available,
            note: None,
        };
        record_signals(&path, &[measured]).unwrap();
        let signals = list_signals(&path, 31 * 60 * 1000).unwrap();
        assert_eq!(
            signals
                .iter()
                .find(|signal| signal.id == "codex:primary")
                .unwrap()
                .status,
            UsageStatus::Stale
        );
        assert!(signals
            .iter()
            .any(|signal| signal.category == UsageCategory::ApiSpend
                && signal.status == UsageStatus::Unknown));
    }

    #[test]
    fn manual_estimates_require_finite_nonnegative_values_and_units() {
        let mut signal = UsageSignal {
            id: "manual:api-spend".into(),
            provider_id: None,
            model_id: None,
            category: UsageCategory::ApiSpend,
            metric: "monthlySpend".into(),
            label: "Monthly API spend".into(),
            value: Some(12.5),
            unit: "USD".into(),
            window_duration_seconds: Some(2_592_000),
            reset_at_ms: None,
            collected_at_ms: now_ms(),
            source: "manual estimate".into(),
            confidence: "estimated".into(),
            status: UsageStatus::Estimated,
            note: None,
        };
        assert!(validate_manual_estimate(&signal).is_ok());
        signal.value = Some(f64::NAN);
        assert!(validate_manual_estimate(&signal).is_err());
    }

    #[test]
    fn jev_history_persists_routing_and_usage_without_task_content() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("usage.sqlite3");
        let entry = JevRoutingHistoryEntry {
            evaluated_at_ms: 1234,
            source: "jev".into(),
            selected_provider_id: Some("codex".into()),
            selected_model_id: Some("model-a".into()),
            deterministic_provider_id: Some("ollama".into()),
            deterministic_model_id: Some("model-b".into()),
            confidence: Some(0.91),
            input_tokens: Some(1_000_000),
            output_tokens: Some(250),
            estimated_cost_usd: Some(0.042),
        };
        record_jev_routing(&path, &entry).unwrap();
        let history = list_jev_routing_history(&path).unwrap();
        assert_eq!(history, vec![entry]);
        let connection = Connection::open(&path).unwrap();
        let stored: String = connection
            .query_row("SELECT record_json FROM jev_routing_history", [], |row| {
                row.get(0)
            })
            .unwrap();
        for forbidden in ["taskSummary", "repositoryContent", "diff", "apiKey"] {
            assert!(!stored.contains(forbidden));
        }
    }

    #[test]
    fn run_latency_uses_start_and_finish_epoch_milliseconds() {
        assert_eq!(latency_ms(1_000, 3_500), 2_500);
        assert_eq!(latency_ms(3_500, 1_000), 0);
    }

    #[test]
    fn telemetry_never_persists_token_like_model_identifiers() {
        let message = serde_json::json!({
            "method": "thread/tokenUsage/updated",
            "params": { "tokenUsage": { "last": { "totalTokens": 12 }, "modelContextWindow": 1000 } }
        });
        let signals = signals_from_codex_token_usage(&message, "codex", "sk-secret-value", 10);
        assert!(signals
            .iter()
            .all(|signal| signal.model_id.as_deref() == Some("unknown")));
        assert!(signals.iter().all(|signal| !serde_json::to_string(signal)
            .unwrap()
            .contains("sk-secret-value")));
    }
}
