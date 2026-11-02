//! Pre-login flow: server info → password login → session bootstrap.
//!
//! Notes: `re-notes/01_login.md` §5.

use anyhow::Result;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::param_cipher::ParamCipher;
use crate::session::{obj, urlencode, Session, ZPW_TYPE, ZPW_VER};

/// PasswordCipher (class N) — `re-notes/01_login.md` §2.2.
///
/// `enk = even(sha256_upper("{pwKey}-{phone}-{imei}-{salt}"))[0..16]
///        + reverse(odd(sha256_upper(...)))[0..16]`
/// `encryptPassword = base64(AES256CBC(enk, iv=0, salt + reverse(pwd)))`
pub fn encrypt_password(phone: &str, imei: &str, password: &str, salt: &str) -> Result<String> {
    const PW_KEY: &str = "9fc5vEW3";

    let mut h = Sha256::new();
    h.update(format!("{PW_KEY}-{phone}-{imei}-{salt}").as_bytes());
    let hex = h.finalize().iter().fold(String::new(), |mut acc, b| {
        use std::fmt::Write;
        let _ = write!(acc, "{b:02X}");
        acc
    });

    let even: Vec<char> = hex.chars().step_by(2).collect();
    let odd: Vec<char> = hex.chars().skip(1).step_by(2).collect();
    let enk: String = even[..16].iter().collect::<String>()
        + &odd.iter().rev().take(16).collect::<String>();

    let mut plain = salt.to_string();
    plain.extend(password.chars().rev());

    crate::crypto::encode_aes(enk.as_bytes(), &plain)
}

/// Salt generator for encrypt_password: 8 random alphanumerics.
pub fn random_salt() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    const ALPHA: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let mut seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(1)
        ^ ((std::process::id() as u64) << 17);
    let mut out = String::with_capacity(8);
    for _ in 0..8 {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        out.push(ALPHA[(seed >> 33) as usize % ALPHA.len()] as char);
    }
    out
}

