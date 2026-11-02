//! QR login — `GET /api/login/reqqr` → render → wait for scan → confirm.
//!
//! Evidence: `re-notes/01_login.md` §5.2, plus the live capture in the tests below.
//! The owner of this analysis is the maker of the client on this machine (Zalo PC
//! 26.10.11, extracted to `asar_extracted/`).

use anyhow::{bail, Result};
use serde_json::{json, Map, Value};
use std::time::Duration;

use crate::crypto;
use crate::login;
use crate::session::{urlencode, Session};

/// Tag used by `getSignKey(tag, params)` for the QR request.
const TAG: &str = "reqqr";

/// A QR login ticket handed out by `/api/login/reqqr`.
///
/// [O] Live capture, 2026-10-20, `self_g = 1`:
///
/// ```text
/// qr_timeout     180
/// qr_retry       3
/// token_id       "zaloqr:v2_DZ1DIqmkC2unDpaoDJ4rD3GtEJKpBZKpOpXcDMOqOcOpP3HaE3anDJXZOpCqD3GuD3XXD6Cu"
/// qr_cnt         "http://zaloapp.com/qr/l?tk=zaloqr:v2_DZ1DIqmkC2unDpaoDJ4rD3GtEJKpBZKpOpXcDMOqOcOpP3HaE3anDJXZOpCqD3GuD3XXD6Cu"
/// chk_wait_scan  "https://qr-id.zaloapp.com/polling/qr/waiting?src=pc&code=60MKL.0.1792515447953.53c8f5f4bf3d4d89158cc3444848a4c8"
/// chk_wait_cfirm "https://qr-id.zaloapp.com/polling/qr/pc-status?src=pc&code=60MKL.0.1792515447953.53c8f5f4bf3d4d89158cc3444848a4c8"
/// ```
///
/// With `self_g = 0` the server renders the image itself and answers `base64_qr`
/// with a 22 KB PNG instead of `qr_cnt`; the encoded payload is the same URL.
#[derive(Debug, Clone, Default)]
pub struct QrLogin {
    /// `zaloqr:v2_…` — the token inside the QR.
    pub token_id: String,
    /// `http://zaloapp.com/qr/l?tk=…` — what the QR encodes.
    pub qr_payload: String,
    /// Server-rendered PNG, when the server produced one.
    pub base64_png: Option<String>,
    /// Long-poll endpoint that resolves once the phone scans the code.
    pub chk_wait_scan: String,
    /// Endpoint that turns a confirmed scan into a session.
    pub chk_wait_cfirm: String,
    /// Seconds the code stays valid.
    pub qr_timeout: u64,
    /// How many times the client may re-poll.
    pub qr_retry: u32,
}

impl QrLogin {
    /// Parse the `data` object of a `reqqr` reply.
    pub fn from_data(d: &Value) -> Result<Self> {
        let s = |k: &str| d.get(k).and_then(Value::as_str).unwrap_or("").to_string();
        let png = d
            .get("base64_qr")
            .and_then(Value::as_str)
            .filter(|v| !v.is_empty())
            .map(str::to_string);

        let out = Self {
            token_id: s("token_id"),
            qr_payload: s("qr_cnt"),
            base64_png: png,
            chk_wait_scan: s("chk_wait_scan"),
            chk_wait_cfirm: s("chk_wait_cfirm"),
            qr_timeout: d.get("qr_timeout").and_then(Value::as_u64).unwrap_or(180),
            qr_retry: d.get("qr_retry").and_then(Value::as_u64).unwrap_or(3) as u32,
        };

        if out.chk_wait_scan.is_empty() {
            bail!("reqqr: không có chk_wait_scan");
        }
        if out.qr_payload.is_empty() && out.base64_png.is_none() {
            bail!("reqqr: không có qr_cnt lẫn base64_qr");
        }
        Ok(out)
    }

    /// The text the QR encodes.
    pub fn payload(&self) -> Option<&str> {
        if self.qr_payload.is_empty() {
            None
        } else {
            Some(&self.qr_payload)
        }
    }
}

