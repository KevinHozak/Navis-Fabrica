//! Trusted backend only. Fixed loopback transport, no retries or cloud fallback.
pub mod image;
pub mod lifecycle;
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpStream};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::{Duration, Instant};

pub const CONTRACT: &str = "portal.text/1";
const MAX_RESPONSE: usize = 2_097_152;

#[derive(Clone, Default)]
pub struct Cancellation(Arc<AtomicBool>);
impl Cancellation {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

#[derive(Debug)]
pub struct ClientError {
    pub kind: &'static str,
    pub code: &'static str,
    pub correlation: Option<String>,
}
impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.code)
    }
}
impl std::error::Error for ClientError {}
impl ClientError {
    pub fn receipt(&self) -> Value {
        json!({"kind":self.kind,"code":self.code,"correlation":self.correlation,"cost_usd":null})
    }
}
fn error(kind: &'static str, code: &'static str) -> ClientError {
    ClientError {
        kind,
        code,
        correlation: None,
    }
}

pub struct Operation {
    identity: String,
    used: AtomicBool,
}
impl Operation {
    pub fn new(identity: &str) -> Result<Self, ClientError> {
        if !identifier(identity, 128, true) {
            return Err(error("not_sent", "INVALID_OPERATION"));
        }
        Ok(Self {
            identity: identity.into(),
            used: AtomicBool::new(false),
        })
    }
    fn claim(&self) -> Result<(), ClientError> {
        if self.used.swap(true, Ordering::SeqCst) {
            Err(error("duplicate_unknown", "LOCAL_OPERATION_USED"))
        } else {
            Ok(())
        }
    }
}
fn identifier(value: &str, maximum: usize, colon: bool) -> bool {
    !value.is_empty()
        && value.len() <= maximum
        && value.as_bytes()[0].is_ascii_alphanumeric()
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b) || (colon && b == b':'))
}

// Completion intentionally has no Debug/Serialize implementation: consumers
// decide explicitly which reviewed text to show; receipts omit raw content.
pub struct Completion {
    pub text: String,
    pub model: String,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
    pub cost_usd: String,
    pub correlation: String,
    pub finish_reason: String,
}
impl Completion {
    pub fn receipt(&self) -> Value {
        json!({"kind":"settled","code":"OK","correlation":self.correlation,"cost_usd":self.cost_usd})
    }
}

pub struct PortalClient {
    port: u16,
    key: String,
    managed: Option<lifecycle::ManagedPeer>,
}
impl PortalClient {
    pub fn new(port: u16, key: String) -> Result<Self, ClientError> {
        if port == 0
            || !(32..=256).contains(&key.len())
            || !key.bytes().all(|b| (33..=126).contains(&b))
        {
            return Err(error("not_sent", "INVALID_CONFIGURATION"));
        }
        Ok(Self {
            port,
            key,
            managed: None,
        })
    }

    pub fn complete(
        &self,
        operation: &Operation,
        user: &str,
        system: Option<&str>,
        alias: &str,
        max_tokens: u32,
        timeout: Duration,
        cancel: &Cancellation,
    ) -> Result<Completion, ClientError> {
        if user.chars().count() > 500_000
            || system.is_some_and(|s| s.chars().count() > 500_000)
            || !identifier(alias, 64, false)
            || !(1..=8192).contains(&max_tokens)
            || timeout < Duration::from_millis(1)
            || timeout > Duration::from_secs(180)
        {
            return Err(error("not_sent", "INVALID_REQUEST"));
        }
        let mut messages = vec![];
        if let Some(content) = system {
            messages.push(json!({"role":"system","content":content}));
        }
        messages.push(json!({"role":"user","content":user}));
        let body =
            serde_json::to_vec(&json!({"model":alias,"messages":messages,"max_tokens":max_tokens}))
                .map_err(|_| error("not_sent", "INVALID_REQUEST"))?;
        if body.len() > 1_048_576 {
            return Err(error("not_sent", "INVALID_REQUEST"));
        }
        self.post_body(operation, body, max_tokens, CONTRACT, timeout, cancel)
    }

