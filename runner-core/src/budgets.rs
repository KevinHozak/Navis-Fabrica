//! Durable admission reservations for per-job and aggregate batch limits.
//! Reserve the full token/cost/time envelope before provider dispatch. Unknown
//! post-dispatch billing keeps the reservation charged until reconciled.

use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;

const SCHEMA_VERSION: i64 = 3;

/// A single paid provider attempt, bounded by its already-admitted parent run.
/// These records deliberately do not participate in global policy totals: the
/// parent reservation already holds that complete envelope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PaidCallRequest {
    pub parent_reservation_id: String,
    pub call_id: String,
    pub provider_id: String,
    pub model_id: String,
    pub runtime_id: String,
    pub max_tokens: u64,
    pub max_cost_micros: u64,
    pub max_calls: u32,
    pub pricing_source: String,
    pub pricing_verified_at_ms: u64,
    pub now_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PaidCall {
    pub parent_reservation_id: String,
    pub call_id: String,
    pub max_tokens: u64,
    pub max_cost_micros: u64,
    pub state: PaidCallState,
    pub dispatched_at_ms: u64,
    pub actual_tokens: Option<u64>,
    pub actual_cost_micros: Option<u64>,
    pub settlement_source: Option<String>,
}

/// A zero-price OpenRouter request, stored separately from paid call accounting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FreeCallRequest {
    pub parent_reservation_id: String,
    pub call_id: String,
    pub provider_id: String,
    pub model_id: String,
    pub runtime_id: String,
    pub max_tokens: u64,
    pub max_calls: u32,
    pub pricing_source: String,
    pub pricing_verified_at_ms: u64,
    pub now_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FreeCall {
    pub parent_reservation_id: String,
    pub call_id: String,
    pub max_tokens: u64,
    pub state: PaidCallState,
    pub dispatched_at_ms: u64,
    pub actual_tokens: Option<u64>,
    pub settlement_source: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PaidCallState {
    Dispatched,
    Settled,
    Unresolved,
}

impl PaidCallState {
    fn parse(value: &str) -> Result<Self, BudgetError> {
        match value {
            "dispatched" => Ok(Self::Dispatched),
            "settled" => Ok(Self::Settled),
            "unresolved" => Ok(Self::Unresolved),
            _ => Err(BudgetError::Invalid(
                "unknown stored paid call state".into(),
            )),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BudgetPolicy {
    pub max_tokens: u64,
    pub max_cost_micros: u64,
    pub max_seconds: u64,
    pub max_concurrency: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReservationRequest {
    pub reservation_id: String,
    pub batch_id: String,
    pub job_id: String,
    pub task_id: String,
    pub provider_id: String,
    pub model_id: String,
    pub runtime_id: String,
    /// Maximum input plus output tokens exposed to this provider call.
    pub max_tokens: u64,
    /// Conservative ceiling derived from fresh pricing and token limits.
    /// Paid work must supply it; local work may reserve zero.
    pub max_cost_micros: Option<u64>,
    pub max_seconds: u64,
    pub is_paid: bool,
    pub pricing_source: Option<String>,
    pub pricing_verified_at_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Reservation {
    pub reservation_id: String,
    pub batch_id: String,
    pub job_id: String,
    pub task_id: String,
    pub provider_id: String,
    pub model_id: String,
    pub runtime_id: String,
    pub max_tokens: u64,
    pub max_cost_micros: u64,
    pub max_seconds: u64,
    pub state: ReservationState,
    pub created_at_ms: u64,
    pub dispatched_at_ms: Option<u64>,
    pub pricing_source: Option<String>,
    pub pricing_verified_at_ms: Option<u64>,
    pub settlement_source: Option<String>,
    pub reconciliation_ref: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReservationState {
    Reserved,
    Dispatched,
    Settled,
    Released,
    Unresolved,
}

impl ReservationState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Reserved => "reserved",
            Self::Dispatched => "dispatched",
            Self::Settled => "settled",
            Self::Released => "released",
            Self::Unresolved => "unresolved",
        }
    }
    fn parse(value: &str) -> Result<Self, BudgetError> {
        match value {
            "reserved" => Ok(Self::Reserved),
            "dispatched" => Ok(Self::Dispatched),
            "settled" => Ok(Self::Settled),
            "released" => Ok(Self::Released),
            "unresolved" => Ok(Self::Unresolved),
            _ => Err(BudgetError::Invalid(
                "unknown stored reservation state".into(),
            )),
        }
    }
}

#[derive(Debug)]
pub enum BudgetError {
    Sql(rusqlite::Error),
    Invalid(String),
    Exhausted(String),
    Conflict(String),
    UnsupportedSchema(i64),
}
impl std::fmt::Display for BudgetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Sql(error) => write!(f, "budget store error: {error}"),
            Self::Invalid(message) => write!(f, "invalid budget request: {message}"),
            Self::Exhausted(message) => write!(f, "budget exhausted: {message}"),
            Self::Conflict(message) => write!(f, "budget conflict: {message}"),
            Self::UnsupportedSchema(version) => write!(
                f,
                "budget schema {version} is newer than supported schema {SCHEMA_VERSION}"
            ),
        }
    }
}
impl std::error::Error for BudgetError {}
impl From<rusqlite::Error> for BudgetError {
    fn from(value: rusqlite::Error) -> Self {
        Self::Sql(value)
    }
}

#[derive(Clone, Debug)]
pub struct BudgetStore {
    path: PathBuf,
}

impl BudgetStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, BudgetError> {
        let store = Self {
            path: path.as_ref().to_path_buf(),
        };
        let connection = store.connect()?;
        let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version > SCHEMA_VERSION {
            return Err(BudgetError::UnsupportedSchema(version));
        }
        if version == 0 {
            connection.execute_batch("BEGIN IMMEDIATE;
                CREATE TABLE IF NOT EXISTS budget_policies(
                    scope_type TEXT NOT NULL CHECK(scope_type IN ('batch','job')),
                    scope_id TEXT NOT NULL,
                    max_tokens INTEGER NOT NULL,
                    max_cost_micros INTEGER NOT NULL,
                    max_seconds INTEGER NOT NULL,
                    max_concurrency INTEGER NOT NULL,
                    tokens_used INTEGER NOT NULL DEFAULT 0,
                    cost_used INTEGER NOT NULL DEFAULT 0,
                    seconds_used INTEGER NOT NULL DEFAULT 0,
                    PRIMARY KEY(scope_type,scope_id));
                CREATE TABLE IF NOT EXISTS budget_reservations(
                    reservation_id TEXT PRIMARY KEY,
                    batch_id TEXT NOT NULL,
                    job_id TEXT NOT NULL,
                    task_id TEXT NOT NULL,
                    provider_id TEXT NOT NULL,
                    model_id TEXT NOT NULL,
                    runtime_id TEXT NOT NULL,
                    max_tokens INTEGER NOT NULL,
                    max_cost_micros INTEGER NOT NULL,
                    max_seconds INTEGER NOT NULL,
                    state TEXT NOT NULL,
                    created_at_ms INTEGER NOT NULL,
                    dispatched_at_ms INTEGER,
                    pricing_source TEXT,
                    pricing_verified_at_ms INTEGER,
                    actual_tokens INTEGER,
                    actual_cost_micros INTEGER,
                    actual_seconds INTEGER,
                    settlement_source TEXT,
                    reconciliation_ref TEXT);
                CREATE TABLE IF NOT EXISTS budget_stops(
                    scope_type TEXT NOT NULL CHECK(scope_type IN ('batch','job')),
                    scope_id TEXT NOT NULL,
                    reason TEXT NOT NULL,
                    stopped_at_ms INTEGER NOT NULL,
                    PRIMARY KEY(scope_type,scope_id));
                CREATE TABLE IF NOT EXISTS budget_reviews(
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    scope_type TEXT NOT NULL,
                    scope_id TEXT NOT NULL,
                    evidence_ref TEXT NOT NULL,
                    cleared_at_ms INTEGER NOT NULL);
                CREATE TABLE IF NOT EXISTS paid_call_ledger(
                    call_id TEXT PRIMARY KEY,
                    parent_reservation_id TEXT NOT NULL,
                    max_tokens INTEGER NOT NULL,
                    max_cost_micros INTEGER NOT NULL,
                    state TEXT NOT NULL CHECK(state IN ('dispatched','settled','unresolved')),
                    dispatched_at_ms INTEGER NOT NULL,
                    actual_tokens INTEGER,
                    actual_cost_micros INTEGER,
                    settlement_source TEXT,
                    pricing_source TEXT NOT NULL,
                    pricing_verified_at_ms INTEGER NOT NULL);
                CREATE INDEX IF NOT EXISTS paid_call_ledger_parent ON paid_call_ledger(parent_reservation_id,state);
                CREATE TABLE IF NOT EXISTS free_call_ledger(
                    call_id TEXT PRIMARY KEY,
                    parent_reservation_id TEXT NOT NULL,
                    max_tokens INTEGER NOT NULL,
                    state TEXT NOT NULL CHECK(state IN ('dispatched','settled','unresolved')),
                    dispatched_at_ms INTEGER NOT NULL,
                    pricing_source TEXT NOT NULL,
                    pricing_verified_at_ms INTEGER NOT NULL,
                    actual_tokens INTEGER,
                    settlement_source TEXT);
                CREATE INDEX IF NOT EXISTS free_call_ledger_parent ON free_call_ledger(parent_reservation_id,state);
                CREATE INDEX IF NOT EXISTS budget_reservations_batch ON budget_reservations(batch_id,state);
                CREATE INDEX IF NOT EXISTS budget_reservations_job ON budget_reservations(job_id,state);
                PRAGMA user_version=3;
                COMMIT;").map_err(BudgetError::Sql)?;
        } else if version == 1 {
            connection.execute_batch("BEGIN IMMEDIATE;
                CREATE TABLE IF NOT EXISTS paid_call_ledger(
                    call_id TEXT PRIMARY KEY,
                    parent_reservation_id TEXT NOT NULL,
                    max_tokens INTEGER NOT NULL,
                    max_cost_micros INTEGER NOT NULL,
                    state TEXT NOT NULL CHECK(state IN ('dispatched','settled','unresolved')),
                    dispatched_at_ms INTEGER NOT NULL,
                    actual_tokens INTEGER,
                    actual_cost_micros INTEGER,
                    settlement_source TEXT,
                    pricing_source TEXT NOT NULL,
                    pricing_verified_at_ms INTEGER NOT NULL);
                CREATE INDEX IF NOT EXISTS paid_call_ledger_parent ON paid_call_ledger(parent_reservation_id,state);
                CREATE TABLE IF NOT EXISTS free_call_ledger(
                    call_id TEXT PRIMARY KEY,
                    parent_reservation_id TEXT NOT NULL,
                    max_tokens INTEGER NOT NULL,
                    state TEXT NOT NULL CHECK(state IN ('dispatched','settled','unresolved')),
                    dispatched_at_ms INTEGER NOT NULL,
                    pricing_source TEXT NOT NULL,
                    pricing_verified_at_ms INTEGER NOT NULL,
                    actual_tokens INTEGER,
                    settlement_source TEXT);
                CREATE INDEX IF NOT EXISTS free_call_ledger_parent ON free_call_ledger(parent_reservation_id,state);
                PRAGMA user_version=3;
                COMMIT;").map_err(BudgetError::Sql)?;
        } else if version == 2 {
            connection.execute_batch("BEGIN IMMEDIATE;
                CREATE TABLE IF NOT EXISTS free_call_ledger(
                    call_id TEXT PRIMARY KEY,
                    parent_reservation_id TEXT NOT NULL,
                    max_tokens INTEGER NOT NULL,
                    state TEXT NOT NULL CHECK(state IN ('dispatched','settled','unresolved')),
                    dispatched_at_ms INTEGER NOT NULL,
                    pricing_source TEXT NOT NULL,
                    pricing_verified_at_ms INTEGER NOT NULL,
                    actual_tokens INTEGER,
                    settlement_source TEXT);
                CREATE INDEX IF NOT EXISTS free_call_ledger_parent ON free_call_ledger(parent_reservation_id,state);
                PRAGMA user_version=3;
                COMMIT;").map_err(BudgetError::Sql)?;
        }
        Ok(store)
    }

    fn connect(&self) -> Result<Connection, BudgetError> {
        let connection = Connection::open(&self.path)?;
        connection.busy_timeout(Duration::from_secs(5))?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        Ok(connection)
    }

    /// Atomically install/update immutable scope caps. A smaller limit than
    /// already-consumed or currently reserved capacity is rejected.
    pub fn configure(
        &self,
        batch_id: &str,
        job_id: &str,
        batch: &BudgetPolicy,
        job: &BudgetPolicy,
    ) -> Result<(), BudgetError> {
        validate_scope(batch_id)?;
        validate_scope(job_id)?;
        validate_policy(batch)?;
        validate_policy(job)?;
        let mut connection = self.connect()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        upsert_policy(&tx, "batch", batch_id, batch)?;
        upsert_policy(&tx, "job", job_id, job)?;
        tx.commit()?;
        Ok(())
    }

    /// Reserve the maximum exposure before any provider dispatch. Both scope
    /// caps are checked in one `BEGIN IMMEDIATE` transaction, including active
    /// reservations and settled measured usage.
    pub fn reserve(
        &self,
        request: &ReservationRequest,
        now_ms: u64,
    ) -> Result<Reservation, BudgetError> {
        validate_request(request, now_ms)?;
        let cost = request.max_cost_micros.unwrap_or(0);
        let mut connection = self.connect()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if reservation_exists(&tx, &request.reservation_id)? {
            return Err(BudgetError::Conflict(
                "reservation ID already exists".into(),
            ));
        }
        ensure_capacity(
            &tx,
            "batch",
            &request.batch_id,
            request.max_tokens,
            cost,
            request.max_seconds,
        )?;
        ensure_capacity(
            &tx,
            "job",
            &request.job_id,
            request.max_tokens,
            cost,
            request.max_seconds,
        )?;
        tx.execute("INSERT INTO budget_reservations(reservation_id,batch_id,job_id,task_id,provider_id,model_id,runtime_id,max_tokens,max_cost_micros,max_seconds,state,created_at_ms,pricing_source,pricing_verified_at_ms) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,'reserved',?11,?12,?13)", params![request.reservation_id, request.batch_id, request.job_id, request.task_id, request.provider_id,request.model_id,request.runtime_id,to_i64(request.max_tokens)?, to_i64(cost)?, to_i64(request.max_seconds)?, to_i64(now_ms)?,request.pricing_source,request.pricing_verified_at_ms.map(to_i64).transpose()?])?;
        let value = read_reservation(&tx, &request.reservation_id)?;
        tx.commit()?;
        Ok(value)
    }

    /// Mark the provider boundary. A dispatched reservation cannot be released
    /// as unused; it must settle with measured totals or remain unresolved.
    pub fn mark_dispatched(
        &self,
        reservation_id: &str,
        now_ms: u64,
    ) -> Result<Reservation, BudgetError> {
        let mut connection = self.connect()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let state = read_reservation(&tx, reservation_id)?.state;
        if state != ReservationState::Reserved {
            return Err(BudgetError::Conflict(
                "only a fresh reservation can be dispatched".into(),
            ));
        }
        tx.execute("UPDATE budget_reservations SET state='dispatched',dispatched_at_ms=?2 WHERE reservation_id=?1", params![reservation_id,to_i64(now_ms)?])?;
        let value = read_reservation(&tx, reservation_id)?;
        tx.commit()?;
        Ok(value)
    }

    /// Release a reservation only when the caller has proven dispatch never
    /// started. This makes retries before provider start free and race-safe.
    pub fn release_before_dispatch(&self, reservation_id: &str) -> Result<(), BudgetError> {
        let mut connection = self.connect()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let state = read_reservation(&tx, reservation_id)?.state;
        if state != ReservationState::Reserved {
            return Err(BudgetError::Conflict(
                "dispatched or terminal reservations cannot be released as unused".into(),
            ));
        }
        tx.execute(
            "UPDATE budget_reservations SET state='released' WHERE reservation_id=?1",
            [reservation_id],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Settle measured totals. Values above the reserved envelope are recorded
    /// and flagged by an error, since provider-side billing cannot be reversed.
    /// Missing measured values keep the full reservation charged as unresolved.
    pub fn settle(
        &self,
        reservation_id: &str,
        actual_tokens: Option<u64>,
        actual_cost_micros: Option<u64>,
        actual_seconds: Option<u64>,
        source: &str,
        now_ms: u64,
    ) -> Result<Reservation, BudgetError> {
        validate_scope(source)?;
        let mut connection = self.connect()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = read_reservation(&tx, reservation_id)?;
        if current.state != ReservationState::Dispatched {
            return Err(BudgetError::Conflict("only a dispatched reservation can be settled; unresolved records require reconciliation evidence".into()));
        }
        let (Some(tokens), Some(cost), Some(seconds)) =
            (actual_tokens, actual_cost_micros, actual_seconds)
        else {
            tx.execute(
                "UPDATE budget_reservations SET state='unresolved',settlement_source=?2 WHERE reservation_id=?1",
                params![reservation_id,source],
            )?;
            let result = read_reservation(&tx, reservation_id)?;
            tx.commit()?;
            return Ok(result);
        };
        tx.execute("UPDATE budget_reservations SET state='settled',actual_tokens=?2,actual_cost_micros=?3,actual_seconds=?4,settlement_source=?5 WHERE reservation_id=?1",params![reservation_id,to_i64(tokens)?,to_i64(cost)?,to_i64(seconds)?,source])?;
        for (kind, id) in [
            ("batch", current.batch_id.as_str()),
            ("job", current.job_id.as_str()),
        ] {
            tx.execute("UPDATE budget_policies SET tokens_used=tokens_used+?3,cost_used=cost_used+?4,seconds_used=seconds_used+?5 WHERE scope_type=?1 AND scope_id=?2",params![kind,id,to_i64(tokens)?,to_i64(cost)?,to_i64(seconds)?])?;
        }
        let overrun = tokens > current.max_tokens
            || cost > current.max_cost_micros
            || seconds > current.max_seconds;
        if overrun {
            record_stop(
                &tx,
                "batch",
                &current.batch_id,
                "measured usage exceeded its reservation",
                now_ms,
            )?;
            record_stop(
                &tx,
                "job",
                &current.job_id,
                "measured usage exceeded its reservation",
                now_ms,
            )?;
        }
        let result = read_reservation(&tx, reservation_id)?;
        tx.commit()?;
        if overrun {
            return Err(BudgetError::Exhausted(
                "measured usage exceeded its pre-dispatch reservation; further admission will stop"
                    .into(),
            ));
        }
        Ok(result)
    }

    /// Resolve unknown provider billing exactly once. The reference identifies
    /// the external statement or provider response used for reconciliation.
    pub fn reconcile_unresolved(
        &self,
        reservation_id: &str,
        evidence_ref: &str,
        actual_tokens: u64,
        actual_cost_micros: u64,
        actual_seconds: u64,
        now_ms: u64,
    ) -> Result<Reservation, BudgetError> {
        validate_scope(evidence_ref)?;
        let mut connection = self.connect()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = read_reservation(&tx, reservation_id)?;
        if current.state != ReservationState::Unresolved {
            return Err(BudgetError::Conflict(
                "only an unresolved reservation can be reconciled".into(),
            ));
        }
        tx.execute("UPDATE budget_reservations SET state='settled',actual_tokens=?2,actual_cost_micros=?3,actual_seconds=?4,settlement_source='reconciled',reconciliation_ref=?5 WHERE reservation_id=?1",params![reservation_id,to_i64(actual_tokens)?,to_i64(actual_cost_micros)?,to_i64(actual_seconds)?,evidence_ref])?;
        for (kind, id) in [
            ("batch", current.batch_id.as_str()),
            ("job", current.job_id.as_str()),
        ] {
            tx.execute("UPDATE budget_policies SET tokens_used=tokens_used+?3,cost_used=cost_used+?4,seconds_used=seconds_used+?5 WHERE scope_type=?1 AND scope_id=?2",params![kind,id,to_i64(actual_tokens)?,to_i64(actual_cost_micros)?,to_i64(actual_seconds)?])?;
        }
        let overrun = actual_tokens > current.max_tokens
            || actual_cost_micros > current.max_cost_micros
            || actual_seconds > current.max_seconds;
        if overrun {
            record_stop(
                &tx,
                "batch",
                &current.batch_id,
                "reconciled usage exceeded its reservation",
                now_ms,
            )?;
            record_stop(
                &tx,
                "job",
                &current.job_id,
                "reconciled usage exceeded its reservation",
                now_ms,
            )?;
        }
        let result = read_reservation(&tx, reservation_id)?;
        tx.commit()?;
        if overrun {
            return Err(BudgetError::Exhausted("reconciled usage exceeded its pre-dispatch reservation; affected scopes were stopped".into()));
        }
        Ok(result)
    }

    /// Clear an overrun stop only after an operator has supplied review evidence
    /// and the stored policy now covers all consumed and active reserved usage.
    pub fn clear_stop_after_review(
        &self,
        scope_type: &str,
        scope_id: &str,
        evidence_ref: &str,
        now_ms: u64,
    ) -> Result<(), BudgetError> {
        if !matches!(scope_type, "batch" | "job") {
            return Err(BudgetError::Invalid(
                "scope type must be batch or job".into(),
            ));
        }
        validate_scope(scope_id)?;
        validate_scope(evidence_ref)?;
        let mut connection = self.connect()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let p:Option<(i64,i64,i64,i64,i64,i64,i64)>=tx.query_row("SELECT max_tokens,max_cost_micros,max_seconds,max_concurrency,tokens_used,cost_used,seconds_used FROM budget_policies WHERE scope_type=?1 AND scope_id=?2",params![scope_type,scope_id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?))).optional()?;
        let Some(p) = p else {
            return Err(BudgetError::Invalid(
                "budget policy is not configured".into(),
            ));
        };
        let a = active_totals(&tx, scope_type, scope_id)?;
        if p.4 as i128 + a.0 as i128 > p.0 as i128
            || p.5 as i128 + a.1 as i128 > p.1 as i128
            || p.6 as i128 + a.2 as i128 > p.2 as i128
            || a.3 > p.3
        {
            return Err(BudgetError::Exhausted(
                "reviewed policy still does not cover used and reserved capacity".into(),
            ));
        }
        tx.execute("INSERT INTO budget_reviews(scope_type,scope_id,evidence_ref,cleared_at_ms) VALUES(?1,?2,?3,?4)",params![scope_type,scope_id,evidence_ref,to_i64(now_ms)?])?;
        tx.execute(
            "DELETE FROM budget_stops WHERE scope_type=?1 AND scope_id=?2",
            params![scope_type, scope_id],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn get(&self, reservation_id: &str) -> Result<Reservation, BudgetError> {
        let connection = self.connect()?;
        read_reservation(&connection, reservation_id)
    }

    /// Read a reservation when present without conflating absence with storage
    /// or decoding errors. Recovery uses deterministic attempt IDs to inspect
    /// the reserve-before-checkpoint crash window safely.
    pub fn get_optional(&self, reservation_id: &str) -> Result<Option<Reservation>, BudgetError> {
        validate_scope(reservation_id)?;
        let connection = self.connect()?;
        let exists: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM budget_reservations WHERE reservation_id=?1)",
            [reservation_id],
            |row| row.get(0),
        )?;
        if exists {
            Ok(Some(read_reservation(&connection, reservation_id)?))
        } else {
            Ok(None)
        }
    }

    /// Atomically record that a paid child call is about to cross the upstream
    /// boundary. A call is durable before network dispatch and permanently
    /// consumes its exact ID. Child ceilings draw from the parent reservation;
    /// they are not counted against policy totals a second time.
    pub fn reserve_paid_call(&self, request: &PaidCallRequest) -> Result<PaidCall, BudgetError> {
        validate_scope(&request.parent_reservation_id)?;
        validate_scope(&request.call_id)?;
        validate_scope(&request.provider_id)?;
        validate_scope(&request.model_id)?;
        validate_scope(&request.runtime_id)?;
        validate_scope(&request.pricing_source)?;
        if request.max_tokens == 0 || request.max_cost_micros == 0 || request.max_calls == 0 {
            return Err(BudgetError::Invalid(
                "paid call token, cost, and call ceilings must be positive".into(),
            ));
        }
        if request.pricing_verified_at_ms > request.now_ms
            || request
                .now_ms
                .saturating_sub(request.pricing_verified_at_ms)
                > 24 * 60 * 60 * 1000
        {
            return Err(BudgetError::Invalid(
                "paid call pricing must be verified within the last 24 hours".into(),
            ));
        }
        let mut connection = self.connect()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let parent = read_reservation(&tx, &request.parent_reservation_id)?;
        if parent.state != ReservationState::Dispatched {
            return Err(BudgetError::Conflict(
                "paid child calls require a durably dispatched parent reservation".into(),
            ));
        }
        if parent.provider_id != request.provider_id
            || parent.model_id != request.model_id
            || parent.runtime_id != request.runtime_id
        {
            return Err(BudgetError::Conflict(
                "paid child call provider, model, and runtime must match the parent reservation"
                    .into(),
            ));
        }
        if parent.pricing_source.as_deref() != Some(request.pricing_source.as_str())
            || parent.pricing_verified_at_ms != Some(request.pricing_verified_at_ms)
        {
            return Err(BudgetError::Conflict(
                "paid child pricing provenance must match the parent reservation".into(),
            ));
        }
        let stopped: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM budget_stops WHERE (scope_type='batch' AND scope_id=?1) OR (scope_type='job' AND scope_id=?2))",
            params![parent.batch_id,parent.job_id], |row| row.get(0))?;
        if stopped {
            return Err(BudgetError::Exhausted(
                "parent budget scope is stopped for review".into(),
            ));
        }
        let (count, tokens, cost): (i64, i64, i64) = tx.query_row(
            "SELECT COUNT(*),COALESCE(SUM(CASE WHEN state='settled' THEN actual_tokens ELSE max_tokens END),0),COALESCE(SUM(CASE WHEN state='settled' THEN actual_cost_micros ELSE max_cost_micros END),0) FROM paid_call_ledger WHERE parent_reservation_id=?1",
            [&request.parent_reservation_id], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?)))?;
        if count >= i64::from(request.max_calls) {
            return Err(BudgetError::Exhausted(
                "paid call count ceiling is exhausted".into(),
            ));
        }
        if tokens as i128 + request.max_tokens as i128 > parent.max_tokens as i128 {
            return Err(BudgetError::Exhausted(
                "paid call token ceiling exceeds the parent reservation".into(),
            ));
        }
        if cost as i128 + request.max_cost_micros as i128 > parent.max_cost_micros as i128 {
            return Err(BudgetError::Exhausted(
                "paid call cost ceiling exceeds the parent reservation".into(),
            ));
        }
        tx.execute("INSERT INTO paid_call_ledger(call_id,parent_reservation_id,max_tokens,max_cost_micros,state,dispatched_at_ms,pricing_source,pricing_verified_at_ms) VALUES(?1,?2,?3,?4,'dispatched',?5,?6,?7)",params![request.call_id,request.parent_reservation_id,to_i64(request.max_tokens)?,to_i64(request.max_cost_micros)?,to_i64(request.now_ms)?,request.pricing_source,to_i64(request.pricing_verified_at_ms)?])?;
        let call = read_paid_call(&tx, &request.call_id)?;
        tx.commit()?;
        Ok(call)
    }

    /// Record a strictly zero-price request under a separate, explicitly
    /// zero-cost parent reservation. The paid-call API continues to reject
    /// zero-cost requests.
    pub fn reserve_free_call(&self, request: &FreeCallRequest) -> Result<FreeCall, BudgetError> {
        for value in [
            request.parent_reservation_id.as_str(),
            request.call_id.as_str(),
            request.provider_id.as_str(),
            request.model_id.as_str(),
            request.runtime_id.as_str(),
            request.pricing_source.as_str(),
        ] {
            validate_scope(value)?;
        }
        if request.max_tokens == 0 || request.max_calls == 0 || request.max_calls > 1000 {
            return Err(BudgetError::Invalid(
                "free call token and request ceilings must be positive and bounded".into(),
            ));
        }
        if request.pricing_verified_at_ms > request.now_ms
            || request
                .now_ms
                .saturating_sub(request.pricing_verified_at_ms)
                > 24 * 60 * 60 * 1000
        {
            return Err(BudgetError::Invalid(
                "free model pricing must be freshly verified".into(),
            ));
        }
        let mut connection = self.connect()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let parent = read_reservation(&tx, &request.parent_reservation_id)?;
        if parent.state != ReservationState::Dispatched
            || parent.max_cost_micros != 0
            || parent.provider_id != request.provider_id
            || parent.model_id != request.model_id
            || parent.runtime_id != request.runtime_id
            || parent.pricing_source.as_deref() != Some(request.pricing_source.as_str())
            || parent.pricing_verified_at_ms != Some(request.pricing_verified_at_ms)
        {
            return Err(BudgetError::Conflict(
                "free call requires an exactly matching dispatched zero-cost parent reservation"
                    .into(),
            ));
        }
        let stopped: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM budget_stops WHERE (scope_type='batch' AND scope_id=?1) OR (scope_type='job' AND scope_id=?2))",
            params![parent.batch_id, parent.job_id], |row| row.get(0))?;
        if stopped {
            return Err(BudgetError::Exhausted(
                "parent budget scope is stopped for review".into(),
            ));
        }
        let (count, tokens): (i64, i64) = tx.query_row(
            "SELECT COUNT(*),COALESCE(SUM(CASE WHEN state='settled' THEN actual_tokens ELSE max_tokens END),0) FROM free_call_ledger WHERE parent_reservation_id=?1",
            [&request.parent_reservation_id], |row| Ok((row.get(0)?, row.get(1)?)))?;
        if count >= i64::from(request.max_calls) {
            return Err(BudgetError::Exhausted(
                "free model request count ceiling is exhausted".into(),
            ));
        }
        if tokens as i128 + request.max_tokens as i128 > parent.max_tokens as i128 {
            return Err(BudgetError::Exhausted(
                "free model token ceiling exceeds the parent reservation".into(),
            ));
        }
        tx.execute("INSERT INTO free_call_ledger(call_id,parent_reservation_id,max_tokens,state,dispatched_at_ms,pricing_source,pricing_verified_at_ms) VALUES(?1,?2,?3,'dispatched',?4,?5,?6)", params![request.call_id, request.parent_reservation_id, to_i64(request.max_tokens)?, to_i64(request.now_ms)?, request.pricing_source, to_i64(request.pricing_verified_at_ms)?])?;
        let call = read_free_call(&tx, &request.call_id)?;
        tx.commit()?;
        Ok(call)
    }

    pub fn settle_free_call(
        &self,
        call_id: &str,
        actual_tokens: Option<u64>,
        source: &str,
    ) -> Result<FreeCall, BudgetError> {
        validate_scope(call_id)?;
        validate_scope(source)?;
        let mut connection = self.connect()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = read_free_call(&tx, call_id)?;
        if current.state != PaidCallState::Dispatched {
            return Err(BudgetError::Conflict(
                "only a dispatched free call can be settled".into(),
            ));
        }
        let (state, tokens) = match actual_tokens {
            Some(tokens) => ("settled", Some(tokens)),
            None => ("unresolved", None),
        };
        tx.execute("UPDATE free_call_ledger SET state=?2,actual_tokens=?3,settlement_source=?4 WHERE call_id=?1", params![call_id, state, tokens.map(to_i64).transpose()?, source])?;
        if tokens.is_some_and(|value| value > current.max_tokens) {
            let parent = read_reservation(&tx, &current.parent_reservation_id)?;
            record_stop(
                &tx,
                "batch",
                &parent.batch_id,
                "free provider call exceeded its reserved token ceiling",
                current.dispatched_at_ms,
            )?;
            record_stop(
                &tx,
                "job",
                &parent.job_id,
                "free provider call exceeded its reserved token ceiling",
                current.dispatched_at_ms,
            )?;
        }
        let result = read_free_call(&tx, call_id)?;
        tx.commit()?;
        Ok(result)
    }

    /// Settle a paid call from provider-reported usage and cost. Unknown data
    /// leaves the maximum charged as unresolved; it never fabricates zero use.
    pub fn settle_paid_call(
        &self,
        call_id: &str,
        actual_tokens: Option<u64>,
        actual_cost_micros: Option<u64>,
        source: &str,
    ) -> Result<PaidCall, BudgetError> {
        validate_scope(call_id)?;
        validate_scope(source)?;
        let mut connection = self.connect()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = read_paid_call(&tx, call_id)?;
        if current.state != PaidCallState::Dispatched {
            return Err(BudgetError::Conflict(
                "only a dispatched paid call can be settled".into(),
            ));
        }
        let (state, tokens, cost) = match (actual_tokens, actual_cost_micros) {
            (Some(tokens), Some(cost)) => ("settled", Some(tokens), Some(cost)),
            _ => ("unresolved", None, None),
        };
        tx.execute("UPDATE paid_call_ledger SET state=?2,actual_tokens=?3,actual_cost_micros=?4,settlement_source=?5 WHERE call_id=?1",params![call_id,state,tokens.map(to_i64).transpose()?,cost.map(to_i64).transpose()?,source])?;
        let overrun = tokens.is_some_and(|value| value > current.max_tokens)
            || cost.is_some_and(|value| value > current.max_cost_micros);
        if overrun {
            let parent = read_reservation(&tx, &current.parent_reservation_id)?;
            record_stop(
                &tx,
                "batch",
                &parent.batch_id,
                "paid child call exceeded its reserved ceiling",
                current.dispatched_at_ms,
            )?;
            record_stop(
                &tx,
                "job",
                &parent.job_id,
                "paid child call exceeded its reserved ceiling",
                current.dispatched_at_ms,
            )?;
        }
        let result = read_paid_call(&tx, call_id)?;
        tx.commit()?;
        if overrun {
            return Err(BudgetError::Exhausted("provider-reported paid usage exceeded the per-call ceiling; affected scopes were stopped".into()));
        }
        Ok(result)
    }

    pub fn get_paid_call(&self, call_id: &str) -> Result<PaidCall, BudgetError> {
        let connection = self.connect()?;
        read_paid_call(&connection, call_id)
    }

    pub fn paid_calls_for_parent(
        &self,
        parent_reservation_id: &str,
    ) -> Result<Vec<PaidCall>, BudgetError> {
        validate_scope(parent_reservation_id)?;
        let connection = self.connect()?;
        let mut statement = connection.prepare(
            "SELECT call_id FROM paid_call_ledger WHERE parent_reservation_id=?1 ORDER BY dispatched_at_ms,call_id",
        )?;
        let ids = statement
            .query_map([parent_reservation_id], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        ids.iter()
            .map(|id| read_paid_call(&connection, id))
            .collect()
    }

    pub fn free_calls_for_parent(&self, parent_id: &str) -> Result<Vec<FreeCall>, BudgetError> {
        validate_scope(parent_id)?;
        let connection = self.connect()?;
        let mut statement = connection.prepare(
            "SELECT call_id FROM free_call_ledger WHERE parent_reservation_id=?1 ORDER BY dispatched_at_ms,call_id",
        )?;
        let ids = statement
            .query_map([parent_id], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        ids.iter()
            .map(|id| read_free_call(&connection, id))
            .collect()
    }
}

fn validate_scope(value: &str) -> Result<(), BudgetError> {
    if value.trim().is_empty() || value.len() > 128 || value.chars().any(char::is_control) {
        return Err(BudgetError::Invalid(
            "scope IDs must be 1-128 printable characters".into(),
        ));
    }
    Ok(())
}
fn validate_policy(value: &BudgetPolicy) -> Result<(), BudgetError> {
    if value.max_tokens == 0 || value.max_seconds == 0 || value.max_concurrency == 0 {
        return Err(BudgetError::Invalid("token, time, and concurrency caps must be positive; cost may be zero for local-only budgets".into()));
    }
    Ok(())
}
fn validate_request(value: &ReservationRequest, now_ms: u64) -> Result<(), BudgetError> {
    validate_scope(&value.reservation_id)?;
    validate_scope(&value.batch_id)?;
    validate_scope(&value.job_id)?;
    validate_scope(&value.task_id)?;
    validate_scope(&value.provider_id)?;
    validate_scope(&value.model_id)?;
    validate_scope(&value.runtime_id)?;
    if value.max_tokens == 0 || value.max_seconds == 0 {
        return Err(BudgetError::Invalid(
            "token and time reservation ceilings must be positive".into(),
        ));
    }
    if value.is_paid && value.max_cost_micros.is_none() {
        return Err(BudgetError::Invalid(
            "paid admission requires a verified pricing estimate and maximum cost reservation"
                .into(),
        ));
    }
    if value.is_paid
        && value.pricing_source.as_deref().is_none_or(|source| {
            source.trim().is_empty() || source.len() > 128 || source.chars().any(char::is_control)
        })
    {
        return Err(BudgetError::Invalid(
            "paid admission requires pricing provenance".into(),
        ));
    }
    if value.is_paid
        && value.pricing_verified_at_ms.is_none_or(|checked| {
            checked > now_ms || now_ms.saturating_sub(checked) > 24 * 60 * 60 * 1000
        })
    {
        return Err(BudgetError::Invalid(
            "paid admission requires pricing verified within the last 24 hours".into(),
        ));
    }
    Ok(())
}
fn to_i64(value: u64) -> Result<i64, BudgetError> {
    i64::try_from(value)
        .map_err(|_| BudgetError::Invalid("budget value exceeds SQLite integer range".into()))
}
fn upsert_policy(
    tx: &Transaction<'_>,
    kind: &str,
    id: &str,
    policy: &BudgetPolicy,
) -> Result<(), BudgetError> {
    let values = (
        to_i64(policy.max_tokens)?,
        to_i64(policy.max_cost_micros)?,
        to_i64(policy.max_seconds)?,
        i64::from(policy.max_concurrency),
    );
    tx.execute("INSERT INTO budget_policies(scope_type,scope_id,max_tokens,max_cost_micros,max_seconds,max_concurrency) VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT(scope_type,scope_id) DO UPDATE SET max_tokens=excluded.max_tokens,max_cost_micros=excluded.max_cost_micros,max_seconds=excluded.max_seconds,max_concurrency=excluded.max_concurrency",params![kind,id,values.0,values.1,values.2,values.3])?;
    let used: (i64,i64,i64) = tx.query_row("SELECT tokens_used,cost_used,seconds_used FROM budget_policies WHERE scope_type=?1 AND scope_id=?2",params![kind,id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
    let reserved = active_totals(tx, kind, id)?;
    if (used.0 as i128 + reserved.0 as i128 > values.0 as i128)
        || (used.1 as i128 + reserved.1 as i128 > values.1 as i128)
        || (used.2 as i128 + reserved.2 as i128 > values.2 as i128)
    {
        return Err(BudgetError::Exhausted(
            "new policy is below already used or reserved capacity".into(),
        ));
    }
    Ok(())
}
fn reservation_exists(tx: &Transaction<'_>, id: &str) -> Result<bool, BudgetError> {
    Ok(tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM budget_reservations WHERE reservation_id=?1)",
        [id],
        |r| r.get(0),
    )?)
}
fn active_totals(
    tx: &Transaction<'_>,
    kind: &str,
    id: &str,
) -> Result<(i64, i64, i64, i64), BudgetError> {
    let column = if kind == "batch" {
        "batch_id"
    } else {
        "job_id"
    };
    let sql = format!("SELECT COALESCE(SUM(max_tokens),0),COALESCE(SUM(max_cost_micros),0),COALESCE(SUM(max_seconds),0),COUNT(CASE WHEN state IN ('reserved','dispatched') THEN 1 END) FROM budget_reservations WHERE {column}=?1 AND state IN ('reserved','dispatched','unresolved')");
    Ok(tx.query_row(&sql, [id], |r| {
        Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
    })?)
}
fn ensure_capacity(
    tx: &Transaction<'_>,
    kind: &str,
    id: &str,
    tokens: u64,
    cost: u64,
    seconds: u64,
) -> Result<(), BudgetError> {
    let stopped: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM budget_stops WHERE scope_type=?1 AND scope_id=?2)",
        params![kind, id],
        |r| r.get(0),
    )?;
    if stopped {
        return Err(BudgetError::Exhausted(format!(
            "{kind} budget scope is stopped for review"
        )));
    }
    let policy: Option<(i64,i64,i64,i64,i64,i64,i64)> = tx.query_row("SELECT max_tokens,max_cost_micros,max_seconds,max_concurrency,tokens_used,cost_used,seconds_used FROM budget_policies WHERE scope_type=?1 AND scope_id=?2",params![kind,id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?))).optional()?;
    let Some(policy) = policy else {
        return Err(BudgetError::Invalid(format!(
            "{kind} budget policy is not configured"
        )));
    };
    let active = active_totals(tx, kind, id)?;
    if policy.4 as i128 + active.0 as i128 + tokens as i128 > policy.0 as i128 {
        return Err(BudgetError::Exhausted(format!(
            "{kind} token cap would be exceeded"
        )));
    }
    if policy.5 as i128 + active.1 as i128 + cost as i128 > policy.1 as i128 {
        return Err(BudgetError::Exhausted(format!(
            "{kind} cost cap would be exceeded"
        )));
    }
    if policy.6 as i128 + active.2 as i128 + seconds as i128 > policy.2 as i128 {
        return Err(BudgetError::Exhausted(format!(
            "{kind} time cap would be exceeded"
        )));
    }
    if active.3 >= policy.3 {
        return Err(BudgetError::Exhausted(format!(
            "{kind} concurrency cap is full"
        )));
    }
    Ok(())
}
fn record_stop(
    tx: &Transaction<'_>,
    kind: &str,
    id: &str,
    reason: &str,
    at: u64,
) -> Result<(), BudgetError> {
    tx.execute("INSERT OR IGNORE INTO budget_stops(scope_type,scope_id,reason,stopped_at_ms) VALUES(?1,?2,?3,?4)",params![kind,id,reason,to_i64(at)?])?;
    Ok(())
}
fn read_reservation(c: &Connection, id: &str) -> Result<Reservation, BudgetError> {
    c.query_row("SELECT reservation_id,batch_id,job_id,task_id,provider_id,model_id,runtime_id,max_tokens,max_cost_micros,max_seconds,state,created_at_ms,dispatched_at_ms,pricing_source,pricing_verified_at_ms,settlement_source,reconciliation_ref FROM budget_reservations WHERE reservation_id=?1",[id],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?,r.get::<_,String>(4)?,r.get::<_,String>(5)?,r.get::<_,String>(6)?,r.get::<_,i64>(7)?,r.get::<_,i64>(8)?,r.get::<_,i64>(9)?,r.get::<_,String>(10)?,r.get::<_,i64>(11)?,r.get::<_,Option<i64>>(12)?,r.get::<_,Option<String>>(13)?,r.get::<_,Option<i64>>(14)?,r.get::<_,Option<String>>(15)?,r.get::<_,Option<String>>(16)?))).optional()?.ok_or_else(||BudgetError::Invalid("reservation was not found".into())).and_then(|v|Ok(Reservation{reservation_id:v.0,batch_id:v.1,job_id:v.2,task_id:v.3,provider_id:v.4,model_id:v.5,runtime_id:v.6,max_tokens:v.7 as u64,max_cost_micros:v.8 as u64,max_seconds:v.9 as u64,state:ReservationState::parse(&v.10)?,created_at_ms:v.11 as u64,dispatched_at_ms:v.12.map(|n|n as u64),pricing_source:v.13,pricing_verified_at_ms:v.14.map(|n|n as u64),settlement_source:v.15,reconciliation_ref:v.16}))
}

