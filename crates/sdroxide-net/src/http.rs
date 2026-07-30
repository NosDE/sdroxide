//! Small blocking HTTP(S) helpers over `ureq` (rustls, pure-Rust TLS). Every
//! call is on a worker thread, so blocking is fine.

use std::time::Duration;

/// Shared agent with sane timeouts. Cheap to build per call.
fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(10))
        .timeout(Duration::from_secs(20))
        .user_agent(concat!("sdroxide/", env!("CARGO_PKG_VERSION")))
        .build()
}

/// GET `url`, returning the response body as a string.
pub fn get(url: &str) -> Result<String, String> {
    agent().get(url).call().map_err(err_string)?.into_string().map_err(|e| e.to_string())
}

/// POST `fields` as `application/x-www-form-urlencoded`.
pub fn post_form(url: &str, fields: &[(&str, &str)]) -> Result<String, String> {
    agent()
        .post(url)
        .send_form(fields)
        .map_err(err_string)?
        .into_string()
        .map_err(|e| e.to_string())
}

/// Turn a ureq error into a compact message, unwrapping HTTP status errors so
/// the caller sees the server's own body (many APIs report failures in it).
fn err_string(e: ureq::Error) -> String {
    match e {
        ureq::Error::Status(code, resp) => {
            let body = resp.into_string().unwrap_or_default();
            let body = body.trim();
            if body.is_empty() {
                format!("HTTP {code}")
            } else {
                format!("HTTP {code}: {}", body.chars().take(200).collect::<String>())
            }
        }
        ureq::Error::Transport(t) => t.to_string(),
    }
}
