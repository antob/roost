use chrono::{DateTime, Duration, SecondsFormat, Utc};
use sha2::{Digest, Sha256};

pub fn ts(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Micros, true)
}

pub fn now() -> String {
    ts(Utc::now())
}

pub fn now_plus(secs: i64) -> String {
    ts(Utc::now() + Duration::seconds(secs))
}

pub fn parse_ts(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

/// RFC 3339, or the same without seconds (`2026-06-26T14:00+02:00`); an offset is required.
pub fn parse_when(s: &str) -> Option<DateTime<Utc>> {
    parse_ts(s).or_else(|| {
        ["%Y-%m-%dT%H:%M%:z", "%Y-%m-%dT%H:%M%z"]
            .iter()
            .find_map(|f| DateTime::parse_from_str(s, f).ok())
            .map(|t| t.with_timezone(&Utc))
    })
}

pub fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

fn random_from(alphabet: &[u8], len: usize) -> String {
    let mut out = String::with_capacity(len);
    while out.len() < len {
        for b in uuid::Uuid::new_v4().into_bytes() {
            // Rejection sampling keeps the distribution uniform over the alphabet.
            let limit = 256 - (256 % alphabet.len());
            if (b as usize) < limit && out.len() < len {
                out.push(alphabet[b as usize % alphabet.len()] as char);
            }
        }
    }
    out
}

pub fn new_channel_key() -> String {
    format!(
        "rst_{}",
        random_from(
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789",
            40
        )
    )
}

pub fn random_token(len: usize) -> String {
    random_from(
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789",
        len,
    )
}

pub fn url_safe_b64(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

pub fn new_claim_code() -> String {
    let raw = random_from(b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789", 8);
    format!("{}-{}", &raw[..4], &raw[4..])
}
