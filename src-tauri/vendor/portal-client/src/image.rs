//! Native-owned review values. No filesystem, URL reader or consent-mint API.
use crate::{error, identifier, Cancellation, ClientError, Completion, Operation, PortalClient};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::time::Duration;

pub struct ImageReview {
    operation: String,
    payload: Option<Value>,
    fingerprint: String,
    png: Vec<u8>,
    text: String,
}
pub struct ApprovedImage {
    operation: String,
    payload: Value,
}
impl ImageReview {
    pub fn new(
        operation: &str,
        width: u32,
        height: u32,
        pixels: &[u8],
        text: &str,
        alias: &str,
        max_tokens: u32,
        redactions: &[(u32, u32, u32, u32)],
    ) -> Result<Self, ClientError> {
        Operation::new(operation)?;
        let bad = || error("not_sent", "IMAGE_PREPARATION_INVALID");
        if !(1..=128).contains(&width)
            || !(1..=128).contains(&height)
            || pixels.len() != (width * height * 3) as usize
            || text.len() > 1024
            || !identifier(alias, 64, false)
            || !(1..=8192).contains(&max_tokens)
            || redactions.len() > 16
        {
            return Err(bad());
        }
        let mut pixels = pixels.to_vec();
        for &(x, y, w, h) in redactions {
            if w == 0
                || h == 0
                || x.checked_add(w).is_none_or(|n| n > width)
                || y.checked_add(h).is_none_or(|n| n > height)
            {
                return Err(bad());
            }
            for row in y..y + h {
                let start = ((row * width + x) * 3) as usize;
                pixels[start..start + (w * 3) as usize].fill(0);
            }
        }
        let png = png(width, height, &pixels);
        if png.len() > 65536 {
            return Err(bad());
        }
        let payload = json!({"model":alias,"messages":[{"role":"user","content":text}],"max_tokens":max_tokens,
            "image":{"mime_type":"image/png","data":base64(&png)},"action":"classify-image"});
        let fingerprint = format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(&json!([operation, &payload])).unwrap())
        );
        Ok(Self {
            operation: operation.into(),
            payload: Some(payload),
            fingerprint,
            png,
            text: text.into(),
        })
    }
    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }
    pub fn png(&self) -> &[u8] {
        &self.png
    }
    pub fn text(&self) -> &str {
        &self.text
    }
    pub fn approve(&mut self, displayed_fingerprint: &str) -> Result<ApprovedImage, ClientError> {
        let payload = self
            .payload
            .take()
            .ok_or_else(|| error("not_sent", "IMAGE_REVIEW_REQUIRED"))?;
        self.png.fill(0);
        self.text.clear();
        if displayed_fingerprint != self.fingerprint {
            return Err(error("not_sent", "IMAGE_REVIEW_REQUIRED"));
        }
        Ok(ApprovedImage {
            operation: self.operation.clone(),
            payload,
        })
    }
    pub fn cancel(&mut self) {
        self.payload = None;
        self.png.fill(0);
        self.text.clear();
    }
}
impl PortalClient {
    pub fn complete_image(
        &self,
        mut approved: ApprovedImage,
        consent: &str,
        timeout: Duration,
        cancel: &Cancellation,
    ) -> Result<Completion, ClientError> {
        if consent.len() != 64
            || !consent
                .bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
        {
            return Err(error("not_sent", "EXTERNAL_MEDIA_CONSENT_REQUIRED"));
        }
        approved.payload["consent"] = consent.into();
        let max_tokens = approved.payload["max_tokens"].as_u64().unwrap() as u32;
        let operation = Operation::new(&approved.operation)?;
        self.post_body(
            &operation,
            serde_json::to_vec(&approved.payload)
                .map_err(|_| error("not_sent", "INVALID_REQUEST"))?,
            max_tokens,
            "portal.image/1",
            timeout,
            cancel,
        )
    }
}

fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for c in bytes.chunks(3) {
        let n = ((c[0] as u32) << 16)
            | ((c.get(1).copied().unwrap_or(0) as u32) << 8)
            | c.get(2).copied().unwrap_or(0) as u32;
        out.push(TABLE[((n >> 18) & 63) as usize] as char);
        out.push(TABLE[((n >> 12) & 63) as usize] as char);
        out.push(if c.len() > 1 {
            TABLE[((n >> 6) & 63) as usize] as char
        } else {
            '='
        });
        out.push(if c.len() > 2 {
            TABLE[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}
fn chunk(out: &mut Vec<u8>, kind: &[u8], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let mut crc = !0u32;
    for b in kind.iter().chain(data) {
        crc ^= *b as u32;
        for _ in 0..8 {
            crc = (crc >> 1) ^ if crc & 1 != 0 { 0xedb88320 } else { 0 };
        }
    }
    out.extend_from_slice(&(!crc).to_be_bytes());
}
fn png(width: u32, height: u32, pixels: &[u8]) -> Vec<u8> {
    let mut rows = Vec::new();
    for row in pixels.chunks((width * 3) as usize) {
        rows.push(0);
        rows.extend_from_slice(row);
    }
    let mut zipped = vec![0x78, 0x01, 0x01];
    let n = rows.len() as u16;
    zipped.extend_from_slice(&n.to_le_bytes());
    zipped.extend_from_slice(&(!n).to_le_bytes());
    zipped.extend_from_slice(&rows);
    let (mut a, mut b) = (1u32, 0u32);
    for c in &rows {
        a = (a + *c as u32) % 65521;
        b = (b + a) % 65521;
    }
    zipped.extend_from_slice(&((b << 16) | a).to_be_bytes());
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut header = width.to_be_bytes().to_vec();
    header.extend_from_slice(&height.to_be_bytes());
    header.extend_from_slice(&[8, 2, 0, 0, 0]);
    chunk(&mut out, b"IHDR", &header);
    chunk(&mut out, b"IDAT", &zipped);
    chunk(&mut out, b"IEND", &[]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bounds_and_one_use_review() {
        assert!(ImageReview::new("op", 129, 1, &[], "x", "images", 8, &[]).is_err());
        assert!(ImageReview::new(
            "op",
            1,
            1,
            &[1, 2, 3],
            "x",
            "images",
            8,
            &[(0, 0, u32::MAX, 1)]
        )
        .is_err());
        let mut review = ImageReview::new("op", 1, 1, &[1, 2, 3], "x", "images", 8, &[]).unwrap();
        assert!(review.png().starts_with(b"\x89PNG"));
        let hash = review.fingerprint().to_owned();
        assert!(review.approve(&hash).is_ok());
        assert!(review.approve(&hash).is_err());
        let mut review = ImageReview::new("op", 1, 1, &[1, 2, 3], "x", "images", 8, &[]).unwrap();
        assert!(review.approve("stale").is_err());
        let hash = review.fingerprint().to_owned();
        assert!(review.approve(&hash).is_err());
    }
}