    fn post_body(
        &self,
        operation: &Operation,
        body: Vec<u8>,
        max_tokens: u32,
        contract: &str,
        timeout: Duration,
        cancel: &Cancellation,
    ) -> Result<Completion, ClientError> {
        if body.len() > 1_048_576
            || !(1..=8192).contains(&max_tokens)
            || ![CONTRACT, "portal.image/1"].contains(&contract)
            || timeout < Duration::from_millis(1)
            || timeout > Duration::from_secs(180)
        {
            return Err(error("not_sent", "INVALID_REQUEST"));
        }
        if let Some(managed) = &self.managed {
            let value: Value =
                serde_json::from_slice(&body).map_err(|_| error("not_sent", "INVALID_REQUEST"))?;
            if value["model"] != managed.policy.alias || max_tokens > managed.policy.output {
                return Err(error("not_sent", "CLIENT_CEILING_EXCEEDED"));
            }
        }
        operation.claim()?;
        let deadline = Instant::now() + timeout;
        check(deadline, cancel, false)?;
        let address = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, self.port));
        // Numeric loopback only: no DNS, URL parser, proxy, TLS fallback or pool.
        let socket = socket2::Socket::new(
            socket2::Domain::IPV4,
            socket2::Type::STREAM,
            Some(socket2::Protocol::TCP),
        )
        .map_err(|_| error("not_sent", "GATEWAY_UNAVAILABLE"))?;
        socket
            .set_nonblocking(true)
            .map_err(|_| error("not_sent", "GATEWAY_UNAVAILABLE"))?;
        match socket.connect(&address.into()) {
            Ok(()) => {}
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || matches!(e.raw_os_error(), Some(10035 | 10036 | 10037 | 115 | 114)) =>
            {
                loop {
                    check(deadline, cancel, false)?;
                    if socket
                        .take_error()
                        .map_err(|_| error("not_sent", "GATEWAY_UNAVAILABLE"))?
                        .is_some()
                    {
                        return Err(error("not_sent", "GATEWAY_UNAVAILABLE"));
                    }
                    if connect_ready(&socket)
                        .map_err(|_| error("not_sent", "GATEWAY_UNAVAILABLE"))?
                    {
                        if socket
                            .take_error()
                            .map_err(|_| error("not_sent", "GATEWAY_UNAVAILABLE"))?
                            .is_some()
                        {
                            return Err(error("not_sent", "GATEWAY_UNAVAILABLE"));
                        }
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(2));
                }
            }
            Err(_) => return Err(error("not_sent", "GATEWAY_UNAVAILABLE")),
        }
        let mut stream: TcpStream = socket.into();
        check(deadline, cancel, false)?;
        if let Some(managed) = &self.managed {
            managed.verify(&stream)?;
        }
        check(deadline, cancel, false)?;
        let extra=self.managed.as_ref().map(|m|format!("Portal-Commitment: {}\r\nPortal-Max-USD-Micros: {}\r\nPortal-Max-Tokens: {}\r\n",m.policy.commitment,m.policy.usd,m.policy.tokens)).unwrap_or_default();
        let mut wire=format!("POST /v1/chat/completions HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nPortal-Contract: {}\r\nAuthorization: Bearer {}\r\nIdempotency-Key: {}\r\nContent-Type: application/json\r\nConnection: close\r\n{}Content-Length: {}\r\n\r\n",self.port,contract,self.key,operation.identity,extra,body.len()).into_bytes();
        wire.extend_from_slice(&body);
        let mut attempted = false;
        let mut offset = 0;
        while offset < wire.len() {
            check(deadline, cancel, attempted)?;
            attempted = true;
            match stream.write(&wire[offset..]) {
                Ok(0) => return Err(error("unknown", "INVALID_RESPONSE")),
                Ok(n) => offset += n,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(2))
                }
                Err(_) => return Err(error("unknown", "INVALID_RESPONSE")),
            }
        }
        let mut bytes = vec![];
        let mut parsed = None;
        loop {
            check(deadline, cancel, true)?;
            let mut buffer = [0_u8; 65536];
            match stream.read(&mut buffer) {
                Ok(0) => return Err(error("unknown", "INVALID_RESPONSE")),
                Ok(n) => bytes.extend_from_slice(&buffer[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(2));
                    continue;
                }
                Err(_) => return Err(error("unknown", "INVALID_RESPONSE")),
            }
            if parsed.is_none() {
                if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                    if end > 16384 {
                        return Err(error("unknown", "INVALID_RESPONSE"));
                    }
                    let (status, headers, length) = parse_headers(&bytes[..end], contract)?;
                    parsed = Some((end + 4, status, headers, length));
                } else if bytes.len() > 16384 {
                    return Err(error("unknown", "INVALID_RESPONSE"));
                }
            }
            if let Some((end, status, ref headers, length)) = parsed {
                if bytes.len() > end + length {
                    return Err(error("unknown", "INVALID_RESPONSE"));
                }
                if bytes.len() == end + length {
                    check(deadline, cancel, true)?;
                    return decode(&bytes[end..], status, headers, max_tokens);
                }
            }
        }
    }
}