/// `GET {auth}/api/login/getServerInfo` — unauthenticated, gates param encryption.
///
/// [O] Bundle body:
/// ```text
/// o = { imei, type: apiType, client_version: apiVersion, computer_name,
///       sp_mtn: -1, bkt: -1 }
/// o.signkey = getSignKey("getserverinfo", o)
/// GET {auth}/api/login/getServerInfo?a=1&b=2…   // params inline, NOT params=
/// ```
pub async fn get_server_info(s: &Session) -> Result<Value> {
    let mut p = serde_json::Map::new();
    p.insert("imei".into(), json!(s.imei));
    p.insert("type".into(), json!(ZPW_TYPE));
    p.insert("client_version".into(), json!(ZPW_VER));
    p.insert("computer_name".into(), json!(s.computer_name));
    p.insert("sp_mtn".into(), json!(-1));
    p.insert("bkt".into(), json!(-1));

    let sig = crate::crypto::sign_key("getserverinfo", &p);
    let query: String = p
        .iter()
        .map(|(k, v)| {
            let val = match v {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            format!("{}={}", k, urlencode(&val))
        })
        .collect::<Vec<_>>()
        .join("&");

    let url = format!(
        "{}/api/login/getServerInfo?{query}&signkey={sig}",
        s.domains.auth
    );
    s.get(&url).await
}

/// v1 password login: plain params, signkey over the params object itself.
///
/// [O] `signInPassword`: `o = {phone_num, password, cptch_token, cptch_answer, type,
/// client_version, computer_name, imei}`; then `o.ref`, then `o.signkey =
/// getSignKey("loginbypassw", o)`; POST `params=<encodeURIComponent(JSON)>`.
///
/// Kept as a diagnostic path — it exposes what the server objects to without the v2
/// encryption layer in the way.
pub async fn login_by_password_v1(
    s: &mut Session,
    phone: &str,
    password: &str,
) -> Result<Value> {
    let mut o = serde_json::Map::new();
    o.insert("phone_num".into(), json!(phone));
    o.insert("password".into(), json!(password));
    o.insert("cptch_token".into(), json!(""));
    o.insert("cptch_answer".into(), json!(""));
    o.insert("type".into(), json!(ZPW_TYPE));
    o.insert("client_version".into(), json!(ZPW_VER));
    o.insert("computer_name".into(), json!(s.computer_name));
    o.insert("imei".into(), json!(s.imei));
    o.insert("ref".into(), json!("https://chat.zalo.me"));

    let sig = crate::crypto::sign_key("loginbypassw", &o);
    o.insert("signkey".into(), json!(sig));

    let url = format!("{}/api/login/loginByPassw", s.domains.auth);
    let form = format!("params={}", urlencode(&Value::Object(o).to_string()));
    let resp = s.post_form(&url, form).await?;

    let code = resp.get("error_code").and_then(|c| c.as_i64()).unwrap_or(0);
    if code != 0 {
        let msg = resp.get("error_message").and_then(|m| m.as_str()).unwrap_or("");
        anyhow::bail!("v1 login error {code}: {msg}");
    }
    if let Some(cfg) = resp.get("data") {
        apply_login_config(s, cfg);
    }
    Ok(resp)
}

/// `POST {auth}/api/login/loginByPasswv2` — ParamCipher v2 body.
///
/// [O] Exact bundle flow (`signInPasswordV2`):
/// ```text
/// g = {phone_num, iso_country_code, password, cptch_token, cptch_answer, make_answer,
///      language, logged_uids, computer_name, imei}
/// if (enable_encrypt_param) { n = new N({phone, imei}); g.salt = n.salt; g.password = n.encryptPassword(t) }
/// l && g.quest_cert = l;  c && g.cptch_cert = c;  h && g.last_uid = h;  m && g.viewerkey = m
/// {params, enk} = encryptParam(g, "loginbypasswv2")
/// le && (f.ref = window.location.origin)      // ← AFTER signing, NOT part of signkey
/// POST {auth}/api/login/loginByPasswv2?_constructUrlParams(f)
/// ```
/// Key detail: `salt` replaces the password only; `ref` is added post-signature.
pub async fn login_by_password(
    s: &mut Session,
    phone: &str,
    password: &str,
    iso: &str,
) -> Result<Value> {
    let salt = random_salt();
    let pwd = encrypt_password(phone, &s.imei, password, &salt)?;
    let client_time = unix_now();

    // Field order and set must match `g` exactly — the signkey hashes sorted values.
    //
    // `logged_uids` is passed as the caller's value; the real client sends the stored
    // uid list, and `null` when there is none. Sending the string "[]" changes both the
    // signed payload and what the server expects to see.
    let payload = obj(&[
        ("phone_num", json!(phone)),
        ("iso_country_code", json!(iso)),
        ("password", json!(pwd)),
        ("salt", json!(salt)),
        ("cptch_token", json!("")),
        ("cptch_answer", json!("")),
        ("make_answer", json!(0)),
        ("language", json!("en")),
        ("logged_uids", json!(null)),
        ("computer_name", json!(s.computer_name)),
        ("imei", json!(s.imei)),
    ]);

    let cipher = ParamCipher::new(ZPW_TYPE, &s.imei, client_time)?;
    let body = cipher.build_body(ZPW_TYPE, ZPW_VER, &payload, "loginbypasswv2")?;

    // Signed set only — no `enk`, no `ref` (both are outside the hash server-side).
    let query = flatten_query(&body);
    let url = format!(
        "{}/api/login/loginByPasswv2?{query}&ref={}",
        s.domains.auth,
        urlencode("https://chat.zalo.me")
    );

    let resp = s.post_form(&url, String::new()).await?;
    let resp = s.maybe_decrypt(resp, cipher.key().as_bytes()).await;

    // Surface an error code hidden inside the encrypted envelope.
    let code = resp.get("error_code").and_then(|c| c.as_i64()).unwrap_or(0);
    if code != 0 {
        let msg = resp.get("error_message").and_then(|m| m.as_str()).unwrap_or("");
        anyhow::bail!("login error {code}: {msg}");
    }

    if let Some(cfg) = resp.get("data") {
        apply_login_config(s, cfg);
    }
    Ok(resp)
}

/// `_constructUrlParams(obj)` — flatten a JSON object into `k=v&k=v`.
fn flatten_query(v: &Value) -> String {
    let Some(m) = v.as_object() else {
        return String::new();
    };
    m.iter()
        .map(|(k, val)| {
            let sv = match val {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            format!("{}={}", k, urlencode(&sv))
        })
        .collect::<Vec<_>>()
        .join("&")
}

/// `GET /api/login/getLoginInfo` — returns session keys (dkey, zpw_enk, endpoints).
pub async fn get_login_info(s: &Session) -> Result<Value> {
    let client_time = unix_now();
    let payload = obj(&[
        ("imei", json!(s.imei)),
        ("computer_name", json!(s.computer_name)),
        ("language", json!("vi")),
        ("ts", json!(client_time)),
    ]);
    let cipher = ParamCipher::new(ZPW_TYPE, &s.imei, client_time)?;
    let body = cipher.build_body(ZPW_TYPE, ZPW_VER, &payload, "getlogininfo")?;
    let query = flatten_query(&body);

    // Try both auth host (wpa.zaloapp.com, where PC session cookies live)
    // and wpa_chat (wpa.chat.zalo.me, where web session cookies live).
    let hosts = [&s.domains.auth, &s.domains.wpa_chat];
    let mut last_err = anyhow::anyhow!("get_login_info failed on all hosts");

    for host in hosts {
        let url = format!("{host}/api/login/getLoginInfo?{query}&nretry=0");
        match s.get(&url).await {
            Ok(resp) => {
                let decrypted = s.maybe_decrypt(resp, cipher.key().as_bytes()).await;
                let code = decrypted.get("error_code").and_then(|c| c.as_i64()).unwrap_or(0);
                if code == 0 {
                    return Ok(decrypted);
                }
            }
            Err(e) => {
                last_err = e;
            }
        }
    }
    Err(last_err)
}

/// Pull `dkey` / `zpw_enk` / domains out of a login response payload.
///
/// Pull `dkey` / `zpw_enk` / domains out of a login response payload.
/// Preserves already established session credentials if the new payload only contains partial fields.
pub fn apply_login_config(s: &mut Session, cfg: &Value) {
    let new_dkey = cfg
        .get("uid")
        .or_else(|| cfg.get("userId"))
        .or_else(|| cfg.get("account").and_then(|a| a.get("userId").or_else(|| a.get("uid"))))
        .or_else(|| cfg.get("dkey"))
        .and_then(as_str)
        .unwrap_or_default();
    let new_phone = cfg
        .get("phone_number")
        .or_else(|| cfg.get("phone"))
        .or_else(|| {
            cfg.get("account")
                .and_then(|a| a.get("phone_number").or_else(|| a.get("phone")))
        })
        .and_then(as_str)
        .unwrap_or_default();

    let new_enk = cfg.get("zpw_enk").and_then(as_str).unwrap_or_default();
    let new_send2me_id = cfg.get("send2me_id").and_then(as_str).unwrap_or_default();

    let new_sek = cfg
        .get("zpw_sek")
        .or_else(|| cfg.get("account").and_then(|a| a.get("zpw_sek")))
        .and_then(as_str)
        .unwrap_or_default();

    let new_name = cfg
        .get("account")
        .and_then(|a| a.get("display_name"))
        .or_else(|| cfg.get("display_name"))
        .or_else(|| cfg.get("settings").and_then(|st| st.get("profile")).and_then(|p| p.get("name")))
        .and_then(as_str)
        .unwrap_or_default();

    if !new_enk.is_empty() {
        let raw = crate::crypto::parse_secret_key(&new_enk);
        s.secret_key = Some(raw);
    }
    if !new_sek.is_empty() {
        s.set_zpw_sek(new_sek.clone());
    }

    let mut current = s.login.take().unwrap_or_default();
    if !new_dkey.is_empty() { current.uin = new_dkey.to_string(); }
    if !new_enk.is_empty() { current.zpw_enk = new_enk.to_string(); }
    if !new_sek.is_empty() { current.zpw_sek = new_sek.to_string(); }
    else if current.zpw_sek.is_empty() {
        if let Some(ref sek) = s.zpw_sek {
            current.zpw_sek = sek.clone();
        }
    }
    if !new_name.is_empty() { current.display_name = new_name.to_string(); }
    if !new_phone.is_empty() { current.phone = new_phone; }
    if !new_send2me_id.is_empty() { current.send2me_id = new_send2me_id.to_string(); }

    s.login = Some(current);

    // Domain overrides: `zpw_chts_new` and `zpw_ctrs_new` are long-polling hosts,
    // while `zpw_service_map_v3` holds the REST API domains.
    s.apply_server_info(cfg);
    // A real login response means the session is fresh: drop the "refused" marker so it
    // cannot leak into the new `session.json` (#21).
    s.clear_expired();
    if let Some(v) = cfg.get("zpw_chts_new").and_then(as_str) {
        if !v.is_empty() {
            s.domains.lp_chat = normalize_host(&v);
        }
    }
    if let Some(v) = cfg.get("zpw_ctrs_new").and_then(as_str) {
        if !v.is_empty() {
            s.domains.lp_control = normalize_host(&v);
        }
    }
}

fn normalize_host(v: &str) -> String {
    if v.starts_with("http") {
        v.trim_end_matches('/').to_string()
    } else {
        format!("https://{}", v.trim_end_matches('/'))
    }
}

fn as_str(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

pub fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_login_config_captures_zpw_sek() {
        let mut s = Session::new(false, false, false).unwrap();
        let cfg = serde_json::json!({
            "dkey": "12345",
            "zpw_enk": "dGVzdF9lbmtfMzJfYnl0ZXNfbG9uZ19hZXNfa2V5IQ==",
            "zpw_sek": "sek_abc_123",
            "display_name": "Test User"
        });
        apply_login_config(&mut s, &cfg);
        assert_eq!(s.zpw_sek.as_deref(), Some("sek_abc_123"));
        let login = s.login.as_ref().unwrap();
        assert_eq!(login.zpw_sek, "sek_abc_123");
        assert_eq!(login.uin, "12345");
    }

    #[test]
    fn apply_login_config_clears_the_expired_marker() {
        let mut s = Session::new(false, false, false).unwrap();
        s.mark_expired();
        assert!(s.is_expired_marked());

        apply_login_config(
            &mut s,
            &serde_json::json!({
                "dkey": "12345",
                "zpw_enk": "dGVzdF9lbmtfMzJfYnl0ZXNfbG9uZ19hZXNfa2V5IQ==",
                "zpw_sek": "sek_after_relogin",
            }),
        );

        // Keeping the marker would pin the next start to the login screen (#21).
        assert!(!s.is_expired_marked());
        assert_eq!(s.zpw_sek.as_deref(), Some("sek_after_relogin"));
    }

    #[test]
    fn apply_login_config_captures_nested_account_zpw_sek() {
        let mut s = Session::new(false, false, false).unwrap();
        let cfg = serde_json::json!({
            "dkey": "12345",
            "zpw_enk": "dGVzdF9lbmtfMzJfYnl0ZXNfbG9uZ19hZXNfa2V5IQ==",
            "account": {
                "zpw_sek": "sek_nested_456",
                "display_name": "Nested User"
            }
        });
        apply_login_config(&mut s, &cfg);
        assert_eq!(s.zpw_sek.as_deref(), Some("sek_nested_456"));
        let login = s.login.as_ref().unwrap();
        assert_eq!(login.zpw_sek, "sek_nested_456");
    }
}
