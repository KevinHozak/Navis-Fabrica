//! Synthetic harness only. Not a webview command accepting credentials or consent.
use portal_client::{image::ImageReview, Cancellation, PortalClient};
use serde_json::{json, Value};
use std::{io::Read, time::Duration};
fn run() -> Option<Value> {
    let mut raw = String::new();
    std::io::stdin().take(65537).read_to_string(&mut raw).ok()?;
    if raw.len() > 65536 {
        return None;
    }
    let input: Value = serde_json::from_str(&raw).ok()?;
    let operation = input["operation_id"].as_str()?;
    let mut review = ImageReview::new(
        operation,
        2,
        2,
        &[255, 0, 0, 0, 255, 0, 0, 0, 255, 255, 255, 255],
        "Classify generated shapes",
        "deepseek-small",
        10,
        &[(0, 0, 1, 1)],
    )
    .ok()?;
    let tag = review.fingerprint().to_owned();
    if input["cancel_review"] == true {
        review.cancel();
        return Some(json!({"kind":"not_sent","code":"REVIEW_CANCELLED"}));
    }
    let approved = review.approve(&tag).ok()?;
    let client = PortalClient::new(
        input["port"].as_u64()? as u16,
        input["key"].as_str()?.into(),
    )
    .ok()?;
    let cancel = Cancellation::default();
    if input["pre_cancel"] == true {
        cancel.cancel();
    }
    match client.complete_image(
        approved,
        input["consent"].as_str()?,
        Duration::from_millis(input["deadline_ms"].as_u64().unwrap_or(2000)),
        &cancel,
    ) {
        Ok(result) => Some(result.receipt()),
        Err(error) => Some(error.receipt()),
    }
}
fn main() {
    println!(
        "{}",
        run().unwrap_or_else(|| json!({"kind":"not_sent","code":"INVALID_FIXTURE"}))
    );
}