fn check(deadline: Instant, cancel: &Cancellation, attempted: bool) -> Result<(), ClientError> {
    let kind = if attempted { "unknown" } else { "not_sent" };
    if cancel.is_cancelled() {
        return Err(error(kind, "CLIENT_CANCELLED"));
    }
    if Instant::now() >= deadline {
        return Err(error(kind, "DEADLINE_EXCEEDED"));
    }
    Ok(())
}

#[cfg(windows)]
fn connect_ready(socket: &socket2::Socket) -> std::io::Result<bool> {
    use std::os::windows::io::AsRawSocket;
    use windows_sys::Win32::Networking::WinSock::{
        WSAPoll, POLLERR, POLLHUP, POLLNVAL, POLLWRNORM, WSAPOLLFD,
    };
    let mut fd = WSAPOLLFD {
        fd: socket.as_raw_socket() as usize,
        events: POLLWRNORM,
        revents: 0,
    };
    let ready = unsafe { WSAPoll(&mut fd, 1, 0) };
    if ready < 0 || fd.revents & (POLLERR | POLLHUP | POLLNVAL) != 0 {
        return Err(std::io::Error::other("connect failed"));
    }
    Ok(ready > 0 && fd.revents & POLLWRNORM != 0)
}
#[cfg(unix)]
fn connect_ready(socket: &socket2::Socket) -> std::io::Result<bool> {
    use std::os::fd::AsRawFd;
    let mut fd = libc::pollfd {
        fd: socket.as_raw_fd(),
        events: libc::POLLOUT,
        revents: 0,
    };
    let ready = unsafe { libc::poll(&mut fd, 1, 0) };
    if ready < 0 || fd.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
        return Err(std::io::Error::other("connect failed"));
    }
    Ok(ready > 0 && fd.revents & libc::POLLOUT != 0)
}

