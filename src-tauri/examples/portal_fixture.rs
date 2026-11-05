use serde::Deserialize;
use std::io::{Read, Write};
use std::path::PathBuf;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    root: PathBuf,
    worker: PathBuf,
    worker_sha256: String,
    prompt: String,
    operation_id: String,
    #[serde(default)]
    cancel_mode: navis_fabrica_lib::portal_feature::FixtureCancelMode,
}

fn main() {
    let mut bytes = Vec::new();
    if std::io::stdin()
        .take(16_385)
        .read_to_end(&mut bytes)
        .is_err()
        || bytes.len() > 16_384
    {
        println!("{{\"status\":\"denied\"}}");
        return;
    }
    let request: Request = match serde_json::from_slice(&bytes) {
        Ok(value) => value,
        Err(_) => {
            println!("{{\"status\":\"denied\"}}");
            return;
        }
    };
    let output = tauri::async_runtime::block_on(
        navis_fabrica_lib::portal_feature::run_fixture_with_cancel(
            request.root,
            request.worker,
            request.worker_sha256,
            request.prompt,
            request.operation_id,
            request.cancel_mode,
        ),
    );
    let response = match output {
        Ok(value) => value,
        Err(code) => serde_json::json!({"status":code}),
    };
    let mut stdout = std::io::stdout().lock();
    let _ = serde_json::to_writer(&mut stdout, &response);
    let _ = stdout.write_all(b"\n");
}

#[cfg(test)]
mod tests {
    use super::Request;

    #[test]
    fn fixture_request_has_only_fixed_cancel_modes() {
        let base = r#"{"root":"C:\\fixture","worker":"C:\\fixture\\worker.py","worker_sha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","prompt":"synthetic","operation_id":"op-1"}"#;
        let request: Request = serde_json::from_str(base).unwrap();
        assert_eq!(
            request.cancel_mode,
            navis_fabrica_lib::portal_feature::FixtureCancelMode::None
        );
        let controlled = base.trim_end_matches('}').to_owned() + r#", "cancel_mode":"exit"}"#;
        let request: Request = serde_json::from_str(&controlled).unwrap();
        assert_eq!(
            request.cancel_mode,
            navis_fabrica_lib::portal_feature::FixtureCancelMode::Exit
        );
        let arbitrary = base.trim_end_matches('}').to_owned() + r#", "cancel_pid":1234}"#;
        assert!(serde_json::from_str::<Request>(&arbitrary).is_err());
        let unknown_mode = base.trim_end_matches('}').to_owned() + r#", "cancel_mode":"pid"}"#;
        assert!(serde_json::from_str::<Request>(&unknown_mode).is_err());
    }
}
