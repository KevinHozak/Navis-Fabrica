//! Capped OpenAI-compatible gateway for an explicitly authorized paid Aider
//! run. The child receives only a random loopback token; the OpenRouter key
//! stays in this process and all upstream traffic uses one fixed HTTPS URL.

use crate::budgets::{BudgetError, BudgetStore, FreeCallRequest, PaidCallRequest};
use crate::providers::{self, OPENROUTER_API_BASE};
use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::io::Read;
use std::net::{SocketAddr, TcpListener};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::watch;
use tokio::task::JoinSet;

const MAX_REQUEST_BYTES: usize = 1024 * 1024;
const MAX_RESPONSE_BYTES: usize = 2 * 1024 * 1024;
const MAX_MESSAGES: usize = 2_000;
const MAX_TIMEOUT: Duration = Duration::from_secs(300);
const PRICING_FRESHNESS_MS: u64 = 24 * 60 * 60 * 1000;
const TOKENS_PER_MILLION: u128 = 1_000_000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PaidPricingCeiling {
    /// Maximum accepted OpenRouter input price, in micro-USD per million tokens.
    pub input_price_micros_per_1m: u64,
    /// Maximum accepted OpenRouter output price, in micro-USD per million tokens.
    pub output_price_micros_per_1m: u64,
    /// The request-level fee ceiling. Paid Aider calls currently require zero.
    pub request_fee_micros: u64,
    pub source: String,
    pub observed_at_ms: u64,
}

/// Authenticated model-catalog prices. Request-level fees are intentionally
/// absent because the model catalog does not establish an endpoint fee.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PaidModelCatalogPricing {
    pub input_price_micros_per_1m: u64,
    pub output_price_micros_per_1m: u64,
    pub source: String,
    pub observed_at_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PaidModelPolicy {
    #[serde(default, skip_serializing_if = "GatewayMode::is_paid")]
    pub mode: GatewayMode,
    pub provider_id: String,
    pub model_id: String,
    pub runtime_id: String,
    /// Verified model input context. Each call reserves this entire input
    /// envelope plus the configured output maximum.
    pub model_context_window_tokens: u64,
    pub max_output_tokens_per_call: u64,
    pub max_calls: u32,
    /// Parent run's maximum spend; child calls must collectively remain under it.
    pub max_cost_micros: u64,
    pub pricing: PaidPricingCeiling,
    /// Optional authenticated model-catalog quote kept separate from a pinned
    /// endpoint quote. Absent preserves legacy paid JSON and comparisons.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_catalog_pricing: Option<PaidModelCatalogPricing>,
    /// Optional backend-reviewed endpoint pin for bounded paid pilots. `None`
    /// preserves the existing OpenRouter paid routing behavior and JSON shape.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint_pin: Option<PaidEndpointPin>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PaidEndpointPin {
    /// Exact OpenRouter endpoint tag, for example `parasail/bf16`.
    pub endpoint_id: String,
    /// Canonical slug learned from authenticated metadata during admission.
    pub canonical_model_id: String,
    /// Bind tool capability requirements from the planned task.
    pub requires_tools: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "kind", deny_unknown_fields)]
pub enum GatewayMode {
    Paid,
    Economy {
        endpoint_id: String,
    },
    SyntheticTrial {
        endpoint_id: String,
        allow_training: bool,
    },
}

impl Default for GatewayMode {
    fn default() -> Self {
        Self::Paid
    }
}

impl GatewayMode {
    fn is_paid(mode: &Self) -> bool {
        matches!(mode, Self::Paid)
    }
}

pub struct PaidGatewayConfig {
    pub upstream_api_key: String,
    pub parent_reservation_id: String,
    pub policy: PaidModelPolicy,
    /// Ephemeral alias learned from authenticated provider metadata during
    /// this synthetic trial admission; never deserialized or persisted as policy.
    pub authenticated_canonical_slug: Option<String>,
    /// Opaque backend admission for ordinary Economy calls. Synthetic trials
    /// intentionally use only their separate consent and fixture proof.
    pub economy_admission: Option<crate::economy::VerifiedEconomyAdmission>,
    pub synthetic_trial_consent: bool,
    pub synthetic_trial_fixture: Option<crate::economy::VerifiedTrialFixture>,
    pub request_timeout: Duration,
    /// Monotonic deadline for the parent runtime, shared with admission checks.
    pub dispatch_deadline: Instant,
    /// The parent run's cancellation flag; checked at gateway admission points.
    pub cancel: Arc<AtomicBool>,
}

#[derive(Debug)]
pub enum PaidGatewayError {
    Invalid(&'static str),
    Budget(String),
    Server(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaidPolicyRefusal {
    CallLimit,
    TokenLimit,
    CostLimit,
    ParentScopeStopped,
    OtherLimit,
}

impl PaidPolicyRefusal {
    pub fn message(self) -> &'static str {
        match self {
            Self::CallLimit => {
                "The paid run reached its configured call limit; reconcile before continuing."
            }
            Self::TokenLimit => {
                "The paid run reached its configured token ceiling; reconcile before continuing."
            }
            Self::CostLimit => {
                "The paid run reached its configured cost ceiling; reconcile before continuing."
            }
            Self::ParentScopeStopped => {
                "The paid run budget scope is stopped for review; reconcile before continuing."
            }
            Self::OtherLimit => "The paid run reached a policy limit; reconcile before continuing.",
        }
    }
}

impl std::fmt::Display for PaidGatewayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(message) => write!(f, "paid gateway configuration is invalid: {message}"),
            Self::Budget(message) => write!(f, "paid gateway budget check failed: {message}"),
            Self::Server(message) => write!(f, "paid gateway could not start: {message}"),
        }
    }
}
impl std::error::Error for PaidGatewayError {}

#[derive(Default)]
struct Gate {
    parent_dispatched: bool,
    stopped: bool,
    billing_unknown: bool,
    policy_refusal: Option<PaidPolicyRefusal>,
}

struct Shared {
    config: PaidGatewayConfig,
    budgets: BudgetStore,
    token: String,
    upstream_url: String,
    gate: Mutex<Gate>,
    policy_stop: Arc<AtomicBool>,
    client: reqwest::blocking::Client,
    #[cfg(test)]
    endpoint_validation_fixture: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
}

pub struct PaidGateway {
    address: SocketAddr,
    token: String,
    shared: Arc<Shared>,
    shutdown: watch::Sender<bool>,
    worker: Option<JoinHandle<()>>,
}

impl PaidGateway {
    /// Bind the local server with a closed dispatch gate. The caller must first
    /// durably record ProcessStarted and mark the parent reservation dispatched.
    pub(crate) fn bind(
        config: PaidGatewayConfig,
        budgets: BudgetStore,
    ) -> Result<Self, PaidGatewayError> {
        Self::bind_to_upstream(
            config,
            budgets,
            format!("{OPENROUTER_API_BASE}/chat/completions"),
        )
    }

    fn bind_to_upstream(
        config: PaidGatewayConfig,
        budgets: BudgetStore,
        upstream_url: String,
    ) -> Result<Self, PaidGatewayError> {
        Self::bind_to_upstream_inner(
            config,
            budgets,
            upstream_url,
            #[cfg(test)]
            None,
        )
    }

    #[cfg(test)]
    fn bind_to_upstream_with_endpoint_fixture(
        config: PaidGatewayConfig,
        budgets: BudgetStore,
        upstream_url: String,
    ) -> Result<Self, PaidGatewayError> {
        Self::bind_to_upstream_inner(config, budgets, upstream_url, Some(Arc::new(|| true)))
    }