fn read_paid_call(c: &Connection, id: &str) -> Result<PaidCall, BudgetError> {
    c.query_row(
        "SELECT parent_reservation_id,call_id,max_tokens,max_cost_micros,state,dispatched_at_ms,actual_tokens,actual_cost_micros,settlement_source FROM paid_call_ledger WHERE call_id=?1",
        [id],
        |row| Ok((
            row.get::<_, String>(0)?, row.get::<_, String>(1)?,
            row.get::<_, i64>(2)?, row.get::<_, i64>(3)?, row.get::<_, String>(4)?,
            row.get::<_, i64>(5)?, row.get::<_, Option<i64>>(6)?,
            row.get::<_, Option<i64>>(7)?, row.get::<_, Option<String>>(8)?,
        )),
    )
    .optional()?
    .ok_or_else(|| BudgetError::Invalid("paid call was not found".into()))
    .and_then(|value| Ok(PaidCall {
        parent_reservation_id: value.0,
        call_id: value.1,
        max_tokens: u64::try_from(value.2).map_err(|_| BudgetError::Invalid("stored paid call token ceiling is invalid".into()))?,
        max_cost_micros: u64::try_from(value.3).map_err(|_| BudgetError::Invalid("stored paid call cost ceiling is invalid".into()))?,
        state: PaidCallState::parse(&value.4)?,
        dispatched_at_ms: u64::try_from(value.5).map_err(|_| BudgetError::Invalid("stored paid call dispatch time is invalid".into()))?,
        actual_tokens: value.6.map(u64::try_from).transpose().map_err(|_| BudgetError::Invalid("stored paid call token usage is invalid".into()))?,
        actual_cost_micros: value.7.map(u64::try_from).transpose().map_err(|_| BudgetError::Invalid("stored paid call cost usage is invalid".into()))?,
        settlement_source: value.8,
    }))
}

