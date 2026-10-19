//! Host-side validation for declared, deterministic acceptance contracts.
//!
//! A contract harness is a trusted executable protocol. This module checks that
//! its bounded report contains exact observations for the declared specimens;
//! it does not sandbox a malicious harness.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;

pub const MAX_CONTRACT_SPECIMENS: usize = 128;
pub const MAX_CONTRACT_CRITERIA: usize = 64;
pub const MAX_CONTRACT_REPORT_BYTES: usize = 256 * 1024;
pub const MAX_CONTRACT_EVIDENCE_BYTES: usize = 192 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ContractSpec {
    /// Exact acceptance criterion strings owned by this contract.
    pub criteria: Vec<String>,
    pub specimens: Vec<ContractSpecimen>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ContractSpecimen {
    pub id: String,
    pub input: Value,
    pub expected: Value,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ContractReport {
    pub schema_version: u32,
    pub observations: Vec<ContractObservation>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ContractObservation {
    pub specimen_id: String,
    pub input: Value,
    pub assertion: String,
    pub expected: Value,
    pub actual: Value,
    pub executed: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ContractStatus {
    Passed,
    Failed,
    Inconclusive,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ContractEvidence {
    pub status: ContractStatus,
    pub reason: Option<String>,
    pub spec: ContractSpec,
    pub observations: Vec<ContractObservation>,
}

/// Validate the static, explicitly declared part of a contract.
pub fn validate_spec(spec: &ContractSpec, acceptance_criteria: &[String]) -> Result<(), String> {
    if spec.criteria.is_empty()
        || spec.criteria.len() > MAX_CONTRACT_CRITERIA
        || spec.specimens.is_empty()
        || spec.specimens.len() > MAX_CONTRACT_SPECIMENS
    {
        return Err("contract criteria and specimens must be nonempty and bounded".into());
    }
    if serde_json::to_vec(spec).map_err(|e| e.to_string())?.len() > 128 * 1024 {
        return Err("contract declaration exceeds 128 KiB".into());
    }
    let available: HashSet<&str> = acceptance_criteria.iter().map(String::as_str).collect();
    let mut criteria = HashSet::new();
    for criterion in &spec.criteria {
        if criterion.trim().is_empty()
            || criterion.len() > 2_048
            || criterion.chars().any(char::is_control)
            || !available.contains(criterion.as_str())
            || !criteria.insert(criterion.as_str())
        {
            return Err(
                "contract criteria must be unique exact members of the acceptance criteria".into(),
            );
        }
    }
    let mut ids = HashSet::new();
    for specimen in &spec.specimens {
        if specimen.id.trim().is_empty()
            || specimen.id.len() > 128
            || specimen.id.chars().any(char::is_control)
            || !ids.insert(specimen.id.as_str())
        {
            return Err("contract specimen IDs must be nonempty and unique".into());
        }
    }
    Ok(())
}

/// Classify only complete, exact, executed observations. Any protocol weakness
/// makes the result inconclusive; an exact completed comparison mismatch fails.
pub fn evaluate(spec: &ContractSpec, stdout: &str, command_completed: bool) -> ContractEvidence {
    let inconclusive = |reason: &str, observations: Vec<ContractObservation>| ContractEvidence {
        status: ContractStatus::Inconclusive,
        reason: Some(reason.into()),
        spec: spec.clone(),
        observations,
    };
    if !command_completed || stdout.len() > MAX_CONTRACT_REPORT_BYTES {
        return inconclusive(
            "contract command did not complete with bounded output",
            vec![],
        );
    }
    let report: ContractReport = match serde_json::from_str::<ContractReport>(stdout) {
        Ok(report) if report.schema_version == 1 => report,
        _ => {
            return inconclusive(
                "contract report is malformed or has an unsupported schema",
                vec![],
            )
        }
    };
    // Leave room for the declaration, receipt and source metadata in the 256 KiB checkpoint.
    let evidence_bytes = serde_json::to_vec(&report.observations)
        .map(|v| v.len())
        .unwrap_or(usize::MAX)
        .saturating_add(
            serde_json::to_vec(spec)
                .map(|v| v.len())
                .unwrap_or(usize::MAX),
        );
    if evidence_bytes > MAX_CONTRACT_EVIDENCE_BYTES {
        return inconclusive(
            "contract evidence exceeds the checkpoint payload budget",
            vec![],
        );
    }
    if report.observations.len() != spec.specimens.len() {
        return inconclusive(
            "contract report does not cover every declared specimen exactly once",
            report.observations,
        );
    }

    let mut seen = HashSet::new();
    let mut mismatch = false;
    for observation in &report.observations {
        if !seen.insert(observation.specimen_id.as_str()) {
            return inconclusive(
                "contract report contains a duplicate specimen ID",
                report.observations,
            );
        }
        let Some(specimen) = spec
            .specimens
            .iter()
            .find(|s| s.id == observation.specimen_id)
        else {
            return inconclusive(
                "contract report contains an unknown specimen ID",
                report.observations,
            );
        };
        if !observation.executed
            || observation.assertion != "deep_equal"
            || observation.input != specimen.input
            || observation.expected != specimen.expected
        {
            return inconclusive(
                "contract observation does not prove the declared exact executed comparison",
                report.observations,
            );
        }
        mismatch |= observation.actual != specimen.expected;
    }
    if seen.len() != spec.specimens.len() {
        return inconclusive(
            "contract report omits a declared specimen",
            report.observations,
        );
    }
    ContractEvidence {
        status: if mismatch {
            ContractStatus::Failed
        } else {
            ContractStatus::Passed
        },
        reason: mismatch.then(|| "one or more declared specimen comparisons failed".into()),
        spec: spec.clone(),
        observations: report.observations,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn spec() -> ContractSpec {
        ContractSpec {
            criteria: vec!["returns normalized value".into()],
            specimens: vec![ContractSpecimen {
                id: "basic".into(),
                input: json!({"x": 1}),
                expected: json!({"y": 2}),
            }],
        }
    }

    fn report(actual: Value, assertion: &str) -> String {
        serde_json::to_string(&json!({
            "schema_version": 1,
            "observations": [{
                "specimen_id": "basic",
                "input": {"x": 1},
                "assertion": assertion,
                "expected": {"y": 2},
                "actual": actual,
                "executed": true
            }]
        }))
        .unwrap()
    }

    #[test]
    fn exact_complete_report_passes_and_completed_mismatch_fails() {
        assert_eq!(
            evaluate(&spec(), &report(json!({"y": 2}), "deep_equal"), true).status,
            ContractStatus::Passed
        );
        assert_eq!(
            evaluate(&spec(), &report(json!({"y": 3}), "deep_equal"), true).status,
            ContractStatus::Failed
        );
    }

    #[test]
    fn weak_malformed_missing_duplicate_and_unknown_evidence_is_inconclusive() {
        for (raw, completed) in [
            (report(json!({"y": 2}), "exists"), true),
            ("{}".into(), true),
            (
                report(json!({"y": 2}), "deep_equal")
                    .replace("\"observations\":[{", "\"observations\":[{"),
                false,
            ),
        ] {
            assert_eq!(
                evaluate(&spec(), &raw, completed).status,
                ContractStatus::Inconclusive
            );
        }
        let duplicate = format!(
            "{{\"schema_version\":1,\"observations\":{},\"unused\":0}}",
            "[]"
        );
        assert_eq!(
            evaluate(&spec(), &duplicate, true).status,
            ContractStatus::Inconclusive
        );
    }

    #[test]
    fn criteria_are_exact_and_ids_unique() {
        let mut invalid = spec();
        invalid.criteria[0] = "returns".into();
        assert!(validate_spec(&invalid, &["returns normalized value".into()]).is_err());
        invalid = spec();
        invalid.specimens.push(invalid.specimens[0].clone());
        assert!(validate_spec(&invalid, &["returns normalized value".into()]).is_err());
    }
}

/// Receipts may be deserialized; a status flag alone is not proof of coverage.
pub fn validate_evidence(evidence: &ContractEvidence, criteria: &[String]) -> Result<(), String> {
    validate_spec(&evidence.spec, criteria)?;
    let report = ContractReport {
        schema_version: 1,
        observations: evidence.observations.clone(),
    };
    let raw = serde_json::to_string(&report).map_err(|e| e.to_string())?;
    let observed = evaluate(&evidence.spec, &raw, true);
    if observed.status != evidence.status || observed.status != ContractStatus::Passed {
        return Err("contract receipt does not establish a complete passing contract".into());
    }
    Ok(())
}

#[cfg(test)]
mod protocol_regressions {
    use super::*;
    use serde_json::json;
    fn spec() -> ContractSpec {
        ContractSpec {
            criteria: vec!["finite behavior".into()],
            specimens: vec![
                ContractSpecimen {
                    id: "zero".into(),
                    input: json!(0),
                    expected: json!(0),
                },
                ContractSpecimen {
                    id: "one".into(),
                    input: json!(1),
                    expected: json!(2),
                },
            ],
        }
    }
    fn report() -> Value {
        json!({"schema_version":1,"observations":[
            {"specimen_id":"zero","input":0,"assertion":"deep_equal","expected":0,"actual":0,"executed":true},
            {"specimen_id":"one","input":1,"assertion":"deep_equal","expected":2,"actual":2,"executed":true}
        ]})
    }
    #[test]
    fn incomplete_or_unreliable_evidence_never_passes() {
        let mut variants = vec![json!({"schema_version":1,"observations":[]})];
        for key in ["input", "expected", "executed", "assertion", "specimen_id"] {
            let mut r = report();
            r["observations"][0][key] = match key {
                "executed" => json!(false),
                "assertion" => json!("type"),
                "specimen_id" => json!("unknown"),
                _ => json!(99),
            };
            variants.push(r);
        }
        let mut duplicate = report();
        duplicate["observations"][1] = duplicate["observations"][0].clone();
        variants.push(duplicate);
        let mut unsupported = report();
        unsupported["schema_version"] = json!(2);
        variants.push(unsupported);
        let mut extra = report();
        extra["observations"][0]["passed"] = json!(true);
        variants.push(extra);
        for r in variants {
            assert_eq!(
                evaluate(&spec(), &r.to_string(), true).status,
                ContractStatus::Inconclusive
            );
        }
        assert_eq!(
            evaluate(&spec(), &report().to_string(), false).status,
            ContractStatus::Inconclusive
        );
        assert_eq!(
            evaluate(&spec(), "not json", true).status,
            ContractStatus::Inconclusive
        );
    }
    #[test]
    fn mismatch_requires_a_complete_trustworthy_report() {
        let mut r = report();
        r["observations"][0]["actual"] = json!(1);
        assert_eq!(
            evaluate(&spec(), &r.to_string(), true).status,
            ContractStatus::Failed
        );
        r["observations"].as_array_mut().unwrap().pop();
        assert_eq!(
            evaluate(&spec(), &r.to_string(), true).status,
            ContractStatus::Inconclusive
        );
    }
    #[test]
    fn forged_passed_flag_cannot_override_observations() {
        let mut e = evaluate(&spec(), &report().to_string(), true);
        assert!(validate_evidence(&e, &spec().criteria).is_ok());
        e.observations[0].actual = json!(123);
        assert!(validate_evidence(&e, &spec().criteria).is_err());
    }
    #[test]
    fn oversized_evidence_is_inconclusive_and_fits_checkpoint_budget() {
        let mut r = report();
        r["observations"][0]["actual"] = json!("x".repeat(200 * 1024));
        let evidence = evaluate(&spec(), &r.to_string(), true);
        assert_eq!(evidence.status, ContractStatus::Inconclusive);
        assert!(evidence.observations.is_empty());
        assert!(serde_json::to_vec(&evidence).unwrap().len() < crate::jobs::MAX_CHECKPOINT_BYTES);
    }
    #[test]
    fn declared_scope_and_payload_are_bounded() {
        let mut s = spec();
        s.criteria.push(s.criteria[0].clone());
        assert!(validate_spec(&s, &s.criteria).is_err());
        let mut s = spec();
        s.specimens[0].input = json!("x".repeat(128 * 1024));
        assert!(validate_spec(&s, &s.criteria).is_err());
        assert!(validate_spec(&spec(), &["a different requirement".into()]).is_err());
    }
}