    fn bind_to_upstream_inner(
        config: PaidGatewayConfig,
        budgets: BudgetStore,
        upstream_url: String,
        #[cfg(test)] endpoint_validation_fixture: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
    ) -> Result<Self, PaidGatewayError> {
        validate_config(&config)?;
        crate::initialize_crypto_provider();
        let token = random_hex(32)?;
        let client = reqwest::blocking::Client::builder()
            .timeout(config.request_timeout)
            .connect_timeout(config.request_timeout.min(Duration::from_secs(20)))
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .retry(reqwest::retry::never())
            .build()
            .map_err(|_| PaidGatewayError::Server("could not create the upstream client".into()))?;
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .map_err(|_| PaidGatewayError::Server("could not bind a loopback port".into()))?;
        listener.set_nonblocking(true).map_err(|_| {
            PaidGatewayError::Server("could not configure the loopback listener".into())
        })?;
        let address = listener
            .local_addr()
            .map_err(|_| PaidGatewayError::Server("could not read the loopback address".into()))?;
        let shared = Arc::new(Shared {
            config,
            budgets,
            token: token.clone(),
            upstream_url,
            gate: Mutex::new(Gate::default()),
            policy_stop: Arc::new(AtomicBool::new(false)),
            client,
            #[cfg(test)]
            endpoint_validation_fixture,
        });
        let (shutdown, receiver) = watch::channel(false);
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let server_shared = shared.clone();
        let worker = thread::Builder::new()
            .name("navis-paid-gateway".into())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(_) => {
                        let _ = ready_tx.send(Err("could not create gateway runtime"));
                        return;
                    }
                };
                let _ = ready_tx.send(Ok(()));
                runtime.block_on(serve(listener, server_shared, receiver));
                // A cancelled HTTP task cannot cancel reqwest's blocking
                // worker. Bound stop latency; its ledger row remains
                // dispatched until the worker eventually records usage, so
                // callers retain the parent ceiling while it is unresolved.
                runtime.shutdown_timeout(Duration::from_millis(100));
            })
            .map_err(|_| {
                PaidGatewayError::Server("could not start the loopback server thread".into())
            })?;
        match ready_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(())) => Ok(Self {
                address,
                token,
                shared,
                shutdown,
                worker: Some(worker),
            }),
            Ok(Err(message)) => {
                let _ = worker.join();
                Err(PaidGatewayError::Server(message.into()))
            }
            Err(_) => {
                let _ = shutdown.send(true);
                let _ = worker.join();
                Err(PaidGatewayError::Server("gateway startup timed out".into()))
            }
        }
    }

    /// Local `/v1` base URL for the child environment.
    pub fn base_url(&self) -> String {
        format!("http://{}/v1", self.address)
    }

    /// Ephemeral capability token for the child. It is unrelated to the upstream key.
    pub fn child_token(&self) -> &str {
        &self.token
    }

    pub fn policy_refusal(&self) -> Option<PaidPolicyRefusal> {
        self.shared
            .gate
            .lock()
            .ok()
            .and_then(|gate| gate.policy_refusal)
    }

    /// Separate stop signal for a known paid-policy refusal. Runtime callers
    /// can terminate a silent child retry loop without conflating this with
    /// human cancellation.
    pub fn policy_stop_signal(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.shared.policy_stop)
    }

    /// Open the gate only after the ProcessStarted callback has durably stored
    /// the owned process identity and the parent reservation is dispatched.
    pub fn authorize_parent_dispatch(&self) -> Result<(), PaidGatewayError> {
        let parent = self
            .shared
            .budgets
            .get(&self.shared.config.parent_reservation_id)
            .map_err(|error| PaidGatewayError::Budget(error.to_string()))?;
        validate_parent_matches(&parent, &self.shared.config)?;
        ensure_admission_open(&self.shared.config)?;
        let mut gate = self
            .shared
            .gate
            .lock()
            .expect("gateway gate mutex poisoned");
        if gate.stopped || gate.billing_unknown {
            return Err(PaidGatewayError::Invalid("gateway has already stopped"));
        }
        ensure_admission_open(&self.shared.config)?;
        gate.parent_dispatched = true;
        Ok(())
    }

    /// Return measured aggregate tokens and cost only when every attempted
    /// upstream call has a settled usage record. A missing/unknown result stays
    /// `None` so callers preserve the parent reservation's full ceiling.
    pub fn usage_totals(&self) -> Option<(u64, u64)> {
        if matches!(
            &self.shared.config.policy.mode,
            GatewayMode::Economy { .. } | GatewayMode::SyntheticTrial { .. }
        ) {
            let calls = self
                .shared
                .budgets
                .free_calls_for_parent(&self.shared.config.parent_reservation_id)
                .ok()?;
            return calls
                .into_iter()
                .try_fold((0_u64, 0_u64), |(tokens, _), call| {
                    if call.state != crate::budgets::PaidCallState::Settled {
                        return None;
                    }
                    Some((tokens.checked_add(call.actual_tokens?)?, 0))
                });
        }
        let calls = self
            .shared
            .budgets
            .paid_calls_for_parent(&self.shared.config.parent_reservation_id)
            .ok()?;
        calls
            .into_iter()
            .try_fold((0_u64, 0_u64), |(tokens, cost), call| {
                if call.state != crate::budgets::PaidCallState::Settled {
                    return None;
                }
                Some((
                    tokens.checked_add(call.actual_tokens?)?,
                    cost.checked_add(call.actual_cost_micros?)?,
                ))
            })
    }

    /// Close the gateway before stopping the child process. This returns after
    /// a short bounded runtime shutdown even if an in-flight blocking request
    /// continues until its configured timeout. Its durable ledger row remains
    /// unresolved, so parent settlement must retain the full ceiling.
    pub fn stop(&mut self) {
        if let Ok(mut gate) = self.shared.gate.lock() {
            gate.stopped = true;
            gate.parent_dispatched = false;
        }
        let _ = self.shutdown.send(true);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Drop for PaidGateway {
    fn drop(&mut self) {
        self.stop();
    }
}

fn validate_config(config: &PaidGatewayConfig) -> Result<(), PaidGatewayError> {
    let p = &config.policy;
    for value in [
        config.parent_reservation_id.as_str(),
        p.provider_id.as_str(),
        p.model_id.as_str(),
        p.runtime_id.as_str(),
        p.pricing.source.as_str(),
    ] {
        if value.trim().is_empty() || value.len() > 128 || value.chars().any(char::is_control) {
            return Err(PaidGatewayError::Invalid(
                "identifiers or pricing source are malformed",
            ));
        }
    }
    if config.upstream_api_key.trim().is_empty()
        || config.upstream_api_key.len() > 4096
        || config.upstream_api_key.chars().any(char::is_control)
    {
        return Err(PaidGatewayError::Invalid(
            "upstream credential is unavailable",
        ));
    }
    if p.model_catalog_pricing.as_ref().is_some_and(|quote| {
        p.endpoint_pin.is_none()
            || quote.input_price_micros_per_1m == 0
            || quote.output_price_micros_per_1m == 0
            || quote.source.trim().is_empty()
            || quote.source.len() > 128
            || quote.source.chars().any(char::is_control)
            || quote.observed_at_ms > now_ms()
            || now_ms().saturating_sub(quote.observed_at_ms) > PRICING_FRESHNESS_MS
    }) {
        return Err(PaidGatewayError::Invalid(
            "separate model-catalog pricing is malformed, stale, or not endpoint-pinned",
        ));
    }
    if crate::economy::is_router_model_id(&p.model_id)
        || (matches!(&p.mode, GatewayMode::Paid) && crate::economy::is_free_variant_id(&p.model_id))
    {
        return Err(PaidGatewayError::Invalid(
            "router identities and free model variants cannot use the paid gateway",
        ));
    }
    match (&p.mode, p.endpoint_pin.as_ref()) {
        (GatewayMode::Paid, Some(pin)) => {
            validate_endpoint_id(&pin.endpoint_id)?;
            if pin.canonical_model_id.trim().is_empty()
                || pin.canonical_model_id.len() > 160
                || pin.canonical_model_id.chars().any(char::is_control)
                || crate::economy::is_router_model_id(&pin.canonical_model_id)
                || !crate::economy::reviewed_paid_endpoint_policy_matches(
                    &p.model_id,
                    &pin.canonical_model_id,
                    &pin.endpoint_id,
                    now_ms(),
                )
            {
                return Err(PaidGatewayError::Invalid(
                    "paid endpoint pin has no fresh backend-reviewed privacy evidence",
                ));
            }
        }
        (GatewayMode::Paid, None) => {}
        (GatewayMode::Economy { .. } | GatewayMode::SyntheticTrial { .. }, Some(_)) => {
            return Err(PaidGatewayError::Invalid(
                "paid endpoint pin cannot authorize a free or synthetic route",
            ));
        }
        (GatewayMode::Economy { .. } | GatewayMode::SyntheticTrial { .. }, None) => {}
    }
    match &p.mode {
        GatewayMode::Economy { .. } => {
            let Some(admission) = config.economy_admission.as_ref() else {
                return Err(PaidGatewayError::Invalid(
                    "backend Economy admission is missing",
                ));
            };
            admission
                .validate_pinned_policy(&p.provider_id, p, now_ms())
                .map_err(|_| {
                    PaidGatewayError::Invalid("backend Economy admission is stale or mismatched")
                })?;
            if config.request_timeout > Duration::from_secs(admission.max_seconds())
                || config
                    .dispatch_deadline
                    .saturating_duration_since(Instant::now())
                    > Duration::from_secs(admission.max_seconds())
            {
                return Err(PaidGatewayError::Invalid(
                    "Economy dispatch exceeds the backend time ceiling",
                ));
            }
            if config.authenticated_canonical_slug.as_deref() != Some(admission.canonical_slug()) {
                return Err(PaidGatewayError::Invalid(
                    "authenticated canonical model identity changed",
                ));
            }
        }
        GatewayMode::Paid | GatewayMode::SyntheticTrial { .. }
            if config.economy_admission.is_some() =>
        {
            return Err(PaidGatewayError::Invalid(
                "Economy admission cannot authorize this gateway mode",
            ));
        }
        _ => {}
    }
    match &p.mode {
        GatewayMode::SyntheticTrial { allow_training, .. } => {
            let Some(fixture) = config.synthetic_trial_fixture.as_ref() else {
                return Err(PaidGatewayError::Invalid(
                    "synthetic trial fixture proof is missing",
                ));
            };
            if !config.synthetic_trial_consent
                || !allow_training
                || !fixture.matches_trial_pair(&p.model_id, &fixture.task.id)
            {
                return Err(PaidGatewayError::Invalid(
                    "synthetic trial consent or fixture binding is invalid",
                ));
            }
        }
        GatewayMode::Paid | GatewayMode::Economy { .. }
            if config.synthetic_trial_consent || config.synthetic_trial_fixture.is_some() =>
        {
            return Err(PaidGatewayError::Invalid(
                "synthetic trial context cannot authorize this gateway mode",
            ));
        }
        _ => {}
    }
    if matches!(&p.mode, GatewayMode::SyntheticTrial { .. })
        && config
            .authenticated_canonical_slug
            .as_deref()
            .is_none_or(|slug| {
                slug.is_empty()
                    || slug.len() > 160
                    || slug.chars().any(char::is_control)
                    || slug == "openrouter/free"
                    || slug == "openrouter/auto"
            })
    {
        return Err(PaidGatewayError::Invalid(
            "synthetic trial has no authenticated canonical model identity",
        ));
    }
    let (is_free, max_calls) = match &p.mode {
        GatewayMode::Paid => (false, 1_000),
        GatewayMode::Economy { endpoint_id } => {
            validate_endpoint_id(endpoint_id)?;
            (true, 1_000)
        }
        GatewayMode::SyntheticTrial { endpoint_id, .. } => {
            validate_endpoint_id(endpoint_id)?;
            (true, 3)
        }
    };
    if p.model_context_window_tokens == 0
        || p.max_output_tokens_per_call == 0
        || p.max_calls == 0
        || p.max_calls > max_calls
        || (matches!(p.mode, GatewayMode::SyntheticTrial { .. })
            && p.max_output_tokens_per_call > 8_192)
        || (is_free
            && (p.max_cost_micros != 0
                || p.pricing.input_price_micros_per_1m != 0
                || p.pricing.output_price_micros_per_1m != 0))
        || (!is_free
            && (p.max_cost_micros == 0
                || p.pricing.input_price_micros_per_1m == 0
                || p.pricing.output_price_micros_per_1m == 0))
        || p.pricing.request_fee_micros != 0
    {
        return Err(PaidGatewayError::Invalid(
            "positive token/call/price ceilings and a zero request fee are required",
        ));
    }
    let now = now_ms();
    if p.pricing.observed_at_ms > now
        || now.saturating_sub(p.pricing.observed_at_ms) > PRICING_FRESHNESS_MS
    {
        return Err(PaidGatewayError::Invalid("pricing evidence is stale"));
    }
    if config.request_timeout.is_zero() || config.request_timeout > MAX_TIMEOUT {
        return Err(PaidGatewayError::Invalid(
            "upstream timeout must be between 1 and 300 seconds",
        ));
    }
    if matches!(p.mode, GatewayMode::SyntheticTrial { .. })
        && (config.request_timeout > Duration::from_secs(120)
            || config
                .dispatch_deadline
                .saturating_duration_since(Instant::now())
                > Duration::from_secs(120))
    {
        return Err(PaidGatewayError::Invalid(
            "synthetic trial exceeds its 120-second task limit",
        ));
    }
    let call_tokens = p
        .model_context_window_tokens
        .checked_add(p.max_output_tokens_per_call)
        .ok_or(PaidGatewayError::Invalid("token ceiling overflowed"))?;
    if call_tokens > i64::MAX as u64 {
        return Err(PaidGatewayError::Invalid(
            "token ceiling exceeds supported range",
        ));
    }
    if p.max_output_tokens_per_call > p.model_context_window_tokens {
        return Err(PaidGatewayError::Invalid(
            "output ceiling exceeds the verified context window",
        ));
    }
    let call_cost = worst_case_cost_micros(p, call_tokens)?;
    if call_cost > p.max_cost_micros {
        return Err(PaidGatewayError::Invalid(
            "one worst-case call exceeds the parent task cost ceiling",
        ));
    }
    Ok(())
}

fn validate_endpoint_id(endpoint_id: &str) -> Result<(), PaidGatewayError> {
    if endpoint_id.is_empty()
        || endpoint_id.len() > 128
        || endpoint_id.chars().any(char::is_control)
        || !endpoint_id.split('/').all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
        })
    {
        return Err(PaidGatewayError::Invalid(
            "provider endpoint identity is invalid",
        ));
    }
    Ok(())
}

fn validate_parent_matches(
    parent: &crate::budgets::Reservation,
    config: &PaidGatewayConfig,
) -> Result<(), PaidGatewayError> {
    let p = &config.policy;
    if parent.state != crate::budgets::ReservationState::Dispatched {
        return Err(PaidGatewayError::Invalid(
            "parent reservation is not durably dispatched",
        ));
    }
    if parent.provider_id != p.provider_id
        || parent.model_id != p.model_id
        || parent.runtime_id != p.runtime_id
        || parent.max_cost_micros != p.max_cost_micros
        || parent.pricing_source.as_deref() != Some(p.pricing.source.as_str())
        || parent.pricing_verified_at_ms != Some(p.pricing.observed_at_ms)
        || parent.max_tokens
            < p.model_context_window_tokens
                .saturating_add(p.max_output_tokens_per_call)
    {
        return Err(PaidGatewayError::Invalid(
            "parent reservation does not match the immutable paid model policy",
        ));
    }
    Ok(())
}

fn worst_case_cost_micros(
    policy: &PaidModelPolicy,
    total_tokens: u64,
) -> Result<u64, PaidGatewayError> {
    let input_cost = ceil_div(
        policy.model_context_window_tokens as u128
            * policy.pricing.input_price_micros_per_1m as u128,
        TOKENS_PER_MILLION,
    );
    let output_cost = ceil_div(
        policy.max_output_tokens_per_call as u128
            * policy.pricing.output_price_micros_per_1m as u128,
        TOKENS_PER_MILLION,
    );
    let _ = total_tokens;
    u64::try_from(input_cost + output_cost)
        .map_err(|_| PaidGatewayError::Invalid("computed call cost exceeds supported range"))
}

fn ceil_div(value: u128, divisor: u128) -> u128 {
    value / divisor + u128::from(value % divisor != 0)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_millis().min(u64::MAX as u128) as u64)
        .unwrap_or(0)
}

fn random_hex(bytes: usize) -> Result<String, PaidGatewayError> {
    let mut value = vec![0_u8; bytes];
    getrandom::fill(&mut value).map_err(|_| {
        PaidGatewayError::Server("operating system random source unavailable".into())
    })?;
    Ok(value.iter().map(|byte| format!("{byte:02x}")).collect())
}

async fn serve(listener: TcpListener, shared: Arc<Shared>, mut shutdown: watch::Receiver<bool>) {
    let listener = match tokio::net::TcpListener::from_std(listener) {
        Ok(listener) => listener,
        Err(_) => return,
    };
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    connections.abort_all();
                    break;
                }
            }
            accepted = listener.accept() => {
                let Ok((stream, peer)) = accepted else { continue; };
                if !peer.ip().is_loopback() { continue; }
                let shared = shared.clone();
                connections.spawn(async move {
                    let service = service_fn(move |request| handle_http(request, shared.clone()));
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), service)
                        .with_upgrades()
                        .await;
                });
            }
            completed = connections.join_next(), if !connections.is_empty() => {
                let _ = completed;
            }
        }
    }
}

async fn handle_http(
    request: Request<Incoming>,
    shared: Arc<Shared>,
) -> Result<Response<Full<Bytes>>, hyper::Error> {
    if request.uri().path() != "/v1/chat/completions" {
        return Ok(error_response(
            StatusCode::NOT_FOUND,
            "unsupported gateway route",
        ));
    }
    if request.method() != hyper::Method::POST {
        return Ok(error_response(
            StatusCode::METHOD_NOT_ALLOWED,
            "POST required",
        ));
    }
    let Some(auth) = request
        .headers()
        .get(hyper::header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
    else {
        return Ok(error_response(
            StatusCode::UNAUTHORIZED,
            "gateway token required",
        ));
    };
    let Some(child_token) = auth.strip_prefix("Bearer ") else {
        return Ok(error_response(
            StatusCode::UNAUTHORIZED,
            "gateway token required",
        ));
    };
    if !constant_time_eq(child_token.as_bytes(), shared.token.as_bytes()) {
        return Ok(error_response(
            StatusCode::UNAUTHORIZED,
            "gateway token rejected",
        ));
    }
    if !gate_is_open(&shared) {
        return Ok(error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "paid dispatch gate is closed",
        ));
    }
    if request
        .headers()
        .get(hyper::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<usize>().ok())
        .is_some_and(|length| length > MAX_REQUEST_BYTES)
    {
        return Ok(error_response(
            StatusCode::PAYLOAD_TOO_LARGE,
            "request exceeds gateway limit",
        ));
    }
    let body = match Limited::new(request.into_body(), MAX_REQUEST_BYTES)
        .collect()
        .await
    {
        Ok(body) => body.to_bytes(),
        Err(_) => {
            return Ok(error_response(
                StatusCode::PAYLOAD_TOO_LARGE,
                "request exceeds gateway limit",
            ));
        }
    };
    let shared_for_work = shared.clone();
    let result = tokio::task::spawn_blocking(move || forward_request(shared_for_work, body)).await;
    match result {
        Ok(Ok((status, bytes))) => Ok(response(status, bytes, "application/json")),
        Ok(Err((status, message))) => Ok(error_response(status, message)),
        Err(_) => Ok(error_response(
            StatusCode::BAD_GATEWAY,
            "gateway request failed",
        )),
    }
}

fn gate_is_open(shared: &Shared) -> bool {
    admission_open(&shared.config)
        && shared
            .gate
            .lock()
            .map(|gate| gate.parent_dispatched && !gate.stopped && !gate.billing_unknown)
            .unwrap_or(false)
}

fn admission_open(config: &PaidGatewayConfig) -> bool {
    !config.cancel.load(Ordering::Acquire) && Instant::now() < config.dispatch_deadline
}

fn ensure_admission_open(config: &PaidGatewayConfig) -> Result<(), PaidGatewayError> {
    if admission_open(config) {
        Ok(())
    } else {
        Err(PaidGatewayError::Invalid(
            "paid dispatch is cancelled or past its deadline",
        ))
    }
}