fn read_free_call(c: &Connection, id: &str) -> Result<FreeCall, BudgetError> {
    c.query_row(
        "SELECT parent_reservation_id,call_id,max_tokens,state,dispatched_at_ms,actual_tokens,settlement_source FROM free_call_ledger WHERE call_id=?1",
        [id],
        |row| Ok((
            row.get::<_, String>(0)?, row.get::<_, String>(1)?,
            row.get::<_, i64>(2)?, row.get::<_, String>(3)?,
            row.get::<_, i64>(4)?, row.get::<_, Option<i64>>(5)?,
            row.get::<_, Option<String>>(6)?,
        )),
    )
    .optional()?
    .ok_or_else(|| BudgetError::Invalid("free call was not found".into()))
    .and_then(|value| Ok(FreeCall {
        parent_reservation_id: value.0,
        call_id: value.1,
        max_tokens: u64::try_from(value.2).map_err(|_| BudgetError::Invalid("stored free call token ceiling is invalid".into()))?,
        state: PaidCallState::parse(&value.3)?,
        dispatched_at_ms: u64::try_from(value.4).map_err(|_| BudgetError::Invalid("stored free call dispatch time is invalid".into()))?,
        actual_tokens: value.5.map(u64::try_from).transpose().map_err(|_| BudgetError::Invalid("stored free call usage is invalid".into()))?,
        settlement_source: value.6,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};
    use std::thread;
    fn setup() -> (tempfile::TempDir, BudgetStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = BudgetStore::open(dir.path().join("budgets.sqlite3")).unwrap();
        let p = BudgetPolicy {
            max_tokens: 1000,
            max_cost_micros: 1000,
            max_seconds: 1000,
            max_concurrency: 2,
        };
        store.configure("batch", "job-a", &p, &p).unwrap();
        (dir, store)
    }

    #[test]
    fn version_one_budget_store_migrates_paid_call_ledger_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("budgets.sqlite3");
        let store = BudgetStore::open(&path).unwrap();
        let p = BudgetPolicy {
            max_tokens: 1000,
            max_cost_micros: 1000,
            max_seconds: 1000,
            max_concurrency: 2,
        };
        store.configure("batch", "job-a", &p, &p).unwrap();
        let mut parent_request = request("legacy-parent", "job-a");
        parent_request.is_paid = false;
        parent_request.pricing_source = None;
        parent_request.pricing_verified_at_ms = None;
        store.reserve(&parent_request, 1).unwrap();
        store.mark_dispatched("legacy-parent", 2).unwrap();
        store
            .settle(
                "legacy-parent",
                Some(123),
                Some(45),
                Some(6),
                "legacy-provider",
                3,
            )
            .unwrap();
        drop(store);

        // Recreate the on-disk shape of a v1 store: preserve real policy and
        // settlement rows, removing only the v2 paid-call table and version.
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch("DROP TABLE paid_call_ledger;")
            .unwrap();
        connection.pragma_update(None, "user_version", 1).unwrap();
        drop(connection);

        let store = BudgetStore::open(&path).unwrap();
        let parent = store.get("legacy-parent").unwrap();
        assert_eq!(parent.state, ReservationState::Settled);
        let connection = store.connect().unwrap();
        let usage: (i64, i64) = connection
            .query_row(
                "SELECT tokens_used,cost_used FROM budget_policies WHERE scope_type='batch' AND scope_id='batch'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(usage, (123, 45));
        let connection = store.connect().unwrap();
        let version: i64 = connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        let ledger_exists: bool = connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='paid_call_ledger')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(version, 3);
        assert!(ledger_exists);
        let free_ledger_exists: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='free_call_ledger')",
            [],
            |row| row.get(0),
        ).unwrap();
        assert!(free_ledger_exists);
    }
    fn request(id: &str, job: &str) -> ReservationRequest {
        ReservationRequest {
            reservation_id: id.into(),
            batch_id: "batch".into(),
            job_id: job.into(),
            task_id: "implementation".into(),
            provider_id: "provider-a".into(),
            model_id: "model-a".into(),
            runtime_id: "runtime-a".into(),
            max_tokens: 400,
            max_cost_micros: Some(400),
            max_seconds: 300,
            is_paid: true,
            pricing_source: Some("verified-test-catalog".into()),
            pricing_verified_at_ms: Some(1),
        }
    }
    fn paid_call(
        parent: &str,
        call_id: &str,
        tokens: u64,
        cost: u64,
        now_ms: u64,
    ) -> PaidCallRequest {
        PaidCallRequest {
            parent_reservation_id: parent.into(),
            call_id: call_id.into(),
            provider_id: "provider-a".into(),
            model_id: "model-a".into(),
            runtime_id: "runtime-a".into(),
            max_tokens: tokens,
            max_cost_micros: cost,
            max_calls: 3,
            pricing_source: "verified-test-catalog".into(),
            pricing_verified_at_ms: 1,
            now_ms,
        }
    }

    #[test]
    fn paid_child_ledger_requires_dispatched_parent_and_stays_inside_its_envelope() {
        let (_dir, store) = setup();
        let parent = store.reserve(&request("parent", "job-a"), 1).unwrap();
        assert!(matches!(
            store.reserve_paid_call(&paid_call("parent", "call-before-parent", 100, 100, 2)),
            Err(BudgetError::Conflict(_))
        ));
        store.mark_dispatched(&parent.reservation_id, 2).unwrap();

        let first = store
            .reserve_paid_call(&paid_call("parent", "call-1", 250, 100, 3))
            .unwrap();
        assert_eq!(first.state, PaidCallState::Dispatched);
        // The child reserve is not added to global policy totals a second time.
        assert!(store.reserve(&request("other-parent", "job-a"), 4).is_ok());
        assert!(matches!(
            store.reserve_paid_call(&paid_call("parent", "call-2", 200, 100, 5)),
            Err(BudgetError::Exhausted(_))
        ));
        assert!(matches!(
            store.reserve_paid_call(&paid_call("parent", "call-1", 1, 1, 6)),
            Err(BudgetError::Sql(_))
        ));
    }

    #[test]
    fn paid_child_unknown_billing_retains_ceiling_and_blocks_retries_at_call_limit() {
        let (_dir, store) = setup();
        let parent = store.reserve(&request("parent", "job-a"), 1).unwrap();
        store.mark_dispatched(&parent.reservation_id, 2).unwrap();
        store
            .reserve_paid_call(&paid_call("parent", "call-1", 200, 100, 3))
            .unwrap();
        assert_eq!(
            store
                .settle_paid_call("call-1", None, None, "openrouter-unknown")
                .unwrap()
                .state,
            PaidCallState::Unresolved
        );
        assert_eq!(store.get_paid_call("call-1").unwrap().max_cost_micros, 100);
        let mut too_many = paid_call("parent", "call-2", 1, 1, 4);
        too_many.max_calls = 1;
        assert!(matches!(
            store.reserve_paid_call(&too_many),
            Err(BudgetError::Exhausted(_))
        ));
        assert!(matches!(
            store.settle_paid_call("call-1", Some(0), Some(0), "fake-retry-zero"),
            Err(BudgetError::Conflict(_))
        ));
    }

    #[test]
    fn free_child_uses_separate_zero_cost_ledger_and_keeps_paid_guard() {
        let (_dir, store) = setup();
        let mut parent_request = request("free-parent", "job-a");
        parent_request.max_cost_micros = Some(0);
        parent_request.is_paid = false;
        parent_request.model_id = "vendor/model:free".into();
        parent_request.runtime_id = "aider".into();
        parent_request.pricing_source = Some("verified-zero-price".into());
        parent_request.pricing_verified_at_ms = Some(1);
        let parent = store.reserve(&parent_request, 1).unwrap();
        store.mark_dispatched(&parent.reservation_id, 2).unwrap();
        let child = FreeCallRequest {
            parent_reservation_id: "free-parent".into(),
            call_id: "free-call-1".into(),
            provider_id: "provider-a".into(),
            model_id: "vendor/model:free".into(),
            runtime_id: "aider".into(),
            max_tokens: 100,
            max_calls: 1,
            pricing_source: "verified-zero-price".into(),
            pricing_verified_at_ms: 1,
            now_ms: 3,
        };
        let call = store.reserve_free_call(&child).unwrap();
        assert_eq!(call.state, PaidCallState::Dispatched);
        let settled = store
            .settle_free_call(
                &child.call_id,
                Some(50),
                "openrouter-response-usage|model=vendor/model:free",
            )
            .unwrap();
        assert_eq!(settled.actual_tokens, Some(50));
        assert_eq!(
            settled.settlement_source.as_deref(),
            Some("openrouter-response-usage|model=vendor/model:free")
        );
        let mut duplicate = child.clone();
        duplicate.call_id = "free-call-2".into();
        assert!(matches!(
            store.reserve_free_call(&duplicate),
            Err(BudgetError::Exhausted(_))
        ));

        let paid_zero = PaidCallRequest {
            parent_reservation_id: "missing".into(),
            call_id: "paid-zero".into(),
            provider_id: "provider-a".into(),
            model_id: "vendor/model:free".into(),
            runtime_id: "aider".into(),
            max_tokens: 100,
            max_cost_micros: 0,
            max_calls: 1,
            pricing_source: "verified-zero-price".into(),
            pricing_verified_at_ms: 1,
            now_ms: 3,
        };
        assert!(matches!(
            store.reserve_paid_call(&paid_zero),
            Err(BudgetError::Invalid(_))
        ));
    }

    #[test]
    fn paid_child_requires_fresh_pricing_and_actual_overrun_stops_parent_scope() {
        let (_dir, store) = setup();
        let parent = store.reserve(&request("parent", "job-a"), 1).unwrap();
        store.mark_dispatched(&parent.reservation_id, 2).unwrap();
        let mut stale = paid_call("parent", "stale", 10, 10, 24 * 60 * 60 * 1000 + 2);
        stale.pricing_verified_at_ms = 1;
        assert!(matches!(
            store.reserve_paid_call(&stale),
            Err(BudgetError::Invalid(_))
        ));

        store
            .reserve_paid_call(&paid_call("parent", "overrun", 100, 100, 3))
            .unwrap();
        assert!(matches!(
            store.settle_paid_call("overrun", Some(101), Some(20), "openrouter-usage"),
            Err(BudgetError::Exhausted(_))
        ));
        assert!(matches!(
            store.reserve_paid_call(&paid_call("parent", "blocked", 1, 1, 4)),
            Err(BudgetError::Exhausted(_))
        ));
    }

    #[test]
    fn concurrent_paid_call_reservations_cannot_overbook_parent_ceiling() {
        let (_dir, store) = setup();
        let parent = store.reserve(&request("parent", "job-a"), 1).unwrap();
        store.mark_dispatched(&parent.reservation_id, 2).unwrap();
        let barrier = Arc::new(Barrier::new(3));
        let workers = ["call-a", "call-b"].map(|call_id| {
            let store = store.clone();
            let barrier = barrier.clone();
            thread::spawn(move || {
                barrier.wait();
                store.reserve_paid_call(&paid_call("parent", call_id, 250, 100, 3))
            })
        });
        barrier.wait();
        let results = workers.map(|worker| worker.join().unwrap());
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(
            results
                .iter()
                .filter(|result| matches!(result, Err(BudgetError::Exhausted(_))))
                .count(),
            1
        );
        assert_eq!(store.paid_calls_for_parent("parent").unwrap().len(), 1);
    }

    #[test]
    fn admission_requires_policy_and_paid_cost_reservation() {
        let (_d, s) = setup();
        let mut r = request("missing-price", "job-a");
        r.max_cost_micros = None;
        assert!(matches!(s.reserve(&r, 1), Err(BudgetError::Invalid(_))));
        let mut r = request("stale-price", "job-a");
        r.pricing_verified_at_ms = Some(0);
        assert!(matches!(
            s.reserve(&r, 24 * 60 * 60 * 1000 + 1),
            Err(BudgetError::Invalid(_))
        ));
        let mut r = request("missing-policy", "job-b");
        assert!(matches!(s.reserve(&r, 1), Err(BudgetError::Invalid(_))));
    }
    #[test]
    fn aggregate_caps_and_unresolved_billing_hold_capacity() {
        let (_d, s) = setup();
        let r = s.reserve(&request("one", "job-a"), 1).unwrap();
        assert!(s.reserve(&request("two", "job-a"), 2).is_ok());
        assert!(matches!(
            s.reserve(&request("three", "job-a"), 3),
            Err(BudgetError::Exhausted(_))
        ));
        s.mark_dispatched(&r.reservation_id, 3).unwrap();
        assert_eq!(
            s.settle(
                &r.reservation_id,
                None,
                None,
                Some(2),
                "provider-usage-api",
                4
            )
            .unwrap()
            .state,
            ReservationState::Unresolved
        );
        assert!(matches!(
            s.reserve(&request("four", "job-a"), 5),
            Err(BudgetError::Exhausted(_))
        ));
        assert_eq!(
            s.reconcile_unresolved("one", "billing-statement-1", 100, 200, 20, 6)
                .unwrap()
                .state,
            ReservationState::Settled
        );
    }
    #[test]
    fn unresolved_usage_keeps_budget_reserved_but_releases_process_concurrency() {
        let (_d, s) = setup();
        let policy = BudgetPolicy {
            max_tokens: 900,
            max_cost_micros: 900,
            max_seconds: 900,
            max_concurrency: 1,
        };
        s.configure("batch", "job-a", &policy, &policy).unwrap();

        let first = s.reserve(&request("unknown-usage", "job-a"), 1).unwrap();
        s.mark_dispatched(&first.reservation_id, 2).unwrap();
        assert_eq!(
            s.settle(
                &first.reservation_id,
                None,
                None,
                Some(3),
                "unknown-provider-usage",
                3,
            )
            .unwrap()
            .state,
            ReservationState::Unresolved
        );

        // A completed process with unknown billing must not occupy the only
        // process slot, while its full envelope remains charged to the caps.
        assert!(s
            .reserve(&request("review-after-unknown", "job-a"), 4)
            .is_ok());
        assert!(matches!(
            s.reserve(&request("over-budget", "job-a"), 5),
            Err(BudgetError::Exhausted(_))
        ));
        let connection = s.connect().unwrap();
        let tx = connection.unchecked_transaction().unwrap();
        let totals = active_totals(&tx, "batch", "batch").unwrap();
        assert_eq!(totals, (800, 800, 600, 1));
    }
    #[test]
    fn only_pre_dispatch_reservations_can_be_released_and_settled_usage_counts() {
        let (_d, s) = setup();
        let r = s.reserve(&request("one", "job-a"), 1).unwrap();
        s.mark_dispatched("one", 2).unwrap();
        assert!(matches!(
            s.release_before_dispatch("one"),
            Err(BudgetError::Conflict(_))
        ));
        s.settle(
            "one",
            Some(100),
            Some(200),
            Some(20),
            "provider-usage-api",
            3,
        )
        .unwrap();
        let next = request("two", "job-a");
        assert_eq!(
            s.reserve(&next, 3).unwrap().state,
            ReservationState::Reserved
        );
        let unused = request("unused", "job-a");
        s.reserve(&unused, 4).unwrap();
        s.release_before_dispatch("unused").unwrap();
    }
    #[test]
    fn overrun_stops_both_scopes_until_limits_and_review_evidence_are_updated() {
        let (_d, s) = setup();
        let r = s.reserve(&request("overrun", "job-a"), 1).unwrap();
        s.mark_dispatched("overrun", 2).unwrap();
        assert!(matches!(
            s.settle(
                "overrun",
                Some(401),
                Some(401),
                Some(20),
                "provider-usage-api",
                3
            ),
            Err(BudgetError::Exhausted(_))
        ));
        assert!(matches!(
            s.reserve(&request("blocked", "job-a"), 4),
            Err(BudgetError::Exhausted(_))
        ));
        let larger = BudgetPolicy {
            max_tokens: 2000,
            max_cost_micros: 2000,
            max_seconds: 2000,
            max_concurrency: 2,
        };
        s.configure("batch", "job-a", &larger, &larger).unwrap();
        s.clear_stop_after_review("batch", "batch", "review-1", 5)
            .unwrap();
        assert!(matches!(
            s.reserve(&request("still-blocked", "job-a"), 6),
            Err(BudgetError::Exhausted(_))
        ));
        s.clear_stop_after_review("job", "job-a", "review-2", 7)
            .unwrap();
        assert!(s
            .reserve(&request("released-after-review", "job-a"), 8)
            .is_ok());
    }
    #[test]
    fn concurrent_reservations_have_single_winner_at_aggregate_cap() {
        let (dir, s) = setup();
        let p = BudgetPolicy {
            max_tokens: 400,
            max_cost_micros: 400,
            max_seconds: 300,
            max_concurrency: 2,
        };
        s.configure("batch", "job-a", &p, &p).unwrap();
        let path = dir.path().join("budgets.sqlite3");
        let barrier = Arc::new(Barrier::new(3));
        let mut threads = Vec::new();
        for id in ["a", "b"] {
            let path = path.clone();
            let barrier = barrier.clone();
            threads.push(thread::spawn(move || {
                let store = BudgetStore::open(path).unwrap();
                barrier.wait();
                store.reserve(&request(id, "job-a"), 1).is_ok()
            }));
        }
        barrier.wait();
        let wins = threads
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .filter(|won| *won)
            .count();
        assert_eq!(wins, 1);
    }

    #[test]
    fn batch_cap_is_shared_across_jobs_and_zero_cost_local_budget_is_valid() {
        let (dir, s) = setup();
        let p = BudgetPolicy {
            max_tokens: 1000,
            max_cost_micros: 0,
            max_seconds: 1000,
            max_concurrency: 2,
        };
        s.configure("local-batch", "job-x", &p, &p).unwrap();
        s.configure("local-batch", "job-y", &p, &p).unwrap();
        let make = |id: &str, job: &str| ReservationRequest {
            reservation_id: id.into(),
            batch_id: "local-batch".into(),
            job_id: job.into(),
            task_id: "local".into(),
            provider_id: "ollama".into(),
            model_id: "qwen".into(),
            runtime_id: "aider".into(),
            max_tokens: 700,
            max_cost_micros: Some(0),
            max_seconds: 300,
            is_paid: false,
            pricing_source: None,
            pricing_verified_at_ms: None,
        };
        assert!(s.reserve(&make("local-x", "job-x"), 1).is_ok());
        assert!(matches!(
            s.reserve(&make("local-y", "job-y"), 2),
            Err(BudgetError::Exhausted(_))
        ));
        let _ = dir;
    }
}
