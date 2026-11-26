use std::{
    fs,
    io::{Read, Write},
    net::TcpStream,
    time::Duration,
};

use anyhow::{Context, Result};
use serde_json::Value;
use tracing::info;

const DEFAULT_ROTATOR_ADDR: &str = "127.0.0.1:11111";
const HTTP_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone)]
pub struct RotatedAccount {
    pub account_id: String,
    pub label: String,
}

fn rotator_addr() -> String {
    std::env::var("ACCOUNT_ROTATOR_ADDR")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_ROTATOR_ADDR.to_string())
}

fn read_webclx_local_token() -> Result<String> {
    let path = std::env::var("ACCOUNT_ROTATOR_TOKEN_FILE")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            std::env::var("WEBCLX_LOCAL_TOKEN_FILE")
                .ok()
                .filter(|s| !s.trim().is_empty())
        })
        .ok_or_else(|| {
            anyhow::anyhow!(
                "account rotator token file not configured (set ACCOUNT_ROTATOR_TOKEN_FILE or WEBCLX_LOCAL_TOKEN_FILE)"
            )
        })?;
    let token = fs::read_to_string(&path)
        .with_context(|| format!("failed to read account rotator token from {path}"))?;
    let trimmed = token.trim().to_string();
    anyhow::ensure!(!trimmed.is_empty(), "account rotator token file is empty");
    Ok(trimmed)
}

fn loopback_webclx_request(method: &str, path: &str, body: Option<&str>) -> Result<(u16, String)> {
    let token = read_webclx_local_token()?;
    let addr_str = rotator_addr();
    let addr: std::net::SocketAddr = addr_str
        .parse()
        .with_context(|| format!("invalid ACCOUNT_ROTATOR_ADDR: {addr_str}"))?;
    let mut stream = TcpStream::connect_timeout(&addr, HTTP_TIMEOUT)
        .context("failed to connect to account rotator local API")?;
    stream.set_read_timeout(Some(HTTP_TIMEOUT))?;
    stream.set_write_timeout(Some(HTTP_TIMEOUT))?;

    let payload = body.unwrap_or("");
    let req = if body.is_some() {
        format!(
            "{method} {path} HTTP/1.1\r\nHost: {addr_str}\r\nX-WebClx-Local-Token: {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
            payload.len()
        )
    } else {
        format!(
            "{method} {path} HTTP/1.1\r\nHost: {addr_str}\r\nX-WebClx-Local-Token: {token}\r\nConnection: close\r\n\r\n"
        )
    };

    stream.write_all(req.as_bytes())?;
    stream.flush()?;

    let mut raw = Vec::new();
    stream.read_to_end(&mut raw)?;
    parse_http_response(&raw)
}

pub fn parse_http_response(raw: &[u8]) -> Result<(u16, String)> {
    let text = String::from_utf8_lossy(raw);
    let (head, body) = text
        .split_once("\r\n\r\n")
        .context("malformed HTTP response from webClx")?;
    let status_line = head.lines().next().unwrap_or("");
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .context("missing HTTP status code")?;

    let is_chunked = head
        .lines()
        .any(|l| l.to_ascii_lowercase().contains("transfer-encoding: chunked"));
    let decoded_body = if is_chunked {
        decode_chunked_body(body)
    } else {
        body.to_string()
    };
    Ok((status, decoded_body))
}

fn decode_chunked_body(raw: &str) -> String {
    let mut out = String::new();
    let mut rest = raw;
    while let Some((size_line, after_size)) = rest.split_once("\r\n") {
        let size_hex = size_line.split(';').next().unwrap_or("").trim();
        let Ok(len) = usize::from_str_radix(size_hex, 16) else {
            break;
        };
        if len == 0 {
            break;
        }
        if after_size.len() < len {
            out.push_str(after_size);
            break;
        }
        out.push_str(&after_size[..len]);
        rest = after_size[len..].strip_prefix("\r\n").unwrap_or(&after_size[len..]);
    }
    out
}

fn extract_remaining_credits(account: &Value) -> Option<f64> {
    account
        .pointer("/credits/remaining")
        .or_else(|| account.get("remaining"))
        .and_then(Value::as_f64)
        .filter(|v| v.is_finite())
}

pub fn select_next_codebuddy_account(
    accounts: &[Value],
    tried_ids: &[String],
    block_international: bool,
) -> Option<(String, String)> {
    let mut candidates: Vec<(i32, String, String)> = Vec::new();
    for acc in accounts {
        let Some(id) = acc.get("id").and_then(Value::as_str) else {
            continue;
        };
        if acc.get("disabled").and_then(Value::as_bool) == Some(true)
            || acc.get("active").and_then(Value::as_bool) == Some(true)
            || acc.get("needsRelogin").and_then(Value::as_bool) == Some(true)
            || tried_ids.iter().any(|t| t == id)
        {
            continue;
        }
        let variant = acc.get("variant").and_then(Value::as_str).unwrap_or("cn");
        if block_international && matches!(variant, "ai" | "intl") {
            continue;
        }
        let credits = extract_remaining_credits(acc);
        if let Some(rem) = credits {
            if rem <= 0.0 {
                continue;
            }
        }
        let score = match credits {
            Some(rem) => (rem * 100.0) as i32 + 1000,
            None => 500, // Unknown/unprobed credits still viable fallback
        };
        let label = acc
            .get("nickname")
            .or_else(|| acc.get("email"))
            .or_else(|| acc.get("name"))
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .unwrap_or(id)
            .to_string();
        candidates.push((score, id.to_string(), label));
    }

    candidates.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    candidates.into_iter().next().map(|(_, id, label)| (id, label))
}