fn forward_request(
    shared: Arc<Shared>,
    body: Bytes,
) -> Result<(StatusCode, Bytes), (StatusCode, &'static str)> {
    if !admission_open(&shared.config) {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "paid dispatch is cancelled or past its deadline",
        ));
    }
    if !gate_is_open(&shared) {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "paid dispatch gate is closed",
        ));
    }
    let mut request: Value = serde_json::from_slice(&body)
        .map_err(|_| (StatusCode::BAD_REQUEST, "request must be valid JSON"))?;
    let input_bytes =
        validate_and_cap_request(&mut request, &shared.config.policy).map_err(|_| {
            (
                StatusCode::BAD_REQUEST,
                "request is outside the paid gateway policy",
            )
        })?;
    let policy = &shared.config.policy;
    if !refresh_free_endpoint(&shared) {
        close_for_unknown_billing(&shared);
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "free endpoint identity, price, status, context, or privacy eligibility changed",
        ));
    }
    let call_tokens = policy
        .model_context_window_tokens
        .checked_add(policy.max_output_tokens_per_call)
        .ok_or((StatusCode::PAYLOAD_TOO_LARGE, "token ceiling is invalid"))?;
    if u64::try_from(input_bytes).unwrap_or(u64::MAX) > policy.model_context_window_tokens {
        return Err((
            StatusCode::PAYLOAD_TOO_LARGE,
            "request exceeds the verified model context",
        ));
    }
    let call_cost = worst_case_cost_micros(policy, call_tokens).map_err(|_| {
        (
            StatusCode::PAYLOAD_TOO_LARGE,
            "call cost ceiling is invalid",
        )
    })?;
    let call_nonce = random_hex(16).map_err(|_| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            "secure call identifier unavailable",
        )
    })?;
    let call_prefix = if matches!(&policy.mode, GatewayMode::Paid) {
        "paid"
    } else {
        "free"
    };
    let call_id = format!("{call_prefix}-{}-{call_nonce}", now_ms());
    let reserved = match &policy.mode {
        GatewayMode::Paid => shared
            .budgets
            .reserve_paid_call(&PaidCallRequest {
                parent_reservation_id: shared.config.parent_reservation_id.clone(),
                call_id: call_id.clone(),
                provider_id: policy.provider_id.clone(),
                model_id: policy.model_id.clone(),
                runtime_id: policy.runtime_id.clone(),
                max_tokens: call_tokens,
                max_cost_micros: call_cost,
                max_calls: policy.max_calls,
                pricing_source: policy.pricing.source.clone(),
                pricing_verified_at_ms: policy.pricing.observed_at_ms,
                now_ms: now_ms(),
            })
            .map(|_| ()),
        GatewayMode::Economy { .. } | GatewayMode::SyntheticTrial { .. } => shared
            .budgets
            .reserve_free_call(&FreeCallRequest {
                parent_reservation_id: shared.config.parent_reservation_id.clone(),
                call_id: call_id.clone(),
                provider_id: policy.provider_id.clone(),
                model_id: policy.model_id.clone(),
                runtime_id: policy.runtime_id.clone(),
                max_tokens: call_tokens,
                max_calls: policy.max_calls,
                pricing_source: policy.pricing.source.clone(),
                pricing_verified_at_ms: policy.pricing.observed_at_ms,
                now_ms: now_ms(),
            })
            .map(|_| ()),
    };
    if let Err(error) = reserved {
        if let BudgetError::Exhausted(reason) = error {
            let refusal = classify_policy_refusal(&reason);
            if let Ok(mut gate) = shared.gate.lock() {
                gate.policy_refusal.get_or_insert(refusal);
                gate.stopped = true;
                gate.parent_dispatched = false;
            }
            shared.policy_stop.store(true, Ordering::Release);
        }
        return Err((StatusCode::PAYMENT_REQUIRED, "paid run budget is exhausted"));
    }

    // The durable ledger row exists before this only network boundary. The
    // client has automatic retries and redirects explicitly disabled.
    if !admission_open(&shared.config) || !gate_is_open(&shared) {
        settle_unknown_call(&shared, &call_id, "gateway-closed-before-upstream");
        close_for_unknown_billing(&shared);
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "paid dispatch stopped before provider response",
        ));
    }
    let upstream_remaining = shared
        .config
        .dispatch_deadline
        .saturating_duration_since(Instant::now());
    if upstream_remaining.is_zero() {
        settle_unknown_call(&shared, &call_id, "gateway-deadline-before-upstream");
        close_for_unknown_billing(&shared);
        return Err((
            StatusCode::GATEWAY_TIMEOUT,
            "run deadline expired before provider dispatch",
        ));
    }
    let upstream_result = shared
        .client
        .post(&shared.upstream_url)
        .timeout(upstream_remaining)
        .bearer_auth(&shared.config.upstream_api_key)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .json(&request)
        .send();
    let upstream = match upstream_result {
        Ok(response) => response,
        Err(_) => {
            settle_unknown_call(&shared, &call_id, "openrouter-response-unknown");
            close_for_unknown_billing(&shared);
            return Err((
                StatusCode::BAD_GATEWAY,
                "provider response is unknown; paid calls are closed",
            ));
        }
    };
    let status =
        StatusCode::from_u16(upstream.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let mut response_bytes = Vec::new();
    if upstream
        .take(MAX_RESPONSE_BYTES as u64 + 1)
        .read_to_end(&mut response_bytes)
        .is_err()
        || response_bytes.len() > MAX_RESPONSE_BYTES
    {
        settle_unknown_call(&shared, &call_id, "openrouter-response-truncated");
        close_for_unknown_billing(&shared);
        return Err((
            StatusCode::BAD_GATEWAY,
            "provider response is unknown; paid calls are closed",
        ));
    }
    if !status.is_success() {
        settle_unknown_call(&shared, &call_id, "openrouter-error-response");
        close_for_unknown_billing(&shared);
        return Err((
            StatusCode::BAD_GATEWAY,
            "provider rejected or failed the request; paid calls are closed",
        ));
    }
    let response_value: Value = match serde_json::from_slice(&response_bytes) {
        Ok(value) => value,
        Err(_) => {
            settle_unknown_call(&shared, &call_id, "openrouter-invalid-response");
            close_for_unknown_billing(&shared);
            return Err((
                StatusCode::BAD_GATEWAY,
                "provider usage could not be verified; paid calls are closed",
            ));
        }
    };
    let observed_model = response_value.get("model").and_then(Value::as_str);
    let identity_matches = observed_model == Some(policy.model_id.as_str())
        || policy
            .endpoint_pin
            .as_ref()
            .is_some_and(|pin| observed_model == Some(pin.canonical_model_id.as_str()))
        || (matches!(&policy.mode, GatewayMode::SyntheticTrial { .. })
            && observed_model == shared.config.authenticated_canonical_slug.as_deref());
    if !identity_matches {
        let mismatch_source = observed_model.map_or_else(
            || "openrouter-model-mismatch-missing".to_owned(),
            |value| {
                if safe_observed_model(value) {
                    format!("openrouter-model-mismatch|model={value}")
                } else {
                    format!(
                        "openrouter-model-mismatch-hash-{}",
                        sha256_prefix(value.as_bytes())
                    )
                }
            },
        );
        settle_unknown_call(&shared, &call_id, &mismatch_source);
        close_for_unknown_billing(&shared);
        return Err((
            StatusCode::BAD_GATEWAY,
            "provider model could not be verified; paid calls are closed",
        ));
    }
    let usage = match &shared.config.policy.mode {
        GatewayMode::Paid => parse_usage(&response_value),
        GatewayMode::Economy { .. } | GatewayMode::SyntheticTrial { .. } => {
            parse_free_usage(&response_value)
        }
    };
    let Some((tokens, cost)) = usage else {
        settle_unknown_call(&shared, &call_id, "openrouter-usage-unknown");
        close_for_unknown_billing(&shared);
        return Err((
            StatusCode::BAD_GATEWAY,
            "provider usage could not be verified; paid calls are closed",
        ));
    };
    let settled = match &shared.config.policy.mode {
        GatewayMode::Paid => shared
            .budgets
            .settle_paid_call(
                &call_id,
                Some(tokens),
                Some(cost),
                "openrouter-response-usage",
            )
            .map(|_| ()),
        GatewayMode::Economy { .. } | GatewayMode::SyntheticTrial { .. } => shared
            .budgets
            .settle_free_call(
                &call_id,
                Some(tokens),
                &format!(
                    "openrouter-response-usage|model={}",
                    observed_model.unwrap_or_default()
                ),
            )
            .map(|_| ()),
    };
    if settled.is_err() {
        close_for_unknown_billing(&shared);
        return Err((
            StatusCode::PAYMENT_REQUIRED,
            "provider usage exceeded the call ceiling; paid calls are closed",
        ));
    }
    Ok((StatusCode::OK, Bytes::from(response_bytes)))
}

fn safe_observed_model(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "/:._-".contains(character))
}

fn classify_policy_refusal(reason: &str) -> PaidPolicyRefusal {
    if reason.contains("call count ceiling") {
        PaidPolicyRefusal::CallLimit
    } else if reason.contains("token ceiling") {
        PaidPolicyRefusal::TokenLimit
    } else if reason.contains("cost ceiling") {
        PaidPolicyRefusal::CostLimit
    } else if reason.contains("scope is stopped") {
        PaidPolicyRefusal::ParentScopeStopped
    } else {
        PaidPolicyRefusal::OtherLimit
    }
}

fn refresh_free_endpoint(shared: &Shared) -> bool {
    let policy = &shared.config.policy;
    let (endpoint_id, require_zdr, expected_canonical) = match &policy.mode {
        GatewayMode::Paid => {
            let Some(pin) = policy.endpoint_pin.as_ref() else {
                return true;
            };
            if !crate::economy::reviewed_paid_endpoint_policy_matches(
                &policy.model_id,
                &pin.canonical_model_id,
                &pin.endpoint_id,
                now_ms(),
            ) {
                return false;
            }
            #[cfg(test)]
            if let Some(validate) = shared.endpoint_validation_fixture.as_ref() {
                return validate();
            }
            let remaining = shared
                .config
                .dispatch_deadline
                .saturating_duration_since(Instant::now());
            let timeout = remaining.min(Duration::from_secs(15));
            return !timeout.is_zero()
                && providers::openrouter_pinned_paid_endpoint_probe_with_timeout(
                    &shared.config.upstream_api_key,
                    providers::PinnedPaidEndpointRequirements {
                        model_id: &policy.model_id,
                        canonical_model_id: &pin.canonical_model_id,
                        endpoint_id: &pin.endpoint_id,
                        input_price_ceiling_micros_per_1m: policy.pricing.input_price_micros_per_1m,
                        output_price_ceiling_micros_per_1m: policy
                            .pricing
                            .output_price_micros_per_1m,
                        input_tokens: policy.model_context_window_tokens,
                        output_tokens: policy.max_output_tokens_per_call,
                        requires_tools: pin.requires_tools,
                        model_catalog_input_micros_per_1m: policy
                            .model_catalog_pricing
                            .as_ref()
                            .map(|quote| quote.input_price_micros_per_1m),
                        model_catalog_output_micros_per_1m: policy
                            .model_catalog_pricing
                            .as_ref()
                            .map(|quote| quote.output_price_micros_per_1m),
                    },
                    timeout,
                )
                .is_ok();
        }
        GatewayMode::Economy { endpoint_id } => {
            let Some(admission) = shared.config.economy_admission.as_ref() else {
                return false;
            };
            if admission
                .validate_pinned_policy(&policy.provider_id, policy, now_ms())
                .is_err()
                || admission.endpoint_id() != endpoint_id
                || admission.model_id() != policy.model_id
            {
                return false;
            }
            (endpoint_id, true, Some(admission.canonical_slug()))
        }
        GatewayMode::SyntheticTrial {
            endpoint_id,
            allow_training,
        } => {
            if !allow_training {
                return false;
            }
            (
                endpoint_id,
                false,
                shared.config.authenticated_canonical_slug.as_deref(),
            )
        }
    };
    let now = Instant::now();
    let remaining = shared
        .config
        .dispatch_deadline
        .saturating_duration_since(now);
    if remaining.is_zero() {
        return false;
    }
    let timeout = remaining.min(Duration::from_secs(10));
    let metadata = providers::openrouter_metadata_http_probe_with_timeout(
        &shared.config.upstream_api_key,
        timeout,
    );
    let Ok(metadata) = metadata else {
        return false;
    };
    if metadata.authenticated_for_execution != Some(true) {
        return false;
    }
    let Some(model) = metadata
        .models
        .iter()
        .find(|model| model.id == policy.model_id)
    else {
        return false;
    };
    let current_input = model
        .prompt_price
        .as_deref()
        .and_then(providers::openrouter_price_micros_per_million);
    let current_output = model
        .completion_price
        .as_deref()
        .and_then(providers::openrouter_price_micros_per_million);
    if current_input != Some(0)
        || current_output != Some(0)
        || model.context_length.is_none_or(|context| {
            let required = if matches!(&policy.mode, GatewayMode::Economy { .. }) {
                policy
                    .model_context_window_tokens
                    .saturating_add(policy.max_output_tokens_per_call)
            } else {
                policy.model_context_window_tokens
            };
            context < required
        })
        || expected_canonical
            .is_some_and(|expected| model.canonical_slug.as_deref() != Some(expected))
    {
        return false;
    }
    let endpoint_remaining = shared
        .config
        .dispatch_deadline
        .saturating_duration_since(Instant::now());
    if endpoint_remaining.is_zero() {
        return false;
    }
    let timeout = endpoint_remaining
        .min(shared.config.request_timeout)
        .min(Duration::from_secs(10));
    if matches!(&policy.mode, GatewayMode::Economy { .. }) {
        let Some(admission) = shared.config.economy_admission.as_ref() else {
            return false;
        };
        let Ok(endpoints) = providers::openrouter_endpoints_http_probe_with_timeout(
            &shared.config.upstream_api_key,
            &policy.model_id,
            timeout,
        ) else {
            return false;
        };
        let remaining_zdr = shared
            .config
            .dispatch_deadline
            .saturating_duration_since(Instant::now())
            .min(shared.config.request_timeout)
            .min(Duration::from_secs(10));
        if remaining_zdr.is_zero() {
            return false;
        }
        let Ok(zdr) = providers::openrouter_zdr_endpoints_http_probe_with_timeout(
            &shared.config.upstream_api_key,
            remaining_zdr,
        ) else {
            return false;
        };
        let required_context = policy
            .model_context_window_tokens
            .saturating_add(policy.max_output_tokens_per_call);
        let matches = endpoints
            .iter()
            .filter(|endpoint| {
                endpoint.model_id == policy.model_id
                    && endpoint.tag == *endpoint_id
                    && endpoint.status == Some(0)
                    && endpoint.zdr != Some(false)
                    && endpoint
                        .context_length
                        .is_some_and(|context| context >= required_context)
                    && endpoint
                        .max_prompt_tokens
                        .is_some_and(|tokens| tokens >= policy.model_context_window_tokens)
                    && endpoint
                        .max_completion_tokens
                        .is_some_and(|tokens| tokens >= policy.max_output_tokens_per_call)
                    && endpoint
                        .prompt_price
                        .as_deref()
                        .and_then(providers::openrouter_price_micros_per_million)
                        == Some(0)
                    && endpoint
                        .completion_price
                        .as_deref()
                        .and_then(providers::openrouter_price_micros_per_million)
                        == Some(0)
                    && endpoint
                        .request_price
                        .as_deref()
                        .and_then(providers::openrouter_price_micros_per_million)
                        == Some(0)
                    && endpoint
                        .supported_parameters
                        .as_ref()
                        .is_some_and(|values| values.iter().any(|value| value == "max_tokens"))
                    && (!admission.requires_tools()
                        || endpoint
                            .supported_parameters
                            .as_ref()
                            .is_some_and(|values| {
                                values
                                    .iter()
                                    .any(|value| matches!(value.as_str(), "tools" | "tool_choice"))
                            }))
                    && zdr.contains(&(policy.model_id.clone(), endpoint.tag.clone()))
                    && endpoint.tag.split('/').next() == Some(admission.provider_slug())
            })
            .count();
        matches == 1
    } else {
        providers::validate_openrouter_free_endpoint(
            &shared.config.upstream_api_key,
            &policy.model_id,
            endpoint_id,
            policy.model_context_window_tokens,
            require_zdr,
            timeout,
        )
        .is_ok_and(|eligible| eligible)
    }
}

