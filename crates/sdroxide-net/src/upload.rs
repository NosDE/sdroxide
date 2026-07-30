//! One-click QSO upload to eQSL, QRZ Logbook and Club Log, plus QSL-confirmation
//! download from LoTW and eQSL. LoTW *upload* is intentionally not automated
//! (it requires TQSL signing) — the UI exports ADIF for the operator to sign;
//! but confirmations can still be downloaded here to drive award tracking.

use sdroxide_types::{NetworkConfig, QsoRecord, UploadTarget};

use crate::http;

/// Upload one QSO's ADIF to `target`, returning a human-readable status on
/// success or an error string.
pub fn upload(
    cfg: &NetworkConfig,
    my_call: &str,
    target: UploadTarget,
    adif: &str,
) -> Result<String, String> {
    match target {
        UploadTarget::Eqsl => upload_eqsl(cfg, adif),
        UploadTarget::QrzLogbook => upload_qrz(cfg, adif),
        UploadTarget::ClubLog => upload_clublog(cfg, my_call, adif),
    }
}

fn upload_eqsl(cfg: &NetworkConfig, adif: &str) -> Result<String, String> {
    if cfg.eqsl.user.trim().is_empty() {
        return Err("eQSL username/password not set".into());
    }
    let body = http::post_form(
        "https://www.eqsl.cc/qslcard/importADIF.cfm",
        &[
            ("EQSL_USER", cfg.eqsl.user.trim()),
            ("EQSL_PSWD", cfg.eqsl.password.trim()),
            ("ADIFData", adif),
        ],
    )?;
    // eQSL returns HTML; success contains "Result: 1 out of 1 …". Errors carry
    // an "Error:" / "Warning:" line.
    let text = strip_html(&body);
    if text.to_ascii_lowercase().contains("added") || text.contains("Result: 1") {
        Ok("eQSL: accepted".into())
    } else if let Some(line) = text.lines().find(|l| {
        let l = l.to_ascii_lowercase();
        l.contains("error") || l.contains("warning") || l.contains("bad")
    }) {
        Err(line.trim().to_string())
    } else {
        // Some accounts return a terse OK page; treat a 200 with no error as ok.
        Ok("eQSL: submitted".into())
    }
}

fn upload_qrz(cfg: &NetworkConfig, adif: &str) -> Result<String, String> {
    if cfg.qrz_logbook_key.trim().is_empty() {
        return Err("QRZ Logbook API key not set".into());
    }
    let body = http::post_form(
        "https://logbook.qrz.com/api",
        &[("KEY", cfg.qrz_logbook_key.trim()), ("ACTION", "INSERT"), ("ADIF", adif)],
    )?;
    // Response is url-encoded key=value pairs: RESULT=OK / FAIL / AUTH / REPLACE.
    let fields = parse_kv(&body);
    match fields.iter().find(|(k, _)| k == "RESULT").map(|(_, v)| v.as_str()) {
        Some("OK") => Ok("QRZ: logged".into()),
        Some("REPLACE") => Ok("QRZ: already logged (replaced)".into()),
        Some(other) => {
            let reason = fields
                .iter()
                .find(|(k, _)| k == "REASON")
                .map(|(_, v)| v.clone())
                .unwrap_or_else(|| other.to_string());
            Err(format!("QRZ: {reason}"))
        }
        None => {
            Err(format!("QRZ: unexpected response: {}", body.chars().take(120).collect::<String>()))
        }
    }
}

fn upload_clublog(cfg: &NetworkConfig, my_call: &str, adif: &str) -> Result<String, String> {
    if cfg.clublog.user.trim().is_empty() || cfg.clublog_api_key.trim().is_empty() {
        return Err("Club Log email/password/API key not set".into());
    }
    // Station callsign for the log: my_call, else the record's own is used by CL.
    let body = http::post_form(
        "https://clublog.org/realtime.php",
        &[
            ("email", cfg.clublog.user.trim()),
            ("password", cfg.clublog.password.trim()),
            ("callsign", my_call.trim()),
            ("api", cfg.clublog_api_key.trim()),
            ("adif", adif),
        ],
    )?;
    let t = body.trim();
    // Club Log returns 200 with an empty/OK body on success, error text otherwise.
    if t.is_empty() || t.eq_ignore_ascii_case("ok") {
        Ok("Club Log: accepted".into())
    } else if t.to_ascii_lowercase().contains("error") || t.len() > 3 {
        Err(format!("Club Log: {}", t.chars().take(160).collect::<String>()))
    } else {
        Ok("Club Log: accepted".into())
    }
}