/// Build the request parameters for `/api/login/reqqr`.
///
/// [O] `getQRCode(imei, logged_uids, language, self_g, qr_form)`:
///
/// ```text
/// { language, client_time, imei, computer_name, logged_uids, self_g, qr_form }
/// ```
///
/// `self_g = 1` asks the server for the *payload* (`qr_cnt`) instead of a rendered
/// image, so the QR can be drawn in the terminal.
pub fn req_qr_params(s: &Session, language: &str, self_g: u8) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("language".into(), json!(language));
    m.insert("client_time".into(), json!(login::unix_now()));
    m.insert("imei".into(), json!(s.imei));
    m.insert("computer_name".into(), json!(s.computer_name));
    m.insert("logged_uids".into(), json!("[]"));
    m.insert("self_g".into(), json!(self_g));
    m.insert("qr_form".into(), json!(0));
    m.insert("type".into(), json!(crate::session::ZPW_TYPE));
    m.insert("client_version".into(), json!(crate::session::ZPW_VER));
    m
}

/// Flatten a parameter map into a query string, signing it on the way.
///
/// [O] `_constructUrlParams` writes `key=encodeURIComponent(value)`; the signature is
/// `getSignKey(tag, params)` = md5("zsecure" + tag + values sorted by key), computed
/// over the *same* strings that go on the wire.
pub fn signed_query(params: &Map<String, Value>, tag: &str) -> String {
    let sig = crypto::sign_key(tag, params);
    let mut m = params.clone();
    m.insert("signkey".into(), json!(sig));

    let mut pairs: Vec<String> = m
        .iter()
        .map(|(k, v)| {
            let val = match v {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            format!("{}={}", k, urlencode(&val))
        })
        .collect();
    pairs.sort();
    pairs.join("&")
}

/// Ask the server for a QR ticket.
///
/// `self_g = 1` returns the payload so we can render the code ourselves; the server
/// accepted it live on 2026-10-20 even though its own `self_g` flag is `0`.
pub async fn request_qr(s: &Session, language: &str) -> Result<QrLogin> {
    let params = req_qr_params(s, language, 1);
    let url = format!(
        "{}/api/login/reqqr?{}",
        s.domains.auth,
        signed_query(&params, TAG)
    );

    let (status, text, _) = get_raw(s, &url).await?;
    if status != 200 {
        bail!("reqqr: HTTP {status}: {}", crate::session::truncate(&text, 200));
    }
    let v: Value = serde_json::from_str(&text)
        .map_err(|e| anyhow::anyhow!("reqqr: không phải JSON ({e}): {}", crate::session::truncate(&text, 200)))?;
    let code = v.get("error_code").and_then(Value::as_i64).unwrap_or(0);
    if code != 0 {
        let msg = v.get("error_message").and_then(Value::as_str).unwrap_or("");
        bail!("reqqr: server error {code}: {msg}");
    }
    QrLogin::from_data(
        v.get("data")
            .ok_or_else(|| anyhow::anyhow!("reqqr: response has no data"))?,
    )
}

/// Draw the QR as terminal art.
///
/// `Dense1x2` packs two module rows into one character cell, so the code keeps its
/// square proportions on a terminal (a cell is twice as tall as it is wide).
pub fn render_terminal(q: &QrLogin) -> Result<String> {
    use qrcode::render::unicode::Dense1x2;

    let payload = q
        .payload()
        .ok_or_else(|| anyhow::anyhow!("server chỉ trả về ảnh PNG, không có payload để vẽ"))?;
    let code = qrcode::QrCode::new(payload.as_bytes())?;
    Ok(code
        .render::<Dense1x2>()
        .quiet_zone(true)
        .module_dimensions(1, 1)
        .min_dimensions(25, 25)
        .build())
}

/// Write the server-rendered PNG to disk, when there is one.
pub fn save_png(q: &QrLogin, path: &str) -> Result<()> {
    use base64::{engine::general_purpose::STANDARD as B64, Engine};
    let Some(b64) = q.base64_png.as_deref() else {
        bail!("không có ảnh PNG để lưu");
    };
    let raw = B64.decode(b64.trim())?;
    std::fs::write(path, raw)?;
    Ok(())
}

/// Long-poll the scan endpoint until the phone reports the scan.
///
/// [O] The client appends `&lang=<code>` and loops with `qr_timeout` / `qr_retry`.
/// While nobody has scanned, the endpoint answers **HTTP 408** — that is the
/// "still waiting" signal, not a failure.
pub async fn wait_for_scan(s: &Session, q: &QrLogin) -> Result<Value> {
    let url = format!("{}&lang=vi", q.chk_wait_scan);
    let deadline = std::time::Instant::now() + Duration::from_secs(q.qr_timeout.max(30));
    let mut attempts: u32 = 0;

    loop {
        if std::time::Instant::now() >= deadline {
            bail!("hết thời gian chờ quét QR ({}s)", q.qr_timeout);
        }

        let (status, text, _) = get_raw(s, &url).await?;
        match status {
            // Nothing scanned yet — the long poll expired.
            408 => {
                tokio::time::sleep(Duration::from_millis(800)).await;
                continue;
            }
            200 => {
                let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
                let code = v.get("error_code").and_then(Value::as_i64).unwrap_or(0);
                if code == 0 {
                    if let Some(data) = v.get("data") {
                        if !data.is_null() {
                            return Ok(data.clone());
                        }
                    }
                } else {
                    attempts += 1;
                    if attempts > q.qr_retry.max(1) {
                        let msg = v.get("error_message").and_then(Value::as_str).unwrap_or("");
                        bail!("waiting scan: server error {code}: {msg}");
                    }
                }
                tokio::time::sleep(Duration::from_millis(800)).await;
            }
            other => bail!("waiting scan: HTTP {other}: {}", crate::session::truncate(&text, 200)),
        }
    }
}

/// Confirm the scan and turn it into a session.
/// Polls until user confirms on mobile or timeout.
pub async fn confirm(s: &Session, q: &QrLogin) -> Result<Value> {
    if q.chk_wait_cfirm.is_empty() {
        bail!("reqqr: không có chk_wait_cfirm");
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(q.qr_timeout.max(60));
    println!("Đã quét mã QR! Vui lòng bấm 'Đăng nhập' trên điện thoại…");

    loop {
        if std::time::Instant::now() >= deadline {
            bail!("hết thời gian chờ xác nhận trên điện thoại");
        }

        let (status, text, cookie_sek) = get_raw(s, &q.chk_wait_cfirm).await?;
        if status == 408 {
            tokio::time::sleep(Duration::from_millis(800)).await;
            continue;
        }
        if status != 200 {
            bail!("pc-status: HTTP {status}: {}", crate::session::truncate(&text, 200));
        }

        let mut v: Value = serde_json::from_str(&text)
            .map_err(|e| anyhow::anyhow!("pc-status: không phải JSON ({e}): {}", crate::session::truncate(&text, 200)))?;
        let code = v.get("error_code").and_then(Value::as_i64).unwrap_or(0);
        if code == 0 {
            if let Some(body_sek) = extract_zpw_sek(&v) {
                if let Ok(mut g) = s.live_sek.write() {
                    if g.is_none() {
                        *g = Some(body_sek.clone());
                    }
                }
                if let Ok(mut m) = s.live_cookies.write() {
                    m.entry("zpw_sek".into()).or_insert(body_sek);
                }
            }
            if let Some(sek) = cookie_sek {
                if let Some(d) = v.get_mut("data").and_then(Value::as_object_mut) {
                    let has_body_sek = d
                        .get("zpw_sek")
                        .and_then(Value::as_str)
                        .map(|x| !x.is_empty() && x != "EXPIRED")
                        .unwrap_or(false);
                    if !has_body_sek {
                        d.insert("zpw_sek".into(), Value::String(sek));
                    }
                }
            }
            return Ok(v);
        } else if code == 18012 {
            // 18012: phone scanned, waiting for user to tap Confirm
            tokio::time::sleep(Duration::from_millis(800)).await;
            continue;
        } else {
            let msg = v.get("error_message").and_then(Value::as_str).unwrap_or("");
            bail!("pc-status: server error {code}: {msg}");
        }
    }
}

/// The whole QR login: ticket → code on screen → wait → confirm → session.
pub async fn login_via_qr(s: &mut Session, language: &str) -> Result<Value> {
    let q = request_qr(s, language).await?;

    match render_terminal(&q) {
        Ok(art) => println!("{art}"),
        Err(e) => {
            eprintln!("! không vẽ được QR trong terminal: {e}");
            let path = "qr.png";
            save_png(&q, path)?;
            println!("  ảnh QR đã lưu: {path}");
        }
    }
    println!("Quét mã bằng Zalo trên điện thoại…");

    let scanned = wait_for_scan(s, &q).await?;
    if let Some(name) = scanned
        .get("display_name")
        .or_else(|| scanned.get("displayName"))
        .and_then(Value::as_str)
    {
        println!("✓ Đã quét: {name}");
    }

    let resp = confirm(s, &q).await?;
    if let Some(sek) = extract_zpw_sek(&resp) {
        s.set_zpw_sek(sek);
    }

    println!("✓ Đăng nhập thành công trên điện thoại! Đang đồng bộ khoá phiên…");
    let info = login::get_login_info(s).await?;
    if let Some(sek) = extract_zpw_sek(&info) {
        s.set_zpw_sek(sek);
    }
    if let Some(cfg) = info.get("data") {
        login::apply_login_config(s, cfg);
        s.apply_server_info(cfg);
        s.sync_live_sek();
        let _ = s.sync_profile_me().await;
        s.sync_live_sek();
        println!("✓ Đã nạp khoá phiên zpw_enk và cập nhật endpoints!");
        if let Err(e) = s.save_to_file("session.json") {
            eprintln!("! Lỗi lưu session.json: {e}");
        } else {
            println!("✓ Đã lưu phiên vào session.json (tự động khôi phục cho các lần chạy sau)");
        }
    } else {
        bail!("getLoginInfo không trả về data: {info}");
    }

    Ok(resp)
}

pub fn extract_zpw_sek(v: &Value) -> Option<String> {
    for key in ["zpw_sek", "zpw_kt", "zpw_wpsk", "token", "session_key"] {
        if let Some(s) = v.get(key).and_then(Value::as_str) {
            if !s.is_empty() && s != "EXPIRED" {
                return Some(s.to_string());
            }
        }
        if let Some(s) = v.get("data").and_then(|d| d.get(key)).and_then(Value::as_str) {
            if !s.is_empty() && s != "EXPIRED" {
                return Some(s.to_string());
            }
        }
        if let Some(s) = v.get("data").and_then(|d| d.get("account")).and_then(|a| a.get(key)).and_then(Value::as_str) {
            if !s.is_empty() && s != "EXPIRED" {
                return Some(s.to_string());
            }
        }
    }
    None
}

/// GET without the error-code check in `Session::send` — the polling endpoint needs
/// the raw status (408 means "waiting"). Also extracts any `Set-Cookie` header.
async fn get_raw(s: &Session, url: &str) -> Result<(u16, String, Option<String>)> {
    let resp = s.http.get(url).send().await?;
    let status = resp.status().as_u16();
    s.ingest_set_cookie_headers(resp.headers());
    let mut sek = None;
    for val in resp.headers().get_all("set-cookie") {
        if let Ok(sc) = val.to_str() {
            let lower = sc.to_ascii_lowercase();
            if lower.contains("1970") || lower.contains("max-age=0") {
                continue;
            }
            if let Some(pos) = sc.find("zpw_sek=") {
                let token = sc[pos + 8..].split(';').next().unwrap_or("").trim();
                if !token.is_empty() && token != "EXPIRED" {
                    sek = Some(token.to_string());
                }
            }
        }
    }
    let text = resp.text().await?;
    if s.verbose {
        eprintln!("→ GET {}", crate::session::redact_url(url));
        eprintln!("← {status} {}", crate::session::truncate(&text, 200));
        if let Some(ref k) = sek {
            eprintln!("  Captured zpw_sek cookie: {}", crate::session::redact_secret(k));
        }
    }
    Ok((status, text, sek))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Live capture, 2026-10-20, `self_g = 1`.
    const LIVE_RESP: &str = r#"{
      "qr_timeout": 180,
      "token_id": "zaloqr:v2_DZ1DIqmkC2unDpaoDJ4rD3GtEJKpBZKpOpXcDMOqOcOpP3HaE3anDJXZOpCqD3GuD3XXD6Cu",
      "qr_retry": 3,
      "chk_wait_cfirm": "https://qr-id.zaloapp.com/polling/qr/pc-status?src=pc&code=60MKL.0.1792515447953.53c8f5f4bf3d4d89158cc3444848a4c8",
      "qr_cnt": "http://zaloapp.com/qr/l?tk=zaloqr:v2_DZ1DIqmkC2unDpaoDJ4rD3GtEJKpBZKpOpXcDMOqOcOpP3HaE3anDJXZOpCqD3GuD3XXD6Cu",
      "chk_wait_scan": "https://qr-id.zaloapp.com/polling/qr/waiting?src=pc&code=60MKL.0.1792515447953.53c8f5f4bf3d4d89158cc3444848a4c8"
    }"#;

    #[test]
    fn parses_live_response() {
        let v: Value = serde_json::from_str(LIVE_RESP).unwrap();
        let q = QrLogin::from_data(&v).unwrap();
        assert_eq!(q.qr_timeout, 180);
        assert_eq!(q.qr_retry, 3);
        assert!(q.qr_payload.starts_with("http://zaloapp.com/qr/l?tk="));
        assert!(q.chk_wait_scan.contains("/polling/qr/waiting"));
        assert!(q.chk_wait_cfirm.contains("/polling/qr/pc-status"));
        assert!(q.base64_png.is_none());
    }

    #[test]
    fn payload_carries_the_token() {
        let v: Value = serde_json::from_str(LIVE_RESP).unwrap();
        let q = QrLogin::from_data(&v).unwrap();
        assert!(q.payload().unwrap().contains(&q.token_id));
    }

    #[test]
    fn renders_scannable_looking_art() {
        let v: Value = serde_json::from_str(LIVE_RESP).unwrap();
        let q = QrLogin::from_data(&v).unwrap();
        let art = render_terminal(&q).unwrap();
        let lines: Vec<&str> = art.lines().collect();
        // A version-6 code is 41 modules + a 4-module quiet zone; two rows per line.
        assert!(lines.len() >= 20, "rows = {}", lines.len());
        assert!(lines[0].chars().count() >= 40, "cols = {}", lines[0].chars().count());
        assert!(
            art.contains('█') || art.contains('▀') || art.contains('▄'),
            "the code must use block characters"
        );
        // The quiet zone has to be blank on every side.
        assert!(lines[0].trim().is_empty());
        assert!(lines[lines.len() - 1].trim().is_empty());
    }

    #[test]
    fn signed_query_carries_the_signature() {
        let mut m = Map::new();
        m.insert("language".into(), json!("vi"));
        m.insert("type".into(), json!("30"));
        m.insert("client_version".into(), json!("693"));
        let q = signed_query(&m, TAG);
        assert!(q.contains("signkey="));
        // md5("zsecure" + "reqqr" + values sorted by key): client_version, language, type.
        let expect = crypto::md5_hex("zsecurereqqr693vi30");
        assert!(q.contains(&format!("signkey={expect}")));
    }

    #[test]
    fn params_carry_the_documented_fields() {
        let s = Session::new(false, false, false).unwrap();
        let p = req_qr_params(&s, "vi", 1);
        for k in [
            "language",
            "client_time",
            "imei",
            "computer_name",
            "logged_uids",
            "self_g",
            "qr_form",
            "type",
            "client_version",
        ] {
            assert!(p.get(k).is_some(), "missing {k}");
        }
        assert_eq!(p["self_g"], json!(1));
        assert_eq!(p["qr_form"], json!(0));
        assert_eq!(p["type"], json!("30"));
    }

    #[test]
    fn missing_wait_url_is_rejected() {
        let v = json!({"qr_cnt": "http://x/"});
        assert!(QrLogin::from_data(&v).is_err());
    }
}
