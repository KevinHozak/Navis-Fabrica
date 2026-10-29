use std::{
    collections::{BTreeMap, BTreeSet},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use tauri::AppHandle;

use crate::{load_settings, providers, resolve_openrouter_credential, settings_path};
use navis_fabrica_runner_core::economy::{self, EconomyModelStatus};

const CATALOG_TIMEOUT: Duration = Duration::from_secs(12);

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FreeModelDailyRequests {
    pub used: u64,
    pub limit: u64,
    pub remaining: u64,
    pub observed_at_ms: u64,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EconomyStatusResponse {
    pub models: Vec<EconomyModelStatus>,
    pub free_model_daily_requests: Option<FreeModelDailyRequests>,
    pub catalog_observed_at_ms: u64,
}

/// Reads current, user-authenticated OpenRouter metadata only. Network work is
/// isolated from Tauri's async executor and bounded by the core probe deadline.
#[tauri::command]
pub async fn get_economy_model_status(
    app: AppHandle,
    provider_id: String,
) -> Result<EconomyStatusResponse, String> {
    if provider_id.is_empty() || provider_id.len() > 80 {
        return Err("Choose a valid OpenRouter provider profile.".into());
    }
    let settings = load_settings(&settings_path(&app)?)?;
    let profile = settings
        .providers
        .into_iter()
        .find(|profile| profile.id == provider_id)
        .ok_or_else(|| "The OpenRouter provider profile no longer exists.".to_string())?;
    if profile.kind != providers::ProviderKind::OpenRouter {
        return Err("Choose an OpenRouter provider profile.".into());
    }
    let runtime_identity_fingerprint = navis_fabrica_runner_core::providers::runtime_identity_fingerprint(&profile)
        .unwrap_or_default();
    let key = resolve_openrouter_credential(&profile.id, profile.secret_ref.as_deref())
        .ok_or_else(|| "Save a valid OpenRouter credential for this profile first.".to_string())?;
    let provider_id = profile.id;
    let runtime_executable = profile.executable;
    let endpoint_key = key.clone();

    let status_deadline = Instant::now() + CATALOG_TIMEOUT;
    let catalog_probe = tauri::async_runtime::spawn_blocking(move || {
        providers::openrouter_metadata_http_probe_with_timeout(&key, CATALOG_TIMEOUT)
    });
    let runtime_probe = tauri::async_runtime::spawn_blocking(move || {
        let path = std::env::var_os("PATH");
        providers::find_executable(&runtime_executable, path.as_deref())
            .and_then(|executable| providers::probe_aider_runtime_version(&executable))
    });

    let observation_result = catalog_probe
        .await
        .map_err(|_| "OpenRouter model status check did not complete.".to_string())?;
    let runtime_result = runtime_probe
        .await
        .map_err(|_| "Aider runtime check did not complete.".to_string())?;
    let observation = observation_result?;
    let runtime_version = runtime_result.unwrap_or_else(|| "unverified".into());
    if observation.authenticated_for_execution != Some(true) {
        return Err(
            "OpenRouter execution access could not be verified for this credential.".into(),
        );
    }
    let observed_at = now_ms();
    let passing_ids =
        economy::trusted_economy_candidate_ids("aider", &runtime_version, observed_at);
    let paid_candidate_canonicals = economy::paid_economy_candidate_models(
        "aider",
        &runtime_version,
        observed_at,
    )
    .into_iter()
    .map(|(model_id, canonical_model_id, _)| (model_id, canonical_model_id))
    .collect::<BTreeMap<_, _>>();
    let catalog_ids = observation
        .models
        .iter()
        .map(|model| model.id.as_str())
        .collect::<BTreeSet<_>>();
    let mut endpoint_model_ids = passing_ids.into_iter().collect::<BTreeSet<_>>();
    endpoint_model_ids.extend(paid_candidate_canonicals.keys().cloned());
    let proof_ids = endpoint_model_ids
        .into_iter()
        .filter(|model_id| catalog_ids.contains(model_id.as_str()))
        .take(16)
        .collect::<Vec<_>>();
    let (endpoints_by_model, zdr_endpoints) = if proof_ids.is_empty() {
        (BTreeMap::new(), BTreeSet::new())
    } else {
        let proof_key = endpoint_key;
        let paid_candidate_canonicals_for_probe = paid_candidate_canonicals.clone();
        let remaining = status_deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            (BTreeMap::new(), BTreeSet::new())
        } else {
            tauri::async_runtime::spawn_blocking(move || {
                let zdr = providers::openrouter_zdr_endpoints_http_probe_with_timeout(
                    &proof_key, remaining,
                )?;
                let mut endpoints = BTreeMap::new();
                for model_id in proof_ids {
                    let remaining = status_deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        break;
                    }
                    let rows = if let Some(canonical_model_id) =
                        paid_candidate_canonicals_for_probe.get(&model_id)
                    {
                        let allowed_model_ids = [
                            model_id.as_str(),
                            canonical_model_id.as_str(),
                        ];
                        providers::openrouter_endpoints_http_probe_with_aliases(
                            &proof_key,
                            &model_id,
                            &allowed_model_ids,
                            remaining,
                        )
                    } else {
                        providers::openrouter_endpoints_http_probe_with_timeout(
                            &proof_key, &model_id, remaining,
                        )
                    };
                    if let Ok(rows) = rows {
                        endpoints.insert(model_id, rows);
                    }
                }
                Ok::<_, String>((endpoints, zdr))
            })
            .await
            .unwrap_or_else(|_| Err("OpenRouter endpoint proof could not be refreshed.".into()))
            .unwrap_or_else(|_| (BTreeMap::new(), BTreeSet::new()))
        }
    };
    let paid_endpoint_metadata_by_model = paid_candidate_canonicals
        .into_keys()
        .filter_map(|model_id| {
            let model = observation.models.iter().find(|model| model.id == model_id)?;
            let endpoints = endpoints_by_model.get(&model_id)?.clone();
            Some((
                model_id,
                navis_fabrica_runner_core::provider_observations::OpenRouterPaidEndpointMetadata {
                    model: model.clone(),
                    endpoints,
                    zdr_endpoints: zdr_endpoints.clone(),
                    runtime_version: runtime_version.clone(),
                    runtime_identity_fingerprint: runtime_identity_fingerprint.clone(),
                    observed_at_ms: observed_at as u128,
                },
            ))
        })
        .collect::<BTreeMap<_, _>>();
    Ok(EconomyStatusResponse {
        models: economy::statuses_from_catalog(
            &provider_id,
            &runtime_identity_fingerprint,
            &observation.models,
            &endpoints_by_model,
            &zdr_endpoints,
            observed_at,
            &paid_endpoint_metadata_by_model,
            "aider",
            &runtime_version,
            observed_at,
        ),
        free_model_daily_requests: observation.free_model_daily_requests.map(|quota| {
            FreeModelDailyRequests {
                used: quota.used,
                limit: quota.limit,
                remaining: quota.remaining,
                observed_at_ms: observed_at,
            }
        }),
        catalog_observed_at_ms: observed_at,
    })
}