type Headers = std::collections::BTreeMap<String, String>;
fn parse_headers(raw: &[u8], contract: &str) -> Result<(u16, Headers, usize), ClientError> {
    let bad = || error("unknown", "INVALID_RESPONSE");
    if !raw.is_ascii() {
        return Err(bad());
    }
    let text = std::str::from_utf8(raw).map_err(|_| bad())?;
    let lines: Vec<_> = text.split("\r\n").collect();
    if lines.len() > 65 || lines.iter().any(|l| l.len() > 4096) {
        return Err(bad());
    }
    let status = lines[0].splitn(3, ' ').collect::<Vec<_>>();
    if status.len() != 3
        || status[0] != "HTTP/1.1"
        || status[1].len() != 3
        || !status[1].bytes().all(|b| b.is_ascii_digit())
        || !status[2].bytes().all(|b| (32..=126).contains(&b))
    {
        return Err(bad());
    }
    let status: u16 = status[1].parse().map_err(|_| bad())?;
    if !(100..600).contains(&status) {
        return Err(bad());
    }
    let mut headers = Headers::new();
    for line in &lines[1..] {
        let (name, value) = line.split_once(':').ok_or_else(bad)?;
        if name.is_empty()
            || !name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+.^_`|~-".contains(&b))
            || !value.bytes().all(|b| (32..=126).contains(&b))
        {
            return Err(bad());
        }
        if headers
            .insert(name.to_ascii_lowercase(), value.trim().into())
            .is_some()
        {
            return Err(bad());
        }
    }
    let length = headers.get("content-length").ok_or_else(bad)?;
    if length.is_empty()
        || length.len() > 10
        || !length.bytes().all(|b| b.is_ascii_digit())
        || (length.len() > 1 && length.starts_with('0'))
    {
        return Err(bad());
    }
    let length: usize = length.parse().map_err(|_| bad())?;
    if length > MAX_RESPONSE
        || headers.contains_key("transfer-encoding")
        || headers.contains_key("content-encoding")
        || headers.get("portal-contract").map(String::as_str) != Some(contract)
        || headers
            .get("content-type")
            .map(|s| s.split(';').next().unwrap_or(""))
            != Some("application/json")
    {
        return Err(bad());
    }
    Ok((status, headers, length))
}

// Check raw object keys before Value parsing; reserved serde number tokens
// cannot masquerade as numeric costs. serde_json validates the full grammar.
fn unique_json(raw: &[u8]) -> bool {
    fn ws(raw: &[u8], p: &mut usize) {
        while *p < raw.len() && raw[*p].is_ascii_whitespace() {
            *p += 1;
        }
    }
    fn string(raw: &[u8], p: &mut usize) -> Option<String> {
        if raw.get(*p) != Some(&b'"') {
            return None;
        }
        let start = *p;
        *p += 1;
        while *p < raw.len() {
            if raw[*p] == b'\\' {
                *p += 2;
                continue;
            }
            if raw[*p] == b'"' {
                *p += 1;
                return serde_json::from_slice(&raw[start..*p]).ok();
            }
            *p += 1;
        }
        None
    }
    fn value(raw: &[u8], p: &mut usize, depth: usize) -> Option<()> {
        if depth > 100 {
            return None;
        }
        ws(raw, p);
        match *raw.get(*p)? {
            b'{' => {
                *p += 1;
                ws(raw, p);
                let mut keys = std::collections::BTreeSet::new();
                if raw.get(*p) == Some(&b'}') {
                    *p += 1;
                    return Some(());
                }
                loop {
                    ws(raw, p);
                    let key = string(raw, p)?;
                    if key == "$serde_json::private::Number" || !keys.insert(key) {
                        return None;
                    }
                    ws(raw, p);
                    if raw.get(*p) != Some(&b':') {
                        return None;
                    }
                    *p += 1;
                    value(raw, p, depth + 1)?;
                    ws(raw, p);
                    match raw.get(*p)? {
                        b'}' => {
                            *p += 1;
                            break;
                        }
                        b',' => *p += 1,
                        _ => return None,
                    }
                }
            }
            b'[' => {
                *p += 1;
                ws(raw, p);
                if raw.get(*p) == Some(&b']') {
                    *p += 1;
                    return Some(());
                }
                loop {
                    value(raw, p, depth + 1)?;
                    ws(raw, p);
                    match raw.get(*p)? {
                        b']' => {
                            *p += 1;
                            break;
                        }
                        b',' => *p += 1,
                        _ => return None,
                    }
                }
            }
            b'"' => {
                string(raw, p)?;
            }
            _ => {
                let start = *p;
                while *p < raw.len() && !b",]} \r\n\t".contains(&raw[*p]) {
                    *p += 1;
                }
                if *p == start {
                    return None;
                }
            }
        }
        Some(())
    }
    let mut p = 0;
    if value(raw, &mut p, 0).is_none() {
        return false;
    }
    ws(raw, &mut p);
    p == raw.len()
}

fn decode(
    raw: &[u8],
    status: u16,
    headers: &Headers,
    max_tokens: u32,
) -> Result<Completion, ClientError> {
    let bad = || error("unknown", "INVALID_RESPONSE");
    if !unique_json(raw) {
        return Err(bad());
    }
    let value: Value = serde_json::from_slice(raw).map_err(|_| bad())?;
    let correlation = headers.get("x-portal-request-id").ok_or_else(bad)?;
    if correlation.len() != 32
        || !correlation
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(bad());
    }
    if status != 200 {
        let e = &value["error"];
        if e["request_id"].as_str() != Some(correlation) {
            return Err(bad());
        }
        let code = e["code"].as_str().ok_or_else(bad)?;
        let kind = match e["billing_state"].as_str() {
            Some("not_admitted") => "not_admitted",
            Some("unresolved") => "unresolved",
            Some("duplicate_unknown") => "duplicate_unknown",
            _ => return Err(bad()),
        };
        return Err(ClientError {
            kind: if code == "DUPLICATE_IDEMPOTENCY_KEY" {
                "duplicate_unknown"
            } else {
                kind
            },
            code: known_error(code),
            correlation: Some(correlation.clone()),
        });
    }
    let usage = &value["usage"];
    let choices = value["choices"].as_array().ok_or_else(bad)?;
    if choices.len() != 1 {
        return Err(bad());
    }
    let choice = &choices[0];
    let message = &choice["message"];
    let prompt = usage["prompt_tokens"].as_u64().ok_or_else(bad)?;
    let output = usage["completion_tokens"].as_u64().ok_or_else(bad)?;
    let total = usage["total_tokens"].as_u64().ok_or_else(bad)?;
    let finish = choice["finish_reason"].as_str().ok_or_else(bad)?;
    let model = value["model"].as_str().ok_or_else(bad)?;
    if prompt < 1
        || prompt > i64::MAX as u64
        || total > i64::MAX as u64
        || output > max_tokens as u64
        || prompt.checked_add(output) != Some(total)
        || !["stop", "length"].contains(&finish)
        || message["role"].as_str() != Some("assistant")
        || model.is_empty()
        || model.len() > 256
        || !model.bytes().all(|b| (32..=126).contains(&b))
    {
        return Err(bad());
    }
    let cost = money(&usage["cost"]).ok_or_else(bad)?;
    Ok(Completion {
        text: message["content"].as_str().ok_or_else(bad)?.into(),
        model: model.into(),
        prompt_tokens: prompt,
        completion_tokens: output,
        total_tokens: total,
        cost_usd: cost,
        correlation: correlation.clone(),
        finish_reason: finish.into(),
    })
}

fn known_error(code: &str) -> &'static str {
    const CODES: &[&str] = &[
        "AUTH_INVALID",
        "AUTH_REQUIRED",
        "ROUTE_NOT_ALLOWED",
        "LIVE_DISPATCH_DISABLED",
        "UPSTREAM_BUSY",
        "HTTP_BUSY",
        "SERVICE_STOPPING",
        "OPERATOR_PAUSED",
        "OPERATOR_REVOKED",
        "EMERGENCY_STOP",
        "POLICY_APPROVAL_REQUIRED",
        "ROUTE_EVIDENCE_STALE",
        "ROUTE_METADATA_STALE",
        "ROUTE_METADATA_UNAVAILABLE",
        "USD_BUDGET_EXCEEDED",
        "CALL_BUDGET_EXCEEDED",
        "TOKEN_BUDGET_EXCEEDED",
        "DUPLICATE_IDEMPOTENCY_KEY",
        "UPSTREAM_TIMEOUT",
        "UPSTREAM_ERROR",
        "INVALID_RESPONSE",
        "MISSING_USAGE",
        "MODEL_IDENTITY_MISMATCH",
        "BILLING_OVER_RESERVATION",
        "INVALID_JSON",
        "UNSUPPORTED_REQUEST_FIELD",
        "INPUT_LIMIT_EXCEEDED",
        "OUTPUT_LIMIT_EXCEEDED",
        "CONTRACT_VERSION_UNSUPPORTED",
        "IMAGE_CAPABILITY_UNQUALIFIED",
        "IMAGE_LIMIT_EXCEEDED",
        "IMAGE_REQUEST_UNSUPPORTED",
        "EXTERNAL_MEDIA_ACTION_DENIED",
        "EXTERNAL_MEDIA_CONSENT_REQUIRED",
        "EXTERNAL_MEDIA_CONSENT_INVALID",
        "IMAGE_BILLING_UNKNOWN",
        "ROUTE_DATA_CLASS_DENIED",
        "INTERNAL_ERROR",
    ];
    CODES
        .iter()
        .copied()
        .find(|v| *v == code)
        .unwrap_or("UNKNOWN_GATEWAY_ERROR")
}

fn money(value: &Value) -> Option<String> {
    let text = match value {
        Value::String(v) => v.clone(),
        Value::Number(v) => v.to_string(),
        _ => return None,
    };
    if text.is_empty() || text.len() > 64 {
        return None;
    }
    let parts: Vec<_> = text.split(['e', 'E']).collect();
    if parts.len() > 2 {
        return None;
    }
    let exponent: i32 = if parts.len() == 2 {
        parts[1].parse().ok()?
    } else {
        0
    };
    if !(-24..=24).contains(&exponent) {
        return None;
    }
    let mantissa: Vec<_> = parts[0].split('.').collect();
    if mantissa.len() > 2
        || mantissa
            .iter()
            .any(|s| s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()))
    {
        return None;
    }
    let fractional = mantissa.get(1).map_or(0, |s| s.len()) as i32;
    let power = exponent - fractional;
    if power < -24 {
        return None;
    }
    let digits = mantissa.concat();
    let digits = digits.trim_start_matches('0');
    if digits.is_empty() {
        return Some("0".into());
    }
    let magnitude = digits.len() as i32 + power;
    if magnitude > 10
        || (magnitude == 10
            && (digits.as_bytes()[0] != b'1' || digits.bytes().skip(1).any(|b| b != b'0')))
    {
        return None;
    }
    let mut plain = if power >= 0 {
        format!("{}{}", digits, "0".repeat(power as usize))
    } else if magnitude > 0 {
        let pos = magnitude as usize;
        format!("{}.{}", &digits[..pos], &digits[pos..])
    } else {
        format!("0.{}{}", "0".repeat((-magnitude) as usize), digits)
    };
    if plain.contains('.') {
        plain = plain.trim_end_matches('0').trim_end_matches('.').into();
    }
    Some(plain)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exact_money_and_unknown_are_distinct() {
        assert_eq!(money(&json!(0)), Some("0".into()));
        assert_eq!(
            money(&json!("1e-24")),
            Some("0.000000000000000000000001".into())
        );
        assert_eq!(money(&json!("1000000000")), Some("1000000000".into()));
        for value in [
            Value::Null,
            json!(true),
            json!("NaN"),
            json!("-1"),
            json!("1e-2147483648"),
            json!("1e-25"),
            json!("1000000000.0001"),
        ] {
            assert_eq!(money(&value), None);
        }
    }
    #[test]
    fn duplicate_keys_and_numeric_object_masquerades_reject() {
        assert!(unique_json(br#"{"usage":{"cost":0.000005}}"#));
        assert!(!unique_json(br#"{"usage":{"cost":"0","cost":"1"}}"#));
        assert!(!unique_json(
            br#"{"usage":{"cost":{"$serde_json::private::Number":"0"}}}"#
        ));
    }
    #[test]
    fn operation_is_claimed_at_most_once() {
        let operation = Operation::new("opaque-1").unwrap();
        assert!(operation.claim().is_ok());
        assert_eq!(operation.claim().unwrap_err().code, "LOCAL_OPERATION_USED");
        assert!(Operation::new("header\r\ninjection").is_err());
    }
}
