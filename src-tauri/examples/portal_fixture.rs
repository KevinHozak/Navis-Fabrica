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
    let output = tauri::async_runtime::block_on(navis_fabrica_lib::portal_feature::run_fixture(
        request.root,
        request.worker,
        request.worker_sha256,
        request.prompt,
        request.operation_id,
    ));
    let response = match output {
        Ok(value) => value,
        Err(code) => serde_json::json!({"status":code}),
    };
    let mut stdout = std::io::stdout().lock();
    let _ = serde_json::to_writer(&mut stdout, &response);
    let _ = stdout.write_all(b"\n");
}