pub fn current_active_codebuddy_account_id() -> Option<String> {
    let (status, body) = loopback_webclx_request("GET", "/api/auth/codebuddy-accounts", None).ok()?;
    if status != 200 {
        return None;
    }
    let accounts: Vec<Value> = serde_json::from_str(&body).ok()?;
    accounts
        .iter()
        .find(|acc| acc.get("active").and_then(Value::as_bool) == Some(true))
        .and_then(|acc| acc.get("id").and_then(Value::as_str))
        .map(str::to_string)
}

pub fn rotate_codebuddy_account(tried_ids: &[String]) -> Result<Option<RotatedAccount>> {
    let (status, body) = loopback_webclx_request("GET", "/api/auth/codebuddy-accounts", None)?;
    anyhow::ensure!(
        status == 200,
        "webClx GET /api/auth/codebuddy-accounts returned HTTP {status}"
    );
    let accounts: Vec<Value> =
        serde_json::from_str(&body).context("invalid JSON from codebuddy-accounts")?;

    let mut block_international = true;
    if let Ok((auto_status, auto_body)) =
        loopback_webclx_request("GET", "/api/auth/codebuddy-auto", None)
    {
        if auto_status == 200 {
            if let Ok(auto_val) = serde_json::from_str::<Value>(&auto_body) {
                block_international = auto_val
                    .get("blockInternationalAutoSwitch")
                    .and_then(Value::as_bool)
                    .unwrap_or(true);
            }
        }
    }

    let Some((next_id, label)) =
        select_next_codebuddy_account(&accounts, tried_ids, block_international)
    else {
        return Ok(None);
    };

    let switch_path = format!("/api/auth/codebuddy-accounts/{next_id}/switch");
    let (sw_status, sw_body) = loopback_webclx_request("PUT", &switch_path, Some("{}"))?;
    anyhow::ensure!(
        (200..300).contains(&sw_status),
        "webClx PUT {switch_path} failed ({sw_status}): {sw_body}"
    );

    info!(account_id = %next_id, label = %label, "rotated CodeBuddy CLI account via webClx");
    Ok(Some(RotatedAccount {
        account_id: next_id,
        label,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn selects_highest_credit_usable_codebuddy_account() {
        let accounts = vec![
            json!({"id": "acc-active", "active": true, "disabled": false, "variant": "cn", "credits": {"remaining": 500.0}}),
            json!({"id": "acc-disabled", "active": false, "disabled": true, "variant": "cn", "credits": {"remaining": 900.0}}),
            json!({"id": "acc-relogin", "active": false, "disabled": false, "needsRelogin": true, "variant": "cn", "credits": {"remaining": 800.0}}),
            json!({"id": "acc-intl", "active": false, "disabled": false, "variant": "ai", "credits": {"remaining": 700.0}}),
            json!({"id": "acc-zero", "active": false, "disabled": false, "variant": "cn", "credits": {"remaining": 0.0}}),
            json!({"id": "acc-low", "active": false, "disabled": false, "variant": "cn", "credits": {"remaining": 20.0}, "nickname": "Low"}),
            json!({"id": "acc-best", "active": false, "disabled": false, "variant": "cn", "credits": {"remaining": 150.0}, "nickname": "Best"}),
            json!({"id": "acc-unknown", "active": false, "disabled": false, "variant": "cn"}),
        ];

        let picked = select_next_codebuddy_account(&accounts, &[], true).unwrap();
        assert_eq!(picked.0, "acc-best");
        assert_eq!(picked.1, "Best");

        // Excluding acc-best picks acc-low next (still preferred over unknown credits)
        let picked2 =
            select_next_codebuddy_account(&accounts, &["acc-best".to_string()], true).unwrap();
        assert_eq!(picked2.0, "acc-low");

        // Excluding acc-best & acc-low falls back to acc-unknown
        let picked3 = select_next_codebuddy_account(
            &accounts,
            &["acc-best".to_string(), "acc-low".to_string()],
            true,
        )
        .unwrap();
        assert_eq!(picked3.0, "acc-unknown");
    }

    #[test]
    fn decodes_chunked_http_response() {
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n4\r\n[ok]\r\n0\r\n\r\n";
        let (status, body) = parse_http_response(raw).unwrap();
        assert_eq!(status, 200);
        assert_eq!(body, "[ok]");
    }
}