fn sha256_prefix(bytes: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, bytes)
        .as_ref()
        .iter()
        .take(8)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn validate_and_cap_request(request: &mut Value, policy: &PaidModelPolicy) -> Result<usize, ()> {
    let object = request.as_object_mut().ok_or(())?;
    if object.keys().any(|key| {
        !matches!(
            key.as_str(),
            "model"
                | "messages"
                | "max_tokens"
                | "max_completion_tokens"
                | "stream"
                | "temperature"
                | "top_p"
                | "presence_penalty"
                | "frequency_penalty"
        )
    }) {
        return Err(());
    }
    if object.get("model").and_then(Value::as_str) != Some(policy.model_id.as_str()) {
        return Err(());
    }
    let messages = object.get("messages").and_then(Value::as_array).ok_or(())?;
    if messages.is_empty() || messages.len() > MAX_MESSAGES {
        return Err(());
    }
    let mut prompt_bytes = 0usize;
    for message in messages {
        let message = message.as_object().ok_or(())?;
        if message
            .keys()
            .any(|key| !matches!(key.as_str(), "role" | "content"))
        {
            return Err(());
        }
        let role = message.get("role").and_then(Value::as_str).ok_or(())?;
        if !matches!(role, "system" | "developer" | "user" | "assistant") {
            return Err(());
        }
        let content = message.get("content").and_then(Value::as_str).ok_or(())?;
        prompt_bytes = prompt_bytes.checked_add(content.len()).ok_or(())?;
    }
    // Count conservative per-message framing overhead so UTF-8 bytes provide
    // a strict upper bound for text tokens under a tokenizer-independent guard.
    let bounded_input = prompt_bytes
        .checked_add(messages.len().saturating_mul(16))
        .ok_or(())?;
    if bounded_input as u64 > policy.model_context_window_tokens {
        return Err(());
    }
    for (key, low, high) in [
        ("temperature", 0.0, 2.0),
        ("top_p", 0.0, 1.0),
        ("presence_penalty", -2.0, 2.0),
        ("frequency_penalty", -2.0, 2.0),
    ] {
        if let Some(value) = object.get(key) {
            let number = value.as_f64().filter(|value| value.is_finite()).ok_or(())?;
            if number < low || number > high {
                return Err(());
            }
        }
    }
    object.insert(
        "max_tokens".into(),
        json!(policy.max_output_tokens_per_call),
    );
    object.remove("max_completion_tokens");
    object.insert("stream".into(), Value::Bool(false));
    let mut provider = json!({
        "only": match &policy.mode {
            GatewayMode::Paid => policy
                .endpoint_pin
                .as_ref()
                .map(|pin| vec![pin.endpoint_id.clone()])
                .unwrap_or_default(),
            GatewayMode::Economy { endpoint_id } | GatewayMode::SyntheticTrial { endpoint_id, .. } => vec![endpoint_id.clone()],
        },
        "allow_fallbacks": false,
        "require_parameters": true,
        "max_price": {
            "prompt": dollars_per_million(policy.pricing.input_price_micros_per_1m)?,
            "completion": dollars_per_million(policy.pricing.output_price_micros_per_1m)?,
            "request": 0,
            "image": 0
        }
    });
    if let Some(provider) = provider.as_object_mut() {
        if matches!(&policy.mode, GatewayMode::Paid) && policy.endpoint_pin.is_none() {
            provider.remove("only");
        }
    }
    object.insert("provider".into(), provider);
    match &policy.mode {
        GatewayMode::Paid if policy.endpoint_pin.is_some() => {
            object.insert("data_collection".into(), json!("deny"));
            object.insert("zdr".into(), Value::Bool(true));
        }
        GatewayMode::Paid => {}
        GatewayMode::Economy { .. } => {
            object.insert("data_collection".into(), json!("deny"));
            object.insert("zdr".into(), Value::Bool(true));
        }
        GatewayMode::SyntheticTrial { allow_training, .. } => {
            object.insert(
                "data_collection".into(),
                json!(if *allow_training { "allow" } else { "deny" }),
            );
        }
    }
    Ok(bounded_input)
}

fn dollars_per_million(micros_per_million: u64) -> Result<Value, ()> {
    let whole = micros_per_million / 1_000_000;
    let fractional = micros_per_million % 1_000_000;
    serde_json::from_str(&format!("{whole}.{fractional:06}")).map_err(|_| ())
}

fn parse_usage(response: &Value) -> Option<(u64, u64)> {
    let usage = response.get("usage")?;
    let input = usage.get("prompt_tokens")?.as_u64()?;
    let output = usage.get("completion_tokens")?.as_u64()?;
    let total = input.checked_add(output)?;
    let cost = usage.get("cost")?;
    let decimal = match cost {
        Value::Number(number) => number.to_string(),
        Value::String(value) => value.clone(),
        _ => return None,
    };
    Some((total, decimal_to_micros_ceil(&decimal)?))
}

fn parse_tokens(response: &Value) -> Option<u64> {
    let usage = response.get("usage")?;
    let input = usage.get("prompt_tokens")?.as_u64()?;
    let output = usage.get("completion_tokens")?.as_u64()?;
    input.checked_add(output)
}

fn parse_free_usage(response: &Value) -> Option<(u64, u64)> {
    let tokens = parse_tokens(response)?;
    let usage = response.get("usage")?;
    if let Some(cost) = usage.get("cost") {
        let decimal = match cost {
            Value::Number(number) => number.to_string(),
            Value::String(value) => value.clone(),
            _ => return None,
        };
        if decimal_to_micros_ceil(&decimal)? != 0 {
            return None;
        }
    }
    Some((tokens, 0))
}

/// Convert a nonnegative decimal dollar amount to integer micros, rounding up.
/// This parses JSON decimal/scientific notation without floating point so a
/// response can never understate billed cost through binary rounding.
fn decimal_to_micros_ceil(value: &str) -> Option<u64> {
    if value.is_empty() || value.starts_with('-') || value.starts_with('+') {
        return None;
    }
    let (mantissa, exponent) = match value.find(['e', 'E']) {
        Some(index) => {
            let exponent = value[index + 1..].parse::<i32>().ok()?;
            (&value[..index], exponent)
        }
        None => (value, 0),
    };
    let mut pieces = mantissa.split('.');
    let whole = pieces.next()?;
    let fraction = pieces.next().unwrap_or("");
    if pieces.next().is_some()
        || (whole.is_empty() && fraction.is_empty())
        || !whole.bytes().all(|byte| byte.is_ascii_digit())
        || !fraction.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    let digits = format!("{whole}{fraction}");
    let significand = digits.parse::<u128>().ok()?;
    let scale = exponent
        .checked_sub(i32::try_from(fraction.len()).ok()?)?
        .checked_add(6)?;
    let micros = if scale >= 0 {
        significand.checked_mul(10_u128.checked_pow(u32::try_from(scale).ok()?)?)?
    } else {
        let divisor = 10_u128.checked_pow(scale.unsigned_abs())?;
        significand
            .checked_div(divisor)?
            .checked_add(u128::from(significand % divisor != 0))?
    };
    u64::try_from(micros).ok()
}

fn close_for_unknown_billing(shared: &Shared) {
    if let Ok(mut gate) = shared.gate.lock() {
        gate.billing_unknown = true;
        gate.parent_dispatched = false;
    }
}

fn settle_unknown_call(shared: &Shared, call_id: &str, source: &str) {
    match &shared.config.policy.mode {
        GatewayMode::Paid => {
            let _ = shared.budgets.settle_paid_call(call_id, None, None, source);
        }
        GatewayMode::Economy { .. } | GatewayMode::SyntheticTrial { .. } => {
            let _ = shared.budgets.settle_free_call(call_id, None, source);
        }
    }
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0_u8, |difference, (a, b)| difference | (a ^ b))
        == 0
}

fn error_response(status: StatusCode, message: &'static str) -> Response<Full<Bytes>> {
    response(
        status,
        Bytes::from(
            serde_json::to_vec(&json!({
                "error": { "message": message, "type": "navis_paid_gateway" }
            }))
            .expect("static error serializes"),
        ),
        "application/json",
    )
}