/// Download QSL confirmations from LoTW and eQSL, returning parsed confirmation
/// records (the UI matches these to the log to set `*_rcvd`). Best-effort: a
/// service that isn't configured is skipped; per-service errors are collected.
pub fn sync_confirmations(cfg: &NetworkConfig) -> (Vec<QsoRecord>, Vec<String>) {
    let mut confirmed = Vec::new();
    let mut errors = Vec::new();

    if !cfg.lotw.user.trim().is_empty() {
        match download_lotw(cfg) {
            Ok(mut recs) => confirmed.append(&mut recs),
            Err(e) => errors.push(format!("LoTW: {e}")),
        }
    }
    if !cfg.eqsl.user.trim().is_empty() {
        match download_eqsl(cfg) {
            Ok(mut recs) => confirmed.append(&mut recs),
            Err(e) => errors.push(format!("eQSL: {e}")),
        }
    }
    (confirmed, errors)
}

fn download_lotw(cfg: &NetworkConfig) -> Result<Vec<QsoRecord>, String> {
    // Confirmed QSLs only (qso_qsl=yes), with detail so BAND/MODE/DATE parse.
    let url = format!(
        "https://lotw.arrl.org/lotwuser/lotwreport.adi?login={}&password={}&qso_query=1&qso_qsl=yes&qso_qsldetail=yes",
        urlencode(cfg.lotw.user.trim()),
        urlencode(cfg.lotw.password.trim())
    );
    let body = http::get(&url)?;
    if body.to_ascii_lowercase().contains("username/password") || body.contains("<!DOCTYPE") {
        return Err("login rejected".into());
    }
    let mut recs = sdroxide_types::adif_to_qso_log(&body);
    for r in &mut recs {
        r.lotw_rcvd = true; // this report is the set of LoTW-confirmed QSOs
    }
    Ok(recs)
}

fn download_eqsl(cfg: &NetworkConfig) -> Result<Vec<QsoRecord>, String> {
    // eQSL's inbox download is two-step: the first call builds an .adi and
    // returns a page linking to it.
    let url = format!(
        "https://www.eqsl.cc/qslcard/DownloadInBox.cfm?UserName={}&Password={}&RcvdSince=19700101",
        urlencode(cfg.eqsl.user.trim()),
        urlencode(cfg.eqsl.password.trim())
    );
    let page = http::get(&url)?;
    // Find the ".adi" link in the returned HTML.
    let link = page
        .split(['"', '\'', ' ', '\n'])
        .find(|t| t.to_ascii_lowercase().ends_with(".adi"))
        .ok_or("no download link returned (check credentials)")?;
    let adi_url = if link.starts_with("http") {
        link.to_string()
    } else {
        format!("https://www.eqsl.cc/qslcard/{}", link.trim_start_matches('/'))
    };
    let body = http::get(&adi_url)?;
    let mut recs = sdroxide_types::adif_to_qso_log(&body);
    for r in &mut recs {
        r.eqsl_rcvd = true; // eQSL inbox = received confirmations
    }
    Ok(recs)
}

// ── helpers ──────────────────────────────────────────────────────────────

/// Parse `k=v&k=v` (or newline-separated) into pairs; keys uppercased.
fn parse_kv(body: &str) -> Vec<(String, String)> {
    body.split(['&', '\n'])
        .filter_map(|p| p.split_once('='))
        .map(|(k, v)| (k.trim().to_ascii_uppercase(), urldecode(v.trim())))
        .collect()
}

/// Very small HTML→text: drop tags, collapse whitespace.
fn strip_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}

fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn urldecode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => {
                let hex = std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or("");
                if let Ok(v) = u8::from_str_radix(hex, 16) {
                    out.push(v as char);
                    i += 3;
                    continue;
                }
                out.push('%');
                i += 1;
            }
            b'+' => {
                out.push(' ');
                i += 1;
            }
            c => {
                out.push(c as char);
                i += 1;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_qrz_response() {
        let kv = parse_kv("RESULT=OK&COUNT=1&LOGID=123");
        assert_eq!(kv.iter().find(|(k, _)| k == "RESULT").unwrap().1, "OK");
    }

    #[test]
    fn strips_html() {
        assert_eq!(strip_html("<b>Result: 1</b> added").trim(), "Result: 1 added");
    }
}
