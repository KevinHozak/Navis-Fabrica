//! Compiled trusted-native bridge example, driven only by the offline harness.
//! This is NOT a webview command accepting keys. Production integrations must
//! load the app key in native state and gate selected context before calling.
use portal_client::{Cancellation, Operation, PortalClient};
use serde_json::{json, Value};
use std::io::Read;
use std::time::Duration;

struct Backend {
    portal: PortalClient,
}
impl Backend {
    fn selected_text(
        &self,
        operation: &Operation,
        text: &str,
        alias: &str,
        deadline: Duration,
        cancel: &Cancellation,
    ) -> Value {
        match self
            .portal
            .complete(operation, text, None, alias, 8, deadline, cancel)
        {
            Ok(completion) => completion.receipt(),
            Err(error) => error.receipt(),
        }
    }
}

fn run() -> Option<Value> {
    let mut raw = String::new();
    std::io::stdin().take(65537).read_to_string(&mut raw).ok()?;
    if raw.len() > 65536 {
        return None;
    }
    let input: Value = serde_json::from_str(&raw).ok()?;
    let port = u16::try_from(input["port"].as_u64()?).ok()?;
    let key = input["key"].as_str()?.to_string();
    let client = match PortalClient::new(port, key) {
        Ok(v) => v,
        Err(e) => return Some(e.receipt()),
    };
    let operation = match Operation::new(input["operation_id"].as_str()?) {
        Ok(v) => v,
        Err(e) => return Some(e.receipt()),
    };
    let backend = Backend { portal: client };
    let cancel = Cancellation::default();
    if input["pre_cancel"].as_bool() == Some(true) {
        cancel.cancel();
    }
    let timer = input["cancel_after_ms"].as_u64().map(|delay| {
        let cancel = cancel.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(delay));
            cancel.cancel();
        })
    });
    let deadline = Duration::from_millis(input["deadline_ms"].as_u64()?);
    let text = input["user"].as_str().unwrap_or("synthetic");
    let alias = input["alias"].as_str().unwrap_or("text");
    let mut result = backend.selected_text(&operation, text, alias, deadline, &cancel);
    if input["repeat_local"].as_bool() == Some(true) {
        result = backend.selected_text(&operation, text, alias, deadline, &cancel);
    }
    if let Some(timer) = timer {
        let _ = timer.join();
    }
    Some(result)
}
fn main() {
    let receipt=run().unwrap_or_else(||json!({"kind":"not_sent","code":"INVALID_CONFIGURATION","correlation":null,"cost_usd":null}));
    // No raw prompts, completion text, keys, URLs, IO errors or response bodies.
    println!("{}", receipt);
}