fn response(status: StatusCode, body: Bytes, content_type: &'static str) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .header(hyper::header::CONTENT_TYPE, content_type)
        .header(hyper::header::CACHE_CONTROL, "no-store")
        .body(Full::new(body))
        .expect("valid response")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::budgets::{BudgetPolicy, PaidCallState, ReservationRequest};
    use crate::providers::aider_paid_gateway_arguments;
    use std::convert::Infallible;
    use std::sync::mpsc::Receiver;

    fn policy(now: u64) -> PaidModelPolicy {
        PaidModelPolicy {
            mode: GatewayMode::Paid,
            provider_id: "openrouter-profile".into(),
            model_id: "vendor/model-a".into(),
            runtime_id: "aider".into(),
            model_context_window_tokens: 1_000,
            max_output_tokens_per_call: 100,
            max_calls: 3,
            max_cost_micros: 100_000,
            pricing: PaidPricingCeiling {
                input_price_micros_per_1m: 50_000_000,
                output_price_micros_per_1m: 200_000_000,
                request_fee_micros: 0,
                source: "test-catalog".into(),
                observed_at_ms: now,
            },
            endpoint_pin: None,
            model_catalog_pricing: None,
        }
    }

    #[test]
    fn legacy_paid_policy_serialization_omits_optional_endpoint_pin() {
        let value = serde_json::to_value(policy(now_ms())).unwrap();
        assert!(value.get("endpointPin").is_none());
        assert!(value.get("modelCatalogPricing").is_none());
        let restored: PaidModelPolicy = serde_json::from_value(value).unwrap();
        assert_eq!(restored.endpoint_pin, None);
    }

    #[test]
    fn pinned_paid_request_builder_enforces_fixed_endpoint_and_privacy_fields() {
        let mut pinned = policy(now_ms());
        pinned.model_id = "qwen/qwen3-coder-next".into();
        pinned.model_context_window_tokens = 16_384;
        pinned.max_output_tokens_per_call = 8_192;
        pinned.max_cost_micros = 100_000;
        pinned.pricing.input_price_micros_per_1m = 120_000;
        pinned.pricing.output_price_micros_per_1m = 800_000;
        pinned.endpoint_pin = Some(PaidEndpointPin {
            endpoint_id: "parasail/bf16".into(),
            canonical_model_id: "qwen/qwen3-coder-next-2025-02-03".into(),
            requires_tools: true,
        });
        let mut request = json!({
            "model":"qwen/qwen3-coder-next",
            "messages":[{"role":"user","content":"synthetic"}],
            "max_tokens": 50_000,
            "stream": true
        });
        validate_and_cap_request(&mut request, &pinned).unwrap();
        assert_eq!(request["max_tokens"], 8_192);
        assert_eq!(request["stream"], false);
        assert_eq!(request["provider"]["only"][0], "parasail/bf16");
        assert_eq!(request["provider"]["allow_fallbacks"], false);
        assert_eq!(request["provider"]["require_parameters"], true);
        assert_eq!(request["provider"]["max_price"]["prompt"], 0.12);
        assert_eq!(request["provider"]["max_price"]["completion"], 0.8);
        assert_eq!(request["provider"]["max_price"]["request"], 0);
        assert_eq!(request["provider"]["max_price"]["image"], 0);
        assert_eq!(request["data_collection"], "deny");
        assert_eq!(request["zdr"], true);
    }

    #[test]
    fn split_catalog_and_endpoint_quotes_keep_endpoint_max_price() {
        let mut pinned = policy(now_ms());
        pinned.model_id = "deepseek/deepseek-v4.1-flash".into();
        pinned.model_context_window_tokens = 262_144;
        pinned.max_output_tokens_per_call = 8_192;
        pinned.max_cost_micros = 98_352;
        pinned.pricing = PaidPricingCeiling {
            input_price_micros_per_1m: 300_000,
            output_price_micros_per_1m: 1_200_000,
            request_fee_micros: 0,
            source: "OpenRouter authenticated model endpoint".into(),
            observed_at_ms: now_ms(),
        };
        pinned.model_catalog_pricing = Some(PaidModelCatalogPricing {
            input_price_micros_per_1m: 2_490,
            output_price_micros_per_1m: 305_000,
            source: "OpenRouter authenticated user model catalog".into(),
            observed_at_ms: now_ms(),
        });
        pinned.endpoint_pin = Some(PaidEndpointPin {
            endpoint_id: "parasail/fp8".into(),
            canonical_model_id: "deepseek/deepseek-v4.1-flash-20260910".into(),
            requires_tools: true,
        });
        let mut request = json!({
            "model":"deepseek/deepseek-v4.1-flash",
            "messages":[{"role":"user","content":"synthetic"}],
            "max_tokens":8192
        });
        validate_and_cap_request(&mut request, &pinned).unwrap();
        assert_eq!(request["provider"]["only"][0], "parasail/fp8");
        assert_eq!(request["provider"]["max_price"]["prompt"], 0.3);
        assert_eq!(request["provider"]["max_price"]["completion"], 1.2);
        assert_eq!(request["provider"]["max_price"]["request"], 0);
        assert_eq!(request["provider"]["allow_fallbacks"], false);
        assert_eq!(request["data_collection"], "deny");
        assert_eq!(request["zdr"], true);
    }

    fn budget_store() -> (tempfile::TempDir, BudgetStore, String, u64) {
        budget_store_with_limits(6_000, 100_000)
    }

    fn budget_store_with_limits(
        max_tokens: u64,
        max_cost_micros: u64,
    ) -> (tempfile::TempDir, BudgetStore, String, u64) {
        let dir = tempfile::tempdir().unwrap();
        let store = BudgetStore::open(dir.path().join("budget.sqlite3")).unwrap();
        let limits = BudgetPolicy {
            max_tokens,
            max_cost_micros,
            max_seconds: 1_000,
            max_concurrency: 1,
        };
        store.configure("batch", "job", &limits, &limits).unwrap();
        let now = now_ms();
        let request = ReservationRequest {
            reservation_id: "parent-paid-run".into(),
            batch_id: "batch".into(),
            job_id: "job".into(),
            task_id: "task".into(),
            provider_id: "openrouter-profile".into(),
            model_id: "vendor/model-a".into(),
            runtime_id: "aider".into(),
            max_tokens,
            max_cost_micros: Some(max_cost_micros),
            max_seconds: 300,
            is_paid: true,
            pricing_source: Some("test-catalog".into()),
            pricing_verified_at_ms: Some(now),
        };
        store.reserve(&request, now).unwrap();
        store.mark_dispatched(&request.reservation_id, now).unwrap();
        (dir, store, request.reservation_id, now)
    }

    #[test]
    fn paid_gateway_rejects_free_and_router_ids_even_with_positive_quote() {
        let (_dir, _budgets, parent_id, now) = budget_store();
        let mut config = PaidGatewayConfig {
            upstream_api_key: "sk-or-test".into(),
            parent_reservation_id: parent_id,
            policy: policy(now),
            authenticated_canonical_slug: None,
            economy_admission: None,
            synthetic_trial_consent: false,
            synthetic_trial_fixture: None,
            request_timeout: Duration::from_secs(5),
            dispatch_deadline: Instant::now() + Duration::from_secs(60),
            cancel: Arc::new(AtomicBool::new(false)),
        };
        for model_id in ["vendor/model:free", "openrouter/free", "openrouter/auto"] {
            config.policy.model_id = model_id.into();
            assert!(matches!(
                validate_config(&config),
                Err(PaidGatewayError::Invalid(_))
            ));
        }
        config.policy.model_id = "vendor/normal-paid-model".into();
        assert!(validate_config(&config).is_ok());
    }

    fn fake_upstream(body: Value) -> (String, Receiver<Value>, JoinHandle<()>) {
        fake_upstream_after(body, Duration::ZERO)
    }

    fn fake_upstream_after(
        body: Value,
        delay: Duration,
    ) -> (String, Receiver<Value>, JoinHandle<()>) {
        fake_upstream_timed(body, delay, Duration::from_secs(5))
    }

    fn fake_upstream_timed(
        body: Value,
        delay: Duration,
        accept_timeout: Duration,
    ) -> (String, Receiver<Value>, JoinHandle<()>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let (capture_tx, capture_rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async move {
                let listener = tokio::net::TcpListener::from_std(listener).unwrap();
                let (stream, _) = tokio::time::timeout(accept_timeout, listener.accept())
                    .await
                    .unwrap()
                    .unwrap();
                let response_body = Bytes::from(serde_json::to_vec(&body).unwrap());
                let service = service_fn(move |request: Request<Incoming>| {
                    let sender = capture_tx.clone();
                    let response_body = response_body.clone();
                    async move {
                        let authorization = request
                            .headers()
                            .get(hyper::header::AUTHORIZATION)
                            .and_then(|value| value.to_str().ok())
                            .unwrap_or_default()
                            .to_owned();
                        let forwarded: Value = request
                            .into_body()
                            .collect()
                            .await
                            .map(|body| serde_json::from_slice(&body.to_bytes()).unwrap())
                            .unwrap_or(Value::Null);
                        sender
                            .send(json!({"authorization": authorization, "request": forwarded}))
                            .unwrap();
                        tokio::time::sleep(delay).await;
                        Ok::<_, Infallible>(
                            Response::builder()
                                .header(hyper::header::CONNECTION, "close")
                                .body(Full::new(response_body))
                                .unwrap(),
                        )
                    }
                });
                hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service)
                    .await
                    .unwrap();
            });
        });
        (
            format!("http://{address}/chat/completions"),
            capture_rx,
            worker,
        )
    }

    fn fake_upstream_observed(
        body: Value,
        accept_timeout: Duration,
    ) -> (String, Receiver<Value>, Arc<AtomicBool>, JoinHandle<()>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let (capture_tx, capture_rx) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        let worker = thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async move {
                let listener = tokio::net::TcpListener::from_std(listener).unwrap();
                let deadline = tokio::time::Instant::now() + accept_timeout;
                let mut connections = tokio::task::JoinSet::new();
                loop {
                    if worker_stop.load(Ordering::Acquire)
                        || tokio::time::Instant::now() >= deadline
                    {
                        break;
                    }
                    tokio::select! {
                        accepted = listener.accept() => {
                            let (stream, _) = accepted.unwrap();
                            let sender = capture_tx.clone();
                            let body = body.clone();
                            connections.spawn(async move {
                                let response_body = Bytes::from(serde_json::to_vec(&body).unwrap());
                                let service = service_fn(move |request: Request<Incoming>| {
                                    let sender = sender.clone();
                                    let response_body = response_body.clone();
                                    async move {
                                        let authorization = request
                                            .headers()
                                            .get(hyper::header::AUTHORIZATION)
                                            .and_then(|value| value.to_str().ok())
                                            .unwrap_or_default()
                                            .to_owned();
                                        let forwarded: Value = request
                                            .into_body()
                                            .collect()
                                            .await
                                            .map(|body| serde_json::from_slice(&body.to_bytes()).unwrap())
                                            .unwrap_or(Value::Null);
                                        sender
                                            .send(json!({"authorization": authorization, "request": forwarded}))
                                            .unwrap();
                                        Ok::<_, Infallible>(
                                            Response::builder()
                                                .header(hyper::header::CONNECTION, "close")
                                                .body(Full::new(response_body))
                                                .unwrap(),
                                        )
                                    }
                                });
                                let _ = hyper::server::conn::http1::Builder::new()
                                    .serve_connection(TokioIo::new(stream), service)
                                    .await;
                            });
                        }
                        _ = tokio::time::sleep(Duration::from_millis(20)) => {}
                    }
                }
                connections.abort_all();
                while connections.join_next().await.is_some() {}
            });
        });
        (
            format!("http://{address}/chat/completions"),
            capture_rx,
            stop,
            worker,
        )
    }

    #[test]
    fn capped_gateway_rejects_until_dispatched_then_forwards_only_capped_text_request() {
        let (_dir, budgets, parent_id, priced_at) = budget_store();
        let response_body = json!({
            "id": "chatcmpl-test",
            "model": "vendor/model-a",
            "object": "chat.completion",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "ok"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 12, "completion_tokens": 4, "total_tokens": 16, "cost": 0.00005}
        });
        let (upstream_url, captured, upstream_worker) = fake_upstream(response_body);
        let mut gateway = PaidGateway::bind_to_upstream(
            PaidGatewayConfig {
                upstream_api_key: "sk-or-never-leaves-process".into(),
                parent_reservation_id: parent_id.clone(),
                policy: policy(priced_at),
                authenticated_canonical_slug: None,
                economy_admission: None,
                synthetic_trial_consent: false,
                synthetic_trial_fixture: None,
                request_timeout: Duration::from_secs(10),
                dispatch_deadline: Instant::now() + Duration::from_secs(60),
                cancel: Arc::new(AtomicBool::new(false)),
            },
            budgets.clone(),
            upstream_url,
        )
        .unwrap();
        let client = reqwest::blocking::Client::new();
        let url = format!("{}/chat/completions", gateway.base_url());
        let outgoing = json!({
            "model": "vendor/model-a",
            "messages": [{"role": "user", "content": "make a small edit"}],
            "max_tokens": 50_000,
            "stream": true
        });
        let response = client
            .post(&url)
            .bearer_auth(gateway.child_token())
            .json(&outgoing)
            .send()
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);

        gateway.authorize_parent_dispatch().unwrap();
        let response = client
            .post(&url)
            .bearer_auth(gateway.child_token())
            .json(&outgoing)
            .send()
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let sent = captured.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(sent["authorization"], "Bearer sk-or-never-leaves-process");
        assert_eq!(sent["request"]["model"], "vendor/model-a");
        assert_eq!(sent["request"]["max_tokens"], 100);
        assert_eq!(sent["request"]["stream"], false);
        assert_eq!(sent["request"]["provider"]["allow_fallbacks"], false);
        assert_eq!(sent["request"]["provider"]["max_price"]["request"], 0);
        assert_eq!(sent["request"]["provider"]["max_price"]["prompt"], 50.0);
        assert!(sent["request"].get("tools").is_none());

        let calls = budgets.paid_calls_for_parent(&parent_id).unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].state, PaidCallState::Settled);
        assert_eq!(calls[0].max_tokens, 1_100);
        assert_eq!(calls[0].actual_tokens, Some(16));
        assert_eq!(calls[0].actual_cost_micros, Some(50));
        gateway.stop();
        upstream_worker.join().unwrap();
    }

    #[test]
    fn pinned_paid_gateway_routes_canonical_response_to_exact_zdr_endpoint_and_settles_paid_usage()
    {
        let dir = tempfile::tempdir().unwrap();
        let budgets = BudgetStore::open(dir.path().join("budget.sqlite3")).unwrap();
        let policy_limits = BudgetPolicy {
            max_tokens: 73_728,
            max_cost_micros: 100_000,
            max_seconds: 120,
            max_concurrency: 1,
        };
        budgets
            .configure("pilot", "qwen", &policy_limits, &policy_limits)
            .unwrap();
        let now = now_ms();
        let parent_id = "pinned-qwen-run".to_owned();
        let source = "authenticated-openrouter-model-and-endpoint-metadata".to_owned();
        let reservation = ReservationRequest {
            reservation_id: parent_id.clone(),
            batch_id: "pilot".into(),
            job_id: "qwen".into(),
            task_id: "small_bugfix".into(),
            provider_id: "openrouter-profile".into(),
            model_id: "qwen/qwen3-coder-next".into(),
            runtime_id: "aider".into(),
            max_tokens: 73_728,
            max_cost_micros: Some(100_000),
            max_seconds: 120,
            is_paid: true,
            pricing_source: Some(source.clone()),
            pricing_verified_at_ms: Some(now),
        };
        budgets.reserve(&reservation, now).unwrap();
        budgets.mark_dispatched(&parent_id, now).unwrap();

        let mut pinned = policy(now);
        pinned.provider_id = "openrouter-profile".into();
        pinned.model_id = "qwen/qwen3-coder-next".into();
        pinned.model_context_window_tokens = 16_384;
        pinned.max_output_tokens_per_call = 8_192;
        pinned.max_calls = 3;
        pinned.max_cost_micros = 100_000;
        pinned.pricing = PaidPricingCeiling {
            input_price_micros_per_1m: 120_000,
            output_price_micros_per_1m: 800_000,
            request_fee_micros: 0,
            source,
            observed_at_ms: now,
        };
        pinned.endpoint_pin = Some(PaidEndpointPin {
            endpoint_id: "parasail/bf16".into(),
            canonical_model_id: "qwen/qwen3-coder-next-2025-02-03".into(),
            requires_tools: true,
        });
        let response_body = json!({
            "id": "chatcmpl-pinned-qwen",
            "model": "qwen/qwen3-coder-next-2025-02-03",
            "choices": [{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}],
            "usage": {"prompt_tokens":12,"completion_tokens":4,"total_tokens":16,"cost":0.00005}
        });
        let (upstream_url, captured, upstream_worker) = fake_upstream(response_body);
        let mut gateway = PaidGateway::bind_to_upstream_with_endpoint_fixture(
            PaidGatewayConfig {
                upstream_api_key: "sk-or-never-leaves-process".into(),
                parent_reservation_id: parent_id.clone(),
                policy: pinned,
                authenticated_canonical_slug: None,
                economy_admission: None,
                synthetic_trial_consent: false,
                synthetic_trial_fixture: None,
                request_timeout: Duration::from_secs(10),
                dispatch_deadline: Instant::now() + Duration::from_secs(60),
                cancel: Arc::new(AtomicBool::new(false)),
            },
            budgets.clone(),
            upstream_url,
        )
        .unwrap();
        gateway.authorize_parent_dispatch().unwrap();
        let response = reqwest::blocking::Client::new()
            .post(format!("{}/chat/completions", gateway.base_url()))
            .bearer_auth(gateway.child_token())
            .json(&json!({
                "model":"qwen/qwen3-coder-next",
                "messages":[{"role":"user","content":"synthetic fixed-endpoint smoke"}],
                "max_tokens": 50_000,
                "stream": true
            }))
            .send()
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let sent = captured.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(sent["request"]["model"], "qwen/qwen3-coder-next");
        assert_eq!(sent["request"]["max_tokens"], 8_192);
        assert_eq!(sent["request"]["provider"]["only"][0], "parasail/bf16");
        assert_eq!(sent["request"]["provider"]["allow_fallbacks"], false);
        assert_eq!(sent["request"]["provider"]["require_parameters"], true);
        assert_eq!(sent["request"]["provider"]["max_price"]["prompt"], 0.12);
        assert_eq!(sent["request"]["provider"]["max_price"]["completion"], 0.8);
        assert_eq!(sent["request"]["provider"]["max_price"]["request"], 0);
        assert_eq!(sent["request"]["provider"]["max_price"]["image"], 0);
        assert_eq!(sent["request"]["data_collection"], "deny");
        assert_eq!(sent["request"]["zdr"], true);
        let calls = budgets.paid_calls_for_parent(&parent_id).unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].state, PaidCallState::Settled);
        assert_eq!(calls[0].actual_tokens, Some(16));
        assert_eq!(calls[0].actual_cost_micros, Some(50));
        gateway.stop();
        upstream_worker.join().unwrap();
    }

    #[test]
    fn expired_or_cancelled_admission_guard_never_opens_paid_dispatch() {
        for cancelled in [false, true] {
            let (_dir, budgets, parent_id, priced_at) = budget_store();
            let cancel = Arc::new(AtomicBool::new(cancelled));
            let deadline = if cancelled {
                Instant::now() + Duration::from_secs(30)
            } else {
                Instant::now() - Duration::from_millis(1)
            };
            let mut gateway = PaidGateway::bind_to_upstream(
                PaidGatewayConfig {
                    upstream_api_key: "sk-or-test".into(),
                    parent_reservation_id: parent_id.clone(),
                    policy: policy(priced_at),
                    authenticated_canonical_slug: None,
                    economy_admission: None,
                    synthetic_trial_consent: false,
                    synthetic_trial_fixture: None,
                    request_timeout: Duration::from_secs(10),
                    dispatch_deadline: deadline,
                    cancel,
                },
                budgets.clone(),
                "http://127.0.0.1:1/chat/completions".into(),
            )
            .unwrap();
            assert!(gateway.authorize_parent_dispatch().is_err());

            let response = reqwest::blocking::Client::new()
                .post(format!("{}/chat/completions", gateway.base_url()))
                .bearer_auth(gateway.child_token())
                .json(&json!({
                    "model": "vendor/model-a",
                    "messages": [{"role": "user", "content": "must not dispatch"}]
                }))
                .send()
                .unwrap();
            assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
            assert!(budgets
                .paid_calls_for_parent(&parent_id)
                .unwrap()
                .is_empty());
            gateway.stop();
        }
    }

    #[test]
    fn cancellation_or_deadline_after_gate_open_blocks_each_upstream_admission() {
        for cancel_after_open in [false, true] {
            let (_dir, budgets, parent_id, priced_at) = budget_store();
            let cancel = Arc::new(AtomicBool::new(false));
            // Keep bind and durable authorization out of the expiry window.
            // The test then waits for this same deadline, so load cannot make
            // the gate expire before we have proved it opened.
            let dispatch_deadline = Instant::now() + Duration::from_secs(5);
            let mut gateway = PaidGateway::bind_to_upstream(
                PaidGatewayConfig {
                    upstream_api_key: "sk-or-test".into(),
                    parent_reservation_id: parent_id.clone(),
                    policy: policy(priced_at),
                    authenticated_canonical_slug: None,
                    economy_admission: None,
                    synthetic_trial_consent: false,
                    synthetic_trial_fixture: None,
                    request_timeout: Duration::from_secs(10),
                    dispatch_deadline,
                    cancel: cancel.clone(),
                },
                budgets.clone(),
                "http://127.0.0.1:1/chat/completions".into(),
            )
            .unwrap();
            gateway.authorize_parent_dispatch().unwrap();
            assert!(
                admission_open(&gateway.shared.config),
                "the gate must be authorized while its deadline is still open"
            );
            if cancel_after_open {
                cancel.store(true, Ordering::Release);
            } else {
                while Instant::now() < dispatch_deadline {
                    thread::sleep(
                        dispatch_deadline
                            .saturating_duration_since(Instant::now())
                            .min(Duration::from_millis(10)),
                    );
                }
                assert!(
                    !admission_open(&gateway.shared.config),
                    "the gate deadline must expire before attempting admission"
                );
            }

            let response = reqwest::blocking::Client::new()
                .post(format!("{}/chat/completions", gateway.base_url()))
                .bearer_auth(gateway.child_token())
                .json(&json!({
                    "model": "vendor/model-a",
                    "messages": [{"role": "user", "content": "must not dispatch"}]
                }))
                .send()
                .unwrap();
            assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
            assert!(budgets
                .paid_calls_for_parent(&parent_id)
                .unwrap()
                .is_empty());
            gateway.stop();
        }
    }

    #[test]
    fn gateway_rejects_unknown_fields_multimodal_and_stale_or_fee_bearing_quotes() {
        let p = policy(now_ms());
        let valid = json!({"model":"vendor/model-a","messages":[{"role":"user","content":"hi"}]});
        let mut copied = valid.clone();
        validate_and_cap_request(&mut copied, &p).unwrap();
        assert_eq!(copied["max_tokens"], 100);
        assert_eq!(copied["stream"], false);
        assert!(copied.get("provider").is_some());

        for invalid in [
            json!({"model":"vendor/model-a","messages":[{"role":"user","content":"hi"}],"tools":[]}),
            json!({"model":"vendor/model-a","messages":[{"role":"user","content":[{"type":"image_url","image_url":{"url":"x"}}]}]}),
            json!({"model":"other/model","messages":[{"role":"user","content":"hi"}]}),
            json!({"model":"vendor/model-a","messages":[{"role":"tool","content":"do work"}]}),
        ] {
            let mut invalid = invalid;
            assert!(validate_and_cap_request(&mut invalid, &p).is_err());
        }

        let (_dir, _budgets, parent_id, _priced_at) = budget_store();
        let mut config = PaidGatewayConfig {
            upstream_api_key: "sk-or-test".into(),
            parent_reservation_id: parent_id,
            policy: p,
            authenticated_canonical_slug: None,
            economy_admission: None,
            synthetic_trial_consent: false,
            synthetic_trial_fixture: None,
            request_timeout: Duration::from_secs(10),
            dispatch_deadline: Instant::now() + Duration::from_secs(60),
            cancel: Arc::new(AtomicBool::new(false)),
        };
        config.policy.pricing.request_fee_micros = 1;
        assert!(matches!(
            validate_config(&config),
            Err(PaidGatewayError::Invalid(_))
        ));
        config.policy.pricing.request_fee_micros = 0;
        config.policy.pricing.observed_at_ms = 0;
        assert!(matches!(
            validate_config(&config),
            Err(PaidGatewayError::Invalid(_))
        ));
    }

    #[test]
    fn billing_cost_decimal_parser_rounds_up_without_float_underflow() {
        assert_eq!(decimal_to_micros_ceil("0.00005"), Some(50));
        assert_eq!(decimal_to_micros_ceil("0.00000100000000000000001"), Some(2));
        assert_eq!(decimal_to_micros_ceil("0.0000001"), Some(1));
        assert_eq!(decimal_to_micros_ceil("1e-7"), Some(1));
        assert_eq!(decimal_to_micros_ceil("1.25e2"), Some(125_000_000));
        assert_eq!(decimal_to_micros_ceil("0"), Some(0));
        assert_eq!(decimal_to_micros_ceil("1e999"), None);
        assert_eq!(decimal_to_micros_ceil("-0.01"), None);
        assert_eq!(decimal_to_micros_ceil("NaN"), None);
        assert_eq!(decimal_to_micros_ceil("1.2.3"), None);
        let response: Value = serde_json::from_str(
            r#"{"usage":{"prompt_tokens":1,"completion_tokens":0,"cost":0.00000100000000000000001}}"#,
        )
        .unwrap();
        assert_eq!(parse_usage(&response), Some((1, 2)));
    }

    #[test]
    fn missing_usage_leaves_call_unresolved_and_closes_gate_without_retry() {
        let (_dir, budgets, parent_id, priced_at) = budget_store();
        let partial_response = json!({
            "id": "chatcmpl-partial",
            "model": "vendor/model-a",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "maybe"}}]
        });
        let (upstream_url, captured, upstream_worker) = fake_upstream(partial_response);
        let mut gateway = PaidGateway::bind_to_upstream(
            PaidGatewayConfig {
                upstream_api_key: "sk-or-test".into(),
                parent_reservation_id: parent_id.clone(),
                policy: policy(priced_at),
                authenticated_canonical_slug: None,
                economy_admission: None,
                synthetic_trial_consent: false,
                synthetic_trial_fixture: None,
                request_timeout: Duration::from_secs(10),
                dispatch_deadline: Instant::now() + Duration::from_secs(60),
                cancel: Arc::new(AtomicBool::new(false)),
            },
            budgets.clone(),
            upstream_url,
        )
        .unwrap();
        gateway.authorize_parent_dispatch().unwrap();
        let client = reqwest::blocking::Client::new();
        let url = format!("{}/chat/completions", gateway.base_url());
        let outgoing = json!({
            "model": "vendor/model-a",
            "messages": [{"role": "user", "content": "make a small edit"}]
        });
        let response = client
            .post(&url)
            .bearer_auth(gateway.child_token())
            .json(&outgoing)
            .send()
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        captured.recv_timeout(Duration::from_secs(2)).unwrap();

        let calls = budgets.paid_calls_for_parent(&parent_id).unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].state, PaidCallState::Unresolved);
        assert_eq!(calls[0].actual_tokens, None);
        assert_eq!(calls[0].actual_cost_micros, None);
        assert_eq!(gateway.usage_totals(), None);

        let response = client
            .post(&url)
            .bearer_auth(gateway.child_token())
            .json(&outgoing)
            .send()
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        gateway.stop();
        upstream_worker.join().unwrap();
    }

    #[test]
    fn stop_returns_promptly_with_inflight_upstream_and_keeps_call_unsettled() {
        let (_dir, budgets, parent_id, priced_at) = budget_store();
        let response_body = json!({
            "id": "chatcmpl-slow",
            "model": "vendor/model-a",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "ok"}}],
            "usage": {"prompt_tokens": 12, "completion_tokens": 4, "cost": 0.00005}
        });
        let (upstream_url, captured, upstream_worker) =
            fake_upstream_after(response_body, Duration::from_secs(2));
        let mut gateway = PaidGateway::bind_to_upstream(
            PaidGatewayConfig {
                upstream_api_key: "sk-or-test".into(),
                parent_reservation_id: parent_id.clone(),
                policy: policy(priced_at),
                authenticated_canonical_slug: None,
                economy_admission: None,
                synthetic_trial_consent: false,
                synthetic_trial_fixture: None,
                request_timeout: Duration::from_secs(10),
                dispatch_deadline: Instant::now() + Duration::from_secs(60),
                cancel: Arc::new(AtomicBool::new(false)),
            },
            budgets.clone(),
            upstream_url,
        )
        .unwrap();
        gateway.authorize_parent_dispatch().unwrap();
        let url = format!("{}/chat/completions", gateway.base_url());
        let token = gateway.child_token().to_owned();
        let client_worker = thread::spawn(move || {
            reqwest::blocking::Client::new()
                .post(url)
                .bearer_auth(token)
                .json(&json!({
                    "model": "vendor/model-a",
                    "messages": [{"role": "user", "content": "wait"}]
                }))
                .send()
        });
        captured.recv_timeout(Duration::from_secs(2)).unwrap();
        let start = std::time::Instant::now();
        gateway.stop();
        assert!(start.elapsed() < Duration::from_secs(1));
        assert_eq!(gateway.usage_totals(), None);
        let calls = budgets.paid_calls_for_parent(&parent_id).unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].state, PaidCallState::Dispatched);
        let _ = client_worker.join();
        upstream_worker.join().unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn paid_call_cap_refusal_cancels_retrying_child_and_preserves_settled_usage() {
        use crate::process::{self, ProcessControl};
        use std::sync::atomic::AtomicBool;
        use std::sync::Mutex;

        let (_dir, budgets, parent_id, priced_at) = budget_store();
        let response_body = json!({
            "id": "chatcmpl-cap-test",
            "model": "vendor/model-a",
            "object": "chat.completion",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "synthetic success"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 12, "completion_tokens": 4, "total_tokens": 16, "cost": 0.00005}
        });
        let (upstream_url, captured, upstream_worker) = fake_upstream(response_body);
        let mut capped_policy = policy(priced_at);
        capped_policy.max_calls = 1;
        let mut gateway = PaidGateway::bind_to_upstream(
            PaidGatewayConfig {
                upstream_api_key: "sk-or-never-leaves-process".into(),
                parent_reservation_id: parent_id.clone(),
                policy: capped_policy,
                authenticated_canonical_slug: None,
                economy_admission: None,
                synthetic_trial_consent: false,
                synthetic_trial_fixture: None,
                request_timeout: Duration::from_secs(10),
                dispatch_deadline: Instant::now() + Duration::from_secs(60),
                cancel: Arc::new(AtomicBool::new(false)),
            },
            budgets.clone(),
            upstream_url,
        )
        .unwrap();
        gateway.authorize_parent_dispatch().unwrap();

        let scratch = tempfile::tempdir().unwrap();
        let script = scratch.path().join("silent-retry-until-cancelled.ps1");
        std::fs::write(
            &script,
            r#"$ErrorActionPreference = 'Stop'
$uri = "$env:OPENAI_API_BASE/chat/completions"
$headers = @{ Authorization = "Bearer $env:OPENAI_API_KEY" }
$body = '{"model":"vendor/model-a","messages":[{"role":"user","content":"synthetic"}]}'
Invoke-RestMethod -Method Post -Uri $uri -Headers $headers -ContentType 'application/json' -Body $body | Out-Null
Write-Output 'FIRST_UPSTREAM_RESPONSE_RECEIVED'
try {
  Invoke-RestMethod -Method Post -Uri $uri -Headers $headers -ContentType 'application/json' -Body $body | Out-Null
  exit 9
} catch {
  # Deliberately silent: policy-stop handling must not depend on another
  # stdout/stderr event from a child's retry loop.
  while ($true) { Start-Sleep -Milliseconds 50 }
}"#,
        )
        .unwrap();
        let user_cancel = Arc::new(AtomicBool::new(false));
        let process_cancel = AtomicBool::new(false);
        let monitor_done = AtomicBool::new(false);
        let policy_stop = gateway.policy_stop_signal();
        let output = Mutex::new(Vec::new());
        let mut child_environment = [
            ("OPENAI_API_BASE".to_owned(), gateway.base_url()),
            (
                "OPENAI_API_KEY".to_owned(),
                gateway.child_token().to_owned(),
            ),
        ]
        .into_iter()
        .map(|(name, value)| (name.into(), value.into()))
        .collect::<Vec<_>>();
        for name in ["PATH", "PATHEXT", "SYSTEMROOT", "WINDIR", "TEMP", "TMP"] {
            if let Some(value) = std::env::var_os(name) {
                child_environment.push((name.into(), value));
            }
        }
        let result = thread::scope(|scope| {
            scope.spawn(|| {
                while !monitor_done.load(Ordering::Acquire) {
                    if user_cancel.load(Ordering::Acquire) || policy_stop.load(Ordering::Acquire) {
                        process_cancel.store(true, Ordering::Release);
                        return;
                    }
                    thread::sleep(Duration::from_millis(10));
                }
            });
            let result = process::run_streamed(
                &script,
                &[],
                scratch.path(),
                &child_environment,
                &["*"],
                Duration::from_secs(15),
                64 * 1024,
                &process_cancel,
                |_| ProcessControl::Continue,
                |chunk| {
                    output.lock().unwrap().extend_from_slice(&chunk.bytes);
                    if gateway.policy_refusal() == Some(PaidPolicyRefusal::CallLimit) {
                        ProcessControl::Cancel
                    } else {
                        ProcessControl::Continue
                    }
                },
            );
            monitor_done.store(true, Ordering::Release);
            result
        })
        .unwrap();

        assert!(
            result.cancelled,
            "known policy refusal must stop silent child retry loop; status={:?}, refusal={:?}, output={:?}, policy_stop={}, user_cancel={}",
            result.status,
            gateway.policy_refusal(),
            String::from_utf8_lossy(&output.lock().unwrap()),
            policy_stop.load(Ordering::Acquire),
            user_cancel.load(Ordering::Acquire),
        );
        assert_eq!(gateway.policy_refusal(), Some(PaidPolicyRefusal::CallLimit));
        assert!(policy_stop.load(Ordering::Acquire));
        assert_eq!(gateway.usage_totals(), Some((16, 50)));
        let calls = budgets.paid_calls_for_parent(&parent_id).unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].state, PaidCallState::Settled);
        assert_eq!(calls[0].actual_tokens, Some(16));
        assert_eq!(calls[0].actual_cost_micros, Some(50));
        let forwarded = captured.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(forwarded["request"]["model"], "vendor/model-a");
        assert!(
            captured.try_recv().is_err(),
            "call beyond cap reached upstream"
        );
        gateway.stop();
        upstream_worker.join().unwrap();
    }

    /// Opt-in compatibility check for the isolated Aider 0.86.2 fixture.
    /// Run with FABRICA_AIDER_SMOKE_EXE pointing at that executable and
    /// `cargo test -- --ignored real_aider_uses_only_the_local_fake_gateway`.
    #[test]
    #[ignore = "requires the isolated Aider 0.86.2 fixture executable"]
    fn real_aider_uses_only_the_local_fake_gateway() {
        use std::process::{Command, Stdio};

        let executable = std::env::var_os("FABRICA_AIDER_SMOKE_EXE")
            .expect("set FABRICA_AIDER_SMOKE_EXE to the isolated Aider 0.86.2 executable");
        let (_dir, budgets, parent_id, priced_at) = budget_store_with_limits(50_000, 100_000);
        let mut smoke_policy = policy(priced_at);
        smoke_policy.model_context_window_tokens = 32_768;
        smoke_policy.pricing.input_price_micros_per_1m = 1_000_000;
        smoke_policy.pricing.output_price_micros_per_1m = 1_000_000;
        let response_body = json!({
            "id": "chatcmpl-aider-smoke",
            "object": "chat.completion",
            "created": 1,
            "model": "vendor/model-a",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "Compatibility smoke complete — observed FABRICA_TASK_FILE_WITNESS_7c91e2. résumé: café 🛠 ►.\n\nweb/theme.css\n```css\n<<<<<<< SEARCH\n/* synthetic admitted source */\n=======\n/* synthetic admitted source ► */\n>>>>>>> REPLACE\n```"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 12, "completion_tokens": 4, "total_tokens": 16, "cost": 0.00005}
        });
        let (upstream_url, captured, upstream_worker) =
            fake_upstream_timed(response_body, Duration::ZERO, Duration::from_secs(45));
        let mut gateway = PaidGateway::bind_to_upstream(
            PaidGatewayConfig {
                upstream_api_key: "sk-or-never-real-smoke-key".into(),
                parent_reservation_id: parent_id.clone(),
                policy: smoke_policy,
                authenticated_canonical_slug: None,
                economy_admission: None,
                synthetic_trial_consent: false,
                synthetic_trial_fixture: None,
                request_timeout: Duration::from_secs(30),
                dispatch_deadline: Instant::now() + Duration::from_secs(60),
                cancel: Arc::new(AtomicBool::new(false)),
            },
            budgets.clone(),
            upstream_url,
        )
        .unwrap();
        gateway.authorize_parent_dispatch().unwrap();

        let scratch = tempfile::tempdir().unwrap();
        let project = scratch.path().join("project");
        let profile = scratch.path().join("isolated-home");
        let appdata = profile.join("AppData").join("Roaming");
        let local_appdata = profile.join("AppData").join("Local");
        for path in [&project, &profile, &appdata, &local_appdata] {
            std::fs::create_dir_all(path).unwrap();
        }
        let git = |args: &[&str]| {
            let output = Command::new("git")
                .arg("-C")
                .arg(&project)
                .args(args)
                .output()
                .expect("run local synthetic git fixture");
            assert!(
                output.status.success(),
                "synthetic git fixture command failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        git(&["init", "--quiet"]);

        let model = "openai/vendor/model-a";
        let metadata = json!({(model): {
            "max_tokens": 32_768,
            "max_input_tokens": 32_768,
            "max_output_tokens": 100,
            "input_cost_per_token": 0.000001,
            "output_cost_per_token": 0.000001,
            "litellm_provider": "openai",
            "mode": "chat"
        }});
        let write = |name: &str, contents: &[u8]| {
            let path = scratch.path().join(name);
            std::fs::write(&path, contents).unwrap();
            path.to_string_lossy().into_owned()
        };
        let prompt = write(
            "prompt.md",
            b"Read the source files. Edit web/theme.css by changing the exact comment `/* synthetic admitted source */` to `/* synthetic admitted source \xe2\x96\xba */`. Include the source witness marker in your reply.\n",
        );
        let metadata = write(
            "model-metadata.json",
            &serde_json::to_vec(&metadata).unwrap(),
        );
        let model_settings = write("empty-model-settings.yml", b"{}\n");
        let config = write("empty-config.yml", b"{}\n");
        let chat_history = write("chat-history.md", b"");
        let input_history = write("input-history", b"");
        let empty_env = write("empty.env", b"");
        let source_marker = "FABRICA_TASK_FILE_WITNESS_7c91e2";
        let source_files = [
            (
                "src-tauri/src/game_view.rs",
                format!("// Task source witness: {source_marker}\n"),
            ),
            ("web/theme.css", "/* synthetic admitted source */\n".into()),
            ("web/app.js", "// synthetic admitted source\n".into()),
            (
                "web/index.html",
                "<!-- synthetic admitted source -->\n".into(),
            ),
        ];
        let mut allowed_source_paths = Vec::with_capacity(source_files.len());
        for (relative, contents) in &source_files {
            let path = project.join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, contents).unwrap();
            allowed_source_paths.push(path.to_string_lossy().into_owned());
        }
        git(&[
            "add",
            "--",
            "src-tauri/src/game_view.rs",
            "web/theme.css",
            "web/app.js",
            "web/index.html",
        ]);
        git(&[
            "-c",
            "user.name=Fabrica Synthetic Fixture",
            "-c",
            "user.email=fabrica-fixture@example.invalid",
            "commit",
            "--quiet",
            "-m",
            "synthetic baseline",
        ]);
        let stdout_path = scratch.path().join("aider.stdout.log");
        let stderr_path = scratch.path().join("aider.stderr.log");
        let mut args = aider_paid_gateway_arguments(
            "vendor/model-a",
            &prompt,
            &metadata,
            &model_settings,
            &config,
            &chat_history,
            &input_history,
            &empty_env,
            None,
            &allowed_source_paths,
        );
        let aider = crate::runtime::resolve_aider_executable(&executable.to_string_lossy())
            .expect("resolve the isolated Aider executable");
        let python = crate::runtime::resolve_aider_python(&aider)
            .expect("resolve the adjacent isolated Aider Python");
        let guard_path = scratch.path().join("paid_aider_guard.py");
        std::fs::write(&guard_path, include_str!("paid_aider_guard.py")).unwrap();
        let allowlist_path = scratch.path().join("allowed-files.json");
        std::fs::write(
            &allowlist_path,
            serde_json::to_vec(&allowed_source_paths).unwrap(),
        )
        .unwrap();
        let (guarded_python, guarded_args) = crate::runtime::guarded_aider_invocation(
            &python,
            &guard_path,
            &project,
            &allowlist_path,
            &args,
        )
        .expect("construct the production guarded Aider invocation");
        let mut command = Command::new(guarded_python);
        command
            .args(guarded_args)
            .current_dir(&project)
            .env_clear()
            .stdin(Stdio::null())
            .env("USERPROFILE", &profile)
            .env("HOME", &profile)
            .env(
                "HOMEDRIVE",
                profile
                    .to_string_lossy()
                    .chars()
                    .take(2)
                    .collect::<String>(),
            )
            .env(
                "HOMEPATH",
                profile
                    .to_string_lossy()
                    .chars()
                    .skip(2)
                    .collect::<String>(),
            )
            .env("APPDATA", &appdata)
            .env("LOCALAPPDATA", &local_appdata)
            .env("XDG_CONFIG_HOME", &profile)
            .env("PYTHONNOUSERSITE", "1")
            .env("PYTHONIOENCODING", "cp1252")
            .env("PYTHONUTF8", "0")
            .env("LITELLM_LOCAL_MODEL_COST_MAP", "True")
            .env("OPENAI_API_BASE", gateway.base_url())
            .env("OPENAI_API_KEY", gateway.child_token())
            .env("NO_PROXY", "*")
            .env("no_proxy", "*")
            .stdout(Stdio::from(std::fs::File::create(&stdout_path).unwrap()))
            .stderr(Stdio::from(std::fs::File::create(&stderr_path).unwrap()));
        for name in ["PATH", "PATHEXT", "SYSTEMROOT", "WINDIR", "TEMP", "TMP"] {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        let mut child = command.spawn().expect("run isolated Aider fixture");
        let deadline = std::time::Instant::now() + Duration::from_secs(45);
        loop {
            if child.try_wait().expect("check Aider process").is_some() {
                break;
            }
            if std::time::Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                let stdout = std::fs::read_to_string(&stdout_path).unwrap_or_default();
                let stderr = std::fs::read_to_string(&stderr_path).unwrap_or_default();
                panic!(
                    "Aider fake-upstream compatibility smoke timed out: {} {}",
                    stdout, stderr
                );
            }
            thread::sleep(Duration::from_millis(100));
        }
        let status = child.wait().expect("wait for Aider");
        let stdout = std::fs::read_to_string(&stdout_path).unwrap_or_default();
        let stderr = std::fs::read_to_string(&stderr_path).unwrap_or_default();
        assert!(
            status.success(),
            "Aider smoke failed: {} {}",
            stdout,
            stderr
        );
        assert!(
            format!("{stdout}\n{stderr}").contains("résumé: café 🛠"),
            "Aider completed without emitting the non-ASCII fake response: {} {}",
            stdout,
            stderr
        );
        assert!(
            format!("{stdout}\n{stderr}").contains('►'),
            "Aider/Rich lost the U+25BA output under a CP1252 parent environment: {} {}",
            stdout,
            stderr
        );
        assert_eq!(
            std::fs::read_to_string(project.join("web/theme.css"))
                .unwrap()
                .replace("\r\n", "\n"),
            "/* synthetic admitted source ► */\n",
            "guarded Aider did not apply the expected Unicode fixture edit"
        );
        let sent = captured
            .recv_timeout(Duration::from_secs(5))
            .unwrap_or_else(|error| {
                panic!(
                    "Aider exited without reaching the local gateway ({error:?}): {} {}",
                    stdout, stderr
                )
            });
        assert_eq!(sent["authorization"], "Bearer sk-or-never-real-smoke-key");
        assert_eq!(sent["request"]["model"], "vendor/model-a");
        assert!(
            sent["request"].to_string().contains(source_marker),
            "Aider reached the fake gateway without the admitted source file contents: {}",
            sent["request"]
        );
        assert_eq!(sent["request"]["provider"]["allow_fallbacks"], false);
        assert_eq!(sent["request"]["provider"]["max_price"]["request"], 0);
        assert!(
            !project.join(".aider.tags.cache.v4").exists(),
            "paid file-scoped Aider created a repo-map cache in the synthetic source repo"
        );
        let status = Command::new("git")
            .arg("-C")
            .arg(&project)
            .args(["status", "--porcelain", "--untracked-files=all"])
            .output()
            .expect("inspect synthetic source repo status");
        assert!(status.status.success());
        assert_eq!(
            String::from_utf8_lossy(&status.stdout).lines().collect::<Vec<_>>(),
            vec![" M web/theme.css"],
            "paid file-scoped Aider must change only the admitted Unicode fixture"
        );
        assert_eq!(budgets.paid_calls_for_parent(&parent_id).unwrap().len(), 1);
        assert_eq!(gateway.usage_totals(), Some((16, 50)));
        gateway.stop();
        upstream_worker.join().unwrap();
    }

    /// Opt-in end-to-end regression for the runner-owned Aider file guard.
    /// Requires the isolated Aider 0.86.2 venv under FABRICA_AIDER_SMOKE_EXE.
    /// The only HTTP endpoint is the local fake gateway; no real provider key
    /// or upstream URL is used.
    #[test]
    #[cfg(windows)]
    #[ignore = "requires the isolated Aider 0.86.2 fixture executable and Windows link support"]
    fn real_aider_guard_blocks_unapproved_files_before_egress() {
        use std::os::windows::fs::{symlink_dir, symlink_file};
        use std::path::Path;
        use std::process::{Command, Stdio};

        let aider_executable = std::env::var_os("FABRICA_AIDER_SMOKE_EXE")
            .expect("set FABRICA_AIDER_SMOKE_EXE to the isolated Aider 0.86.2 executable");
        let aider_executable = Path::new(&aider_executable)
            .canonicalize()
            .expect("resolve isolated Aider executable");
        let python = aider_executable
            .parent()
            .expect("Aider executable has a Scripts directory")
            .join("python.exe")
            .canonicalize()
            .expect("resolve Python adjacent to isolated Aider");

        let (_budget_dir, budgets, parent_id, priced_at) =
            budget_store_with_limits(50_000, 100_000);
        let mut smoke_policy = policy(priced_at);
        smoke_policy.model_context_window_tokens = 32_768;
        smoke_policy.pricing.input_price_micros_per_1m = 1_000_000;
        smoke_policy.pricing.output_price_micros_per_1m = 1_000_000;

        let scratch = tempfile::tempdir()
            .expect("create retained Aider guard fixture")
            .keep();
        println!("AIDER_GUARD_FIXTURE_ROOT={}", scratch.display());
        let project = scratch.join("project");
        let profile = scratch.join("isolated-home");
        let appdata = profile.join("AppData").join("Roaming");
        let local_appdata = profile.join("AppData").join("Local");
        let outside = scratch.join("outside");
        let empty_hooks = scratch.join("empty-git-hooks");
        for path in [
            &project,
            &profile,
            &appdata,
            &local_appdata,
            &outside,
            &empty_hooks,
        ] {
            std::fs::create_dir_all(path).expect("create synthetic fixture directory");
        }

        let git = |args: &[&str]| {
            let output = Command::new("git")
                .arg("-C")
                .arg(&project)
                .arg("-c")
                .arg(format!("core.hooksPath={}", empty_hooks.display()))
                .args(args)
                .output()
                .expect("run local synthetic git fixture");
            assert!(
                output.status.success(),
                "synthetic git fixture command failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        git(&["init", "--quiet"]);

        let witness = "FABRICA_GUARD_ALLOWED_SOURCE_91d3";
        let private_marker = "FABRICA_GUARD_PRIVATE_SENTINEL_70af";
        let outside_marker = "FABRICA_GUARD_OUTSIDE_SENTINEL_5c21";
        let allowed_relative = [
            "src-tauri/src/game_view.rs",
            "web/theme.css",
            "web/app.js",
            "web/index.html",
            "web/nested/allowed-new.txt",
        ];
        for relative in allowed_relative.iter().take(4) {
            let path = project.join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).expect("create admitted parent");
        }
        let game_view = project.join(allowed_relative[0]);
        let original_game_view = format!("// {witness}\nconst STATUS = 'before';\n");
        std::fs::write(&game_view, &original_game_view).expect("write admitted source");
        std::fs::write(project.join(allowed_relative[1]), "/* admitted */\n")
            .expect("write admitted CSS");
        std::fs::write(project.join(allowed_relative[2]), "// admitted\n")
            .expect("write admitted JavaScript");
        std::fs::write(project.join(allowed_relative[3]), "<!-- admitted -->\n")
            .expect("write admitted HTML");

        let private_path = project.join("web/private-notes.txt");
        std::fs::write(&private_path, private_marker).expect("write existing private file");
        std::fs::write(outside.join("secret.txt"), outside_marker).expect("write outside sentinel");
        let linked_dir = project.join("web/linked-dir");
        if symlink_dir(&outside, &linked_dir).is_err() {
            let output = Command::new("cmd.exe")
                .args(["/d", "/c", "mklink", "/J"])
                .arg(linked_dir.to_string_lossy().replace('/', "\\"))
                .arg(outside.to_string_lossy().replace('/', "\\"))
                .output()
                .expect("create a local junction for the reparse-point case");
            assert!(
                output.status.success(),
                "could not create symlink or junction fixture: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let linked_file_name = "web/linked-file.txt";
        let linked_file_path = project.join(linked_file_name);
        let link_file_created =
            symlink_file(&outside.join("secret.txt"), &linked_file_path).is_ok();
        let linked_file = if link_file_created {
            linked_file_path
        } else {
            // The directory junction above provides the same guarded path
            // component case without requiring file-symlink privileges.
            linked_dir.join("secret.txt")
        };
        let linked_file_original =
            std::fs::read(&outside.join("secret.txt")).expect("read linked target baseline");

        git(&[
            "add",
            "--",
            "src-tauri/src/game_view.rs",
            "web/theme.css",
            "web/app.js",
            "web/index.html",
            "web/private-notes.txt",
        ]);
        if link_file_created {
            git(&["add", "--", linked_file_name]);
        }
        git(&[
            "-c",
            "user.name=Fabrica Synthetic Fixture",
            "-c",
            "user.email=fabrica-fixture@example.invalid",
            "-c",
            "commit.gpgSign=false",
            "commit",
            "--quiet",
            "-m",
            "synthetic baseline",
        ]);
        let baseline_status = Command::new("git")
            .arg("-C")
            .arg(&project)
            .arg("-c")
            .arg(format!("core.hooksPath={}", empty_hooks.display()))
            .args(["status", "--porcelain", "--untracked-files=all"])
            .output()
            .expect("read synthetic baseline status");
        assert!(baseline_status.status.success());
        let baseline_status = String::from_utf8_lossy(&baseline_status.stdout)
            .lines()
            .map(str::to_owned)
            .collect::<Vec<_>>();

        let model = "openai/vendor/model-a";
        let model_metadata = json!({(model): {
            "max_tokens": 32_768,
            "max_input_tokens": 32_768,
            "max_output_tokens": 100,
            "input_cost_per_token": 0.000001,
            "output_cost_per_token": 0.000001,
            "litellm_provider": "openai",
            "mode": "chat"
        }});
        let write_support = |name: &str, contents: &[u8]| {
            let path = scratch.join(name);
            std::fs::write(&path, contents).expect("write runner-owned support file");
            path.to_string_lossy().into_owned()
        };
        let prompt = write_support(
            "prompt.md",
            format!(
                "Change only {file}: replace `{before}` with `{after}`. Do not access any other file.\n",
                file = allowed_relative[0],
                before = "const STATUS = 'before';",
                after = "const STATUS = 'after';"
            )
            .as_bytes(),
        );
        let metadata = write_support(
            "model-metadata.json",
            &serde_json::to_vec(&model_metadata).unwrap(),
        );
        let model_settings = write_support("empty-model-settings.yml", b"{}\n");
        let config = write_support("empty-config.yml", b"{}\n");
        let chat_history = write_support("chat-history.md", b"");
        let input_history = write_support("input-history", b"");
        let empty_env = write_support("empty.env", b"");
        let guard = scratch.join("paid_aider_guard.py");
        std::fs::write(&guard, include_str!("paid_aider_guard.py"))
            .expect("materialize the exact embedded Aider guard");
        let allowed_paths = allowed_relative
            .iter()
            .map(|relative| project.join(relative).to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        let allowlist = scratch.join("allowed-files.json");
        std::fs::write(&allowlist, serde_json::to_vec(&allowed_paths).unwrap())
            .expect("write exact runner allowlist");

        let attack_response = format!(
            "{file}\n<<<<<<< SEARCH\n{before}\n=======\n{after}\n>>>>>>> REPLACE\n\n\
             {new_file}\n<<<<<<< SEARCH\n=======\nADMITTED NESTED FILE\n>>>>>>> REPLACE\n\n\
             web/private-notes.txt\n<<<<<<< SEARCH\n=======\nPRIVATE FILE EDIT ATTEMPT\n>>>>>>> REPLACE\n\n\
             web/unapproved-new.txt\n<<<<<<< SEARCH\n=======\nUNAPPROVED FILE EDIT ATTEMPT\n>>>>>>> REPLACE\n\n\
             ../outside/secret.txt\n<<<<<<< SEARCH\n=======\nTRAVERSAL EDIT ATTEMPT\n>>>>>>> REPLACE\n\n\
             web/linked-file.txt\n<<<<<<< SEARCH\n=======\nSYMLINK EDIT ATTEMPT\n>>>>>>> REPLACE\n\n\
             web/linked-dir/secret.txt\n<<<<<<< SEARCH\n=======\nJUNCTION EDIT ATTEMPT\n>>>>>>> REPLACE",
            file = allowed_relative[0],
            before = "const STATUS = 'before';",
            after = "const STATUS = 'after';",
            new_file = allowed_relative[4],
        );
        let response_body = json!({
            "id": "chatcmpl-aider-guard-adversarial",
            "object": "chat.completion",
            "created": 1,
            "model": "vendor/model-a",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": attack_response}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 12, "completion_tokens": 80, "total_tokens": 92, "cost": 0.00005}
        });
        let (upstream_url, captured, upstream_stop, upstream_worker) =
            fake_upstream_observed(response_body, Duration::from_secs(45));
        let mut gateway = PaidGateway::bind_to_upstream(
            PaidGatewayConfig {
                upstream_api_key: "sk-or-never-real-guard-fixture".into(),
                parent_reservation_id: parent_id.clone(),
                policy: smoke_policy,
                authenticated_canonical_slug: None,
                economy_admission: None,
                synthetic_trial_consent: false,
                synthetic_trial_fixture: None,
                request_timeout: Duration::from_secs(30),
                dispatch_deadline: Instant::now() + Duration::from_secs(60),
                cancel: Arc::new(AtomicBool::new(false)),
            },
            budgets.clone(),
            upstream_url,
        )
        .expect("bind the bounded local fake gateway");
        gateway.authorize_parent_dispatch().unwrap();

        let args = aider_paid_gateway_arguments(
            "vendor/model-a",
            &prompt,
            &metadata,
            &model_settings,
            &config,
            &chat_history,
            &input_history,
            &empty_env,
            None,
            &allowed_paths,
        );
        assert!(
            !args.iter().any(|argument| argument == "--yes"),
            "the guarded invocation must not enable Aider's automatic confirmation"
        );
        let (guard_python, guarded_args) =
            crate::runtime::guarded_aider_invocation(&python, &guard, &project, &allowlist, &args)
                .expect("construct the same guarded invocation used by production");

        let stdout_path = scratch.join("aider-guard.stdout.log");
        let stderr_path = scratch.join("aider-guard.stderr.log");
        let mut command = Command::new(guard_python);
        command
            .args(guarded_args)
            .current_dir(&project)
            .env_clear()
            .stdin(Stdio::null())
            .env("USERPROFILE", &profile)
            .env("HOME", &profile)
            .env("APPDATA", &appdata)
            .env("LOCALAPPDATA", &local_appdata)
            .env("XDG_CONFIG_HOME", &profile)
            .env("PYTHONNOUSERSITE", "1")
            .env("PYTHONIOENCODING", "utf-8")
            .env("PYTHONUTF8", "1")
            .env("LITELLM_LOCAL_MODEL_COST_MAP", "True")
            .env("OPENAI_API_BASE", gateway.base_url())
            .env("OPENAI_API_KEY", gateway.child_token())
            .env("NO_PROXY", "*")
            .env("no_proxy", "*")
            .env("TEMP", &scratch)
            .env("TMP", &scratch)
            .stdout(Stdio::from(std::fs::File::create(&stdout_path).unwrap()))
            .stderr(Stdio::from(std::fs::File::create(&stderr_path).unwrap()));
        for name in ["PATH", "PATHEXT", "SYSTEMROOT", "WINDIR"] {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        let mut child = command.spawn().expect("launch Aider through the guard");
        let deadline = Instant::now() + Duration::from_secs(45);
        loop {
            if child
                .try_wait()
                .expect("check guarded Aider process")
                .is_some()
            {
                break;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!(
                    "guarded Aider fixture timed out; retained evidence at {}",
                    scratch.display()
                );
            }
            thread::sleep(Duration::from_millis(100));
        }
        let status = child.wait().expect("wait for guarded Aider");
        let stdout = std::fs::read_to_string(&stdout_path).unwrap_or_default();
        let stderr = std::fs::read_to_string(&stderr_path).unwrap_or_default();
        assert!(
            status.success(),
            "guarded Aider failed; retained evidence at {}: {} {}",
            scratch.display(),
            stdout,
            stderr
        );
        upstream_stop.store(true, Ordering::Release);
        upstream_worker.join().unwrap();

        let first = captured
            .recv_timeout(Duration::from_secs(5))
            .unwrap_or_else(|error| {
                panic!(
                    "Aider did not reach fake gateway ({error:?}); retained evidence at {}",
                    scratch.display()
                )
            });
        let mut sent_requests = vec![first];
        sent_requests.extend(captured.try_iter());
        for sent in &sent_requests {
            assert_eq!(
                sent["authorization"],
                "Bearer sk-or-never-real-guard-fixture"
            );
            let serialized_request = sent["request"].to_string();
            assert!(
                !serialized_request.contains(private_marker),
                "existing unapproved source leaked upstream"
            );
            assert!(
                !serialized_request.contains(outside_marker),
                "outside or linked source leaked upstream"
            );
        }
        let first_user_messages = sent_requests[0]["request"]["messages"].to_string();
        assert!(
            first_user_messages.contains(witness),
            "admitted source was not sent upstream"
        );
        assert_ne!(
            std::fs::read_to_string(&game_view).unwrap(),
            original_game_view,
            "the admitted edit should still be applied"
        );
        assert!(
            std::fs::read_to_string(&game_view)
                .unwrap()
                .contains("const STATUS = 'after';"),
            "the admitted source should contain the requested edit"
        );
        assert_eq!(
            std::fs::read_to_string(project.join(allowed_relative[4]))
                .unwrap()
                .replace("\r\n", "\n"),
            "ADMITTED NESTED FILE\n",
            "an admitted nested new file should be creatable"
        );
        assert_eq!(
            std::fs::read_to_string(&private_path).unwrap(),
            private_marker
        );
        assert!(!project.join("web/unapproved-new.txt").exists());
        assert_eq!(
            std::fs::read(outside.join("secret.txt")).unwrap(),
            linked_file_original
        );
        assert_eq!(std::fs::read(linked_file).unwrap(), linked_file_original);

        let status = Command::new("git")
            .arg("-C")
            .arg(&project)
            .arg("-c")
            .arg(format!("core.hooksPath={}", empty_hooks.display()))
            .args(["status", "--porcelain", "--untracked-files=all"])
            .output()
            .expect("inspect synthetic source changes");
        assert!(status.status.success());
        let status_text = String::from_utf8_lossy(&status.stdout);
        let changed = status_text.lines().map(str::to_owned).collect::<Vec<_>>();
        assert!(
            changed.contains(&" M src-tauri/src/game_view.rs".to_owned()),
            "the admitted source edit was not recorded; retained evidence at {}: {}",
            scratch.display(),
            status_text
        );
        let mut residual = changed.clone();
        residual.retain(|line| {
            line != " M src-tauri/src/game_view.rs" && line != "?? web/nested/allowed-new.txt"
        });
        assert_eq!(
            residual,
            baseline_status,
            "the model added or changed an unapproved path; retained evidence at {}",
            scratch.display()
        );
        assert!(!project.join(".aider.tags.cache.v4").exists());
        let paid_calls = budgets.paid_calls_for_parent(&parent_id).unwrap();
        assert_eq!(paid_calls.len(), sent_requests.len());
        assert!(paid_calls
            .iter()
            .all(|call| call.state == PaidCallState::Settled));
        assert_eq!(
            gateway.usage_totals(),
            Some((
                92 * sent_requests.len() as u64,
                50 * sent_requests.len() as u64
            ))
        );
        gateway.stop();
    }
}
