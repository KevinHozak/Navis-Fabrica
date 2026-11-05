//! Generated-only native lifecycle harness. No production root override in the default SDK.
#[cfg(not(feature = "host-fixtures"))]
fn main() {
    eprintln!("host-fixtures feature required");
    std::process::exit(2)
}

#[cfg(feature = "host-fixtures")]
fn main() {
    use portal_client::lifecycle::{ReviewedPolicy, SharedPortal};
    use portal_client::{Cancellation, Operation};
    use serde_json::json;
    use std::{
        future::Future,
        path::PathBuf,
        sync::Arc,
        task::{Context, Poll, Wake, Waker},
        time::{Duration, Instant},
    };
    struct Notify(std::thread::Thread);
    impl Wake for Notify {
        fn wake(self: Arc<Self>) {
            self.0.unpark()
        }
    }
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 5 {
        std::process::exit(2)
    }
    let root = PathBuf::from(&args[1]);
    let context = SharedPortal::fixture(root.clone(), PathBuf::from(&args[2]), args[3].clone());
    let action = &args[4];
    let cancel = Cancellation::default();
    let timeout = if action == "timeout" {
        Duration::from_millis(1)
    } else {
        Duration::from_secs(25)
    };
    let task = if action == "status" {
        context.status(timeout, &cancel)
    } else {
        context.acquire(timeout, &cancel)
    };
    if action == "cancel" || action == "drop" {
        let until = Instant::now() + Duration::from_secs(10);
        while !root.join("spawned.flag").exists() && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(2))
        }
        if action == "cancel" {
            cancel.cancel()
        } else {
            let now = Instant::now();
            drop(task);
            let elapsed = now.elapsed().as_millis();
            std::thread::sleep(Duration::from_millis(200));
            println!("{}", json!({"dropped":true,"drop_ms":elapsed}));
            return;
        }
    }
    let until = Instant::now() + Duration::from_secs(32);
    let waker = Waker::from(Arc::new(Notify(std::thread::current())));
    let mut context = Context::from_waker(&waker);
    let mut task = Box::pin(task);
    let outcome = loop {
        if let Poll::Ready(value) = task.as_mut().poll(&mut context) {
            break value;
        }
        if Instant::now() > until {
            println!("{}", json!({"code":"HARNESS_TIMEOUT"}));
            std::process::exit(3)
        }
        std::thread::park_timeout(Duration::from_millis(20));
    };
    let connection = match outcome {
        Ok(v) => v,
        Err(e) => {
            println!("{}", e.receipt());
            return;
        }
    };
    if action != "infer" && action != "repeat" && action != "peer-replacement" {
        println!("{}", connection.receipt());
        return;
    }
    if action == "peer-replacement" {
        std::fs::write(root.join("connection-ready.flag"), b"fixture").unwrap();
        let until = Instant::now() + Duration::from_secs(15);
        while !root.join("replacement-ready.flag").exists() {
            if Instant::now() > until {
                std::process::exit(3)
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }
    if args.len() < 7 {
        std::process::exit(2)
    }
    let usd = args.get(7).and_then(|s| s.parse().ok()).unwrap_or(1000);
    let tokens = args.get(8).and_then(|s| s.parse().ok()).unwrap_or(1000);
    let output = args.get(9).and_then(|s| s.parse().ok()).unwrap_or(8);
    let alias = args.get(10).map(String::as_str).unwrap_or("fixture");
    let policy = match ReviewedPolicy::new(alias, &args[6], usd, tokens, output) {
        Ok(v) => v,
        Err(e) => {
            println!("{}", e.receipt());
            return;
        }
    };
    let client = match connection.client("SYNTHETIC_HOST_APP_KEY_0123456789_ABCD".into(), policy) {
        Ok(v) => v,
        Err(e) => {
            println!("{}", e.receipt());
            return;
        }
    };
    let op = Operation::new(&args[5]).unwrap();
    let first = client.complete(
        &op,
        "synthetic lifecycle",
        None,
        "fixture",
        8,
        Duration::from_secs(3),
        &cancel,
    );
    let receipt = match first {
        Ok(v) => v.receipt(),
        Err(e) => e.receipt(),
    };
    if action == "repeat" {
        let second = client.complete(
            &op,
            "synthetic lifecycle",
            None,
            "fixture",
            8,
            Duration::from_secs(3),
            &cancel,
        );
        let second = match second {
            Ok(v) => v.receipt(),
            Err(e) => e.receipt(),
        };
        println!("{}", json!({"first":receipt,"second":second}));
    } else {
        println!("{}", receipt)
    }
}
