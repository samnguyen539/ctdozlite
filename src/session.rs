//! HTTP session: cookie jar, domain table, common params, request encoding.
//!
//! Mirrors the `_get` / `_post` / `_getCommonParams` / `getEncodedParams` helpers from
//! `compact-app-pc-*.js`. See `re-notes/02_chat_data.md` §2–§4.

use std::sync::Arc;

use anyhow::{bail, Result};
use reqwest::cookie::Jar;
use reqwest::header::{HeaderMap, HeaderValue, ORIGIN, REFERER};
use reqwest::{Client, Method};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::crypto;

pub const ZPW_VER: &str = "694";
pub const ZPW_TYPE: &str = "30";
pub const CONFIG_DOMAIN: &str = "zaloapp.com";

/// Domain table.
///
/// [O] **Live-verified** against a QR-authenticated Zalo Web session on 2026-10-21.
/// Every host below answered `error_code 0` for its endpoint. Note these are
/// `*.chat.zalo.me`, not the `*.zaloapp.com` names the PC bundle defaults to — the two
/// flavours differ, and using the wrong set yields nginx 404s.
///
/// The hosts that *are* in [`crate::endpoints::SERVICE_MAP_V3`] are authoritative:
/// [`Domains::heal_from_verified`] restores them on every load and
/// [`Session::apply_server_info`] refuses to overwrite them, because the server still
/// hands out the stale `*.zaloapp.com` flavour for `friend`/`profile` (#22).
///
/// Only the hosts with no verified entry — `lp_chat`, `lp_control`, `ws`,
/// `cloud_msg` — are taken from the login payload on each run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Domains {
    /// `wpa.zaloapp.com` — `reqqr`, password login.
    pub auth: String,
    /// `wpa.chat.zalo.me` — `getServerInfo` / `getLoginInfo` once a session exists.
    pub wpa_chat: String,
    /// `zpw_service_map_v3.chat[0]` — message send (`tt-chat*-wpa.*`).
    pub chat: String,
    /// `zpw_service_map_v3.conversation[0]` — conversation control (pins, preload).
    pub conversation: String,
    /// `zpw_chts_new[0]` — HTTP long-polling message receiver (`getChatFromPolling`, `p*-msg-chat.zaloapp.com`).
    #[serde(default = "default_lp_chat")]
    pub lp_chat: String,
    /// `zpw_ctrs_new[0]` — HTTP long-polling control receiver (`getControllFromPolling`, `p*-ctl-chat.zaloapp.com`).
    #[serde(default = "default_lp_control")]
    pub lp_control: String,
    /// `zpw_service_map_v3.friend[0]` — `social/friend/getfriends`.
    pub friend: String,
    /// `zpw_service_map_v3.conversation[0]` — `conv/*`, `hiddenconvers/*`.
    pub convers_api: String,
    /// `zpw_service_map_v3.profile[0]` — `getprofiles`, `me-v2`.
    pub profile: String,
    pub group: String,
    pub file: String,
    /// `zpw_service_map_v3.group_cloud_message[0]` — `/api/cm/getoldv2`, `/api/cm/getrecentv2`.
    #[serde(default = "default_cloud_msg")]
    pub cloud_msg: String,
    /// `zpw_service_map_v3.voice_call[0]` — `/api/voicecall/*` signaling endpoints.
    #[serde(default = "default_voice_call")]
    pub voice_call: String,
    /// `zpw_service_map_v3.sticker[0]` — `/api/message/sticker/*` catalog & search endpoints.
    #[serde(default = "default_sticker")]
    pub sticker: String,
    /// `zpw_ws[...]` — control websocket endpoints (`wss://wsN-msg.chat.zalo.me`).
    ///
    /// [O] The server hands back full `wss://` URLs in the login config; the client never
    /// builds them from a literal. Real-time VoIP invites only arrive over this transport.
    #[serde(default = "default_ws")]
    pub ws: Vec<String>,
    /// Candidate hosts returned by `zpw_service_map_v3` for each endpoint (#27),
    /// kept in-memory as secondary candidates after the verified constant.
    #[serde(skip)]
    pub service_map_candidates: std::collections::BTreeMap<String, Vec<String>>,
    /// Candidate hosts read from `session.json` before `heal_from_verified` replaced
    /// the primary slots (#27), kept as tertiary candidates.
    #[serde(skip)]
    pub saved_candidates: std::collections::BTreeMap<String, String>,
    /// Winning host per endpoint remembered in-memory for the current session (#27).
    #[serde(skip)]
    pub preferred_host: Arc<std::sync::Mutex<std::collections::BTreeMap<String, String>>>,
}

fn default_lp_chat() -> String {
    "https://p4-msg-chat.zaloapp.com".into()
}

fn default_lp_control() -> String {
    "https://p4-ctl-chat.zaloapp.com".into()
}

fn default_cloud_msg() -> String {
    "https://tt-group-cm.api.zaloapp.com".into()
}

fn default_voice_call() -> String {
    "https://voicecall-wpa.chat.zalo.me".into()
}

fn default_sticker() -> String {
    crate::endpoints::service("sticker")
        .unwrap_or("https://tt-sticker-wpa.chat.zalo.me")
        .into()
}

/// [O] `re-notes/02_chat_data.md`: `zpw_ws` = `wss://ws{1..8}-msg.chat.zalo.me`.
fn default_ws() -> Vec<String> {
    (1..=8)
        .map(|n| format!("wss://ws{n}-msg.chat.zalo.me"))
        .collect()
}

impl Default for Domains {
    fn default() -> Self {
        // `chat` / `conversation` are seeded with the two long-polling hosts the client
        // has observed; `heal_from_verified` then rewrites **every** verified slot, so
        // those seeds only survive if `SERVICE_MAP_V3` ever loses the key.
        let mut domains = Self {
            auth: crate::endpoints::AUTH.into(),
            wpa_chat: crate::endpoints::WPA_CHAT.into(),
            chat: crate::endpoints::MSG_CHAT.into(),
            conversation: crate::endpoints::CONVERS_CTL.into(),
            lp_chat: default_lp_chat(),
            lp_control: default_lp_control(),
            friend: "https://tt-friend-wpa.chat.zalo.me".into(),
            convers_api: "https://tt-convers-wpa.chat.zalo.me".into(),
            profile: "https://tt-profile-wpa.chat.zalo.me".into(),
            group: "https://tt-group-wpa.zaloapp.com".into(),
            file: "https://tt-files-wpa.zaloapp.com".into(),
            cloud_msg: default_cloud_msg(),
            voice_call: default_voice_call(),
            sticker: default_sticker(),
            ws: default_ws(),
            service_map_candidates: std::collections::BTreeMap::new(),
            saved_candidates: std::collections::BTreeMap::new(),
            preferred_host: Arc::new(std::sync::Mutex::new(std::collections::BTreeMap::new())),
        };
        domains.heal_from_verified();
        domains
    }
}

/// First entry of a `zpw_service_map_v3` value, which can be either an array or a string.
fn first_url(v: &Value) -> Option<String> {
    all_urls(v).into_iter().next()
}

/// All normalized URLs in a `zpw_service_map_v3` entry (array or string).
fn all_urls(v: &Value) -> Vec<String> {
    if let Some(arr) = v.as_array() {
        arr.iter()
            .filter_map(|x| x.as_str())
            .map(|s| s.trim_end_matches('/').to_string())
            .filter(|s| !s.is_empty())
            .collect()
    } else if let Some(s) = v.as_str() {
        let trimmed = s.trim_end_matches('/').to_string();
        if trimmed.is_empty() {
            Vec::new()
        } else {
            vec![trimmed]
        }
    } else {
        Vec::new()
    }
}

fn push_unique_host(out: &mut Vec<String>, candidate: &str) {
    let trimmed = candidate.trim().trim_end_matches('/');
    if !trimmed.is_empty() && !out.iter().any(|existing| existing == trimmed) {
        out.push(trimmed.to_string());
    }
}

impl Domains {
    /// Restore every endpoint that has a **verified** entry in
    /// [`crate::endpoints::SERVICE_MAP_V3`], overriding whatever a previous run
    /// persisted while keeping the previous host in `saved_candidates` (#27).
    pub fn heal_from_verified(&mut self) {
        use crate::endpoints::service;

        if let Some(v) = service("chat") {
            let prev = self.chat.trim_end_matches('/').to_string();
            if !prev.is_empty() && !prev.contains("-msg-chat.") {
                self.saved_candidates.insert("chat".into(), prev);
            }
            self.chat = v.into();
        }
        if let Some(v) = service("conversation") {
            let prev = self.convers_api.trim_end_matches('/').to_string();
            if !prev.is_empty() && !prev.contains("-ctl-chat.") {
                self.saved_candidates.insert("conversation".into(), prev);
            }
            self.conversation = v.into();
            self.convers_api = v.into();
        }
        for (key, slot) in [
            ("friend", &mut self.friend),
            ("profile", &mut self.profile),
            ("group", &mut self.group),
            ("file", &mut self.file),
            ("voice_call", &mut self.voice_call),
            ("sticker", &mut self.sticker),
        ] {
            if let Some(v) = service(key) {
                let prev = slot.trim_end_matches('/').to_string();
                if !prev.is_empty() {
                    self.saved_candidates.insert(key.into(), prev);
                }
                *slot = v.into();
            }
        }
    }

    /// Apply `zpw_service_map_v3` **without** overriding verified primary slots,
    /// while recording all server-returned hosts in `service_map_candidates` (#27).
    pub fn apply_service_map(&mut self, map: &Value) {
        for (key, slot) in [
            ("chat", &mut self.chat),
            ("friend", &mut self.friend),
            ("conversation", &mut self.convers_api),
            ("profile", &mut self.profile),
            ("group", &mut self.group),
            ("file", &mut self.file),
            ("group_cloud_message", &mut self.cloud_msg),
            ("voice_call", &mut self.voice_call),
            ("sticker", &mut self.sticker),
        ] {
            let urls = map.get(key).map(all_urls).unwrap_or_default();
            if !urls.is_empty() {
                self.service_map_candidates.insert(key.into(), urls.clone());
            }
            let Some(v) = urls.into_iter().next() else {
                continue;
            };
            let verified = crate::endpoints::service(key).is_some();
            if !verified || slot.is_empty() {
                *slot = v;
            }
        }
        if crate::endpoints::service("conversation").is_none() {
            if let Some(v) = map.get("conversation").and_then(first_url) {
                self.conversation = v;
            }
        }
    }

    /// Primary slot value for a named endpoint key.
    fn slot_for(&self, endpoint: &str) -> &str {
        match endpoint {
            "chat" => &self.chat,
            "friend" => &self.friend,
            "conversation" | "convers_api" => &self.convers_api,
            "profile" => &self.profile,
            "group" => &self.group,
            "file" => &self.file,
            "group_cloud_message" | "cloud_msg" => &self.cloud_msg,
            "voice_call" => &self.voice_call,
            "sticker" => &self.sticker,
            "auth" => &self.auth,
            "wpa_chat" => &self.wpa_chat,
            _ => "",
        }
    }

    /// Ordered candidate hosts for `endpoint` (#27):
    /// 1. Winning host remembered in-memory during this session (if any)
    /// 2. Verified constant from [`crate::endpoints::SERVICE_MAP_V3`] (or primary slot)
    /// 3. Hosts returned by `zpw_service_map_v3`
    /// 4. Host loaded from `session.json` before healing
    /// 5. For `friend`, fallback to `profile` host if distinct
    pub fn candidates(&self, endpoint: &str) -> Vec<String> {
        let canonical = if endpoint == "convers_api" {
            "conversation"
        } else if endpoint == "cloud_msg" {
            "group_cloud_message"
        } else {
            endpoint
        };
        let mut list = Vec::new();
        if let Ok(pref) = self.preferred_host.lock() {
            if let Some(winner) = pref.get(canonical) {
                push_unique_host(&mut list, winner);
            }
        }
        push_unique_host(&mut list, self.slot_for(canonical));
        if let Some(v) = crate::endpoints::service(canonical) {
            push_unique_host(&mut list, v);
        }
        if let Some(mapped) = self.service_map_candidates.get(canonical) {
            for u in mapped {
                push_unique_host(&mut list, u);
            }
        }
        if let Some(saved) = self.saved_candidates.get(canonical) {
            push_unique_host(&mut list, saved);
        }
        if canonical == "friend" {
            push_unique_host(&mut list, &self.profile);
        }
        list
    }

    /// Remember the winning host for `endpoint` for the remainder of the session (#27).
    pub fn remember_winner(&self, endpoint: &str, host: &str) {
        let canonical = if endpoint == "convers_api" {
            "conversation"
        } else if endpoint == "cloud_msg" {
            "group_cloud_message"
        } else {
            endpoint
        };
        let trimmed = host.trim().trim_end_matches('/');
        if !trimmed.is_empty() {
            if let Ok(mut pref) = self.preferred_host.lock() {
                pref.insert(canonical.to_string(), trimmed.to_string());
            }
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LoginInfo {
    pub uin: String,
    /// `zpw_enk` — base64 AES-256 key for legacy param encryption.
    pub zpw_enk: String,
    /// `zpw_sek` — session encryption/cookie token for authenticated endpoints.
    pub zpw_sek: String,
    pub display_name: String,
    pub send2me_id: String,
    #[serde(default)]
    pub avatar: String,
    #[serde(default)]
    pub phone: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavedSession {
    pub imei: String,
    pub computer_name: String,
    pub secret_key_b64: Option<String>,
    pub session_key: Option<String>,
    pub zpw_sek: Option<String>,
    pub login: Option<LoginInfo>,
    pub domains: Domains,
    /// Epoch seconds at which the server last refused these credentials (#21).
    ///
    /// Written **without touching the rest of the file**, so the old session survives
    /// until a fresh login replaces it. Absent in files from older builds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expired_at: Option<i64>,
}

#[derive(Clone)]
pub struct Session {
    pub http: Client,
    pub jar: Arc<Jar>,
    pub domains: Domains,
    pub imei: String,
    pub computer_name: String,
    pub secret_key: Option<Vec<u8>>,
    /// Same key as a string, for `maybe_decrypt`.
    pub session_key: Option<String>,
    pub zpw_sek: Option<String>,
    pub login: Option<LoginInfo>,
    /// Draw commands in front of the user so they can be reproduced by hand.
    pub verbose: bool,
    /// Append every exchange (pretty-printed) to `dump/<n>-<slug>.json`.
    pub dump: bool,
    /// Skip URL redaction in dumps (needed to reproduce signature failures).
    pub dump_raw: bool,
    /// `Some(epoch)` once the server has refused these credentials (#21): the marker
    /// persists in `session.json` so the next start asks for a login instead of
    /// re-testing a dead session.
    pub expired_at: Option<i64>,
    /// Shared live `zpw_sek` updated whenever a response sets a new session cookie.
    pub live_sek: Arc<std::sync::RwLock<Option<String>>>,
    /// Shared live cookie map preserved across requests on `.zalo.me` and `.zaloapp.com`.
    pub live_cookies: Arc<std::sync::RwLock<std::collections::BTreeMap<String, String>>>,
}

/// `error_code` values Zalo answers when the stored credentials are no longer usable.
pub const AUTH_ERROR_CODES: &[i64] = &[600, -102];

/// A non-zero `error_code` from Zalo, carrying the number instead of burying it in a
/// formatted message.
///
/// [`Session::send`] bails with this so callers can tell "the session is gone" (#21)
/// from "the network is down" without matching substrings.
#[derive(Debug)]
pub struct SessionError {
    pub code: i64,
    pub message: String,
    pub url: String,
}

impl SessionError {
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            url: String::new(),
        }
    }

    /// Attach the endpoint that answered, when the caller knows it.
    pub fn at(mut self, url: impl Into<String>) -> Self {
        self.url = url.into();
        self
    }

    /// The server rejected the credentials, not the request.
    pub fn is_auth(&self) -> bool {
        AUTH_ERROR_CODES.contains(&self.code)
    }
}

impl std::fmt::Display for SessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.url.is_empty() {
            write!(f, "server error {}: {}", self.code, self.message)
        } else {
            write!(
                f,
                "server error {}: {} (from {})",
                self.code, self.message, self.url
            )
        }
    }
}

impl std::error::Error for SessionError {}

/// What the server — or the network — last said about the stored session (#21).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionStatus {
    Valid,
    /// The server refused the credentials: re-login required.
    Expired,
    /// No verdict: transport, HTTP, unreadable body, or an unrelated `error_code`.
    /// Never destroy `session.json` for this.
    Unreachable,
}

impl SessionStatus {
    /// Only [`AUTH_ERROR_CODES`] mean the session is gone; `0` is success and anything
    /// else leaves the status undetermined.
    pub fn from_error_code(code: i64) -> Self {
        if code == 0 {
            Self::Valid
        } else if AUTH_ERROR_CODES.contains(&code) {
            Self::Expired
        } else {
            Self::Unreachable
        }
    }

    /// Classify a failed request. Only a [`SessionError`] knows its code; every other
    /// error (timeout, DNS, `HTTP 500`, parse) is [`SessionStatus::Unreachable`].
    pub fn from_error(err: &anyhow::Error) -> Self {
        match err.downcast_ref::<SessionError>() {
            Some(e) => Self::from_error_code(e.code),
            None => Self::Unreachable,
        }
    }

    pub fn is_valid(self) -> bool {
        matches!(self, Self::Valid)
    }
}

impl Session {
    pub fn new(verbose: bool, dump: bool, dump_raw: bool) -> Result<Self> {
        let jar = Arc::new(Jar::default());
        let mut default_headers = HeaderMap::new();
        default_headers.insert(
            REFERER,
            HeaderValue::from_static("https://chat.zalo.me/"),
        );
        default_headers.insert(
            ORIGIN,
            HeaderValue::from_static("https://chat.zalo.me"),
        );
        let http = Client::builder()
            .cookie_provider(Arc::clone(&jar))
            .default_headers(default_headers)
            .connect_timeout(std::time::Duration::from_secs(10))
            .timeout(std::time::Duration::from_secs(35))
            .user_agent(
                "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
                 (KHTML, like Gecko) ZaloPC/26.10.11 Chrome/122.0.0.0 Electron/29.0.0 Safari/537.36",
            )
            .build()?;
        Ok(Self {
            http,
            jar,
            domains: Domains::default(),
            imei: random_imei(),
            computer_name: std::env::var("COMPUTERNAME").unwrap_or_else(|_| "DESKTOP".into()),
            secret_key: None,
            session_key: None,
            zpw_sek: None,
            login: None,
            verbose,
            dump,
            dump_raw,
            expired_at: None,
            live_sek: Arc::new(std::sync::RwLock::new(None)),
            live_cookies: Arc::new(std::sync::RwLock::new(std::collections::BTreeMap::new())),
        })
    }

    /// Store `zpw_sek` session token, attach it to `self.login`, and insert the session cookie.
    pub fn set_zpw_sek(&mut self, v: String) {
        if v.is_empty() {
            return;
        }
        // Insert cookie for all required Zalo hosts
        let hosts = [
            "https://chat.zalo.me",
            "https://wpa.zaloapp.com",
            "https://wpa.chat.zalo.me",
            "https://tt-sticker-wpa.chat.zalo.me",
            "https://tt-sticker-wpa.zaloapp.com",
            "https://sticker-wpa.chat.zalo.me",
            &self.domains.auth,
            &self.domains.chat,
            &self.domains.conversation,
            &self.domains.lp_chat,
            &self.domains.lp_control,
            &self.domains.convers_api,
            &self.domains.profile,
            &self.domains.friend,
            &self.domains.group,
            &self.domains.cloud_msg,
            &self.domains.voice_call,
            &self.domains.sticker,
        ];
        for host in hosts {
            if let Ok(url) = reqwest::Url::parse(host) {
                let cookie = format!("zpw_sek={v}; Path=/");
                self.jar.add_cookie_str(&cookie, &url);
            }
        }
        if let Ok(mut g) = self.live_sek.write() {
            *g = Some(v.clone());
        }
        if let Ok(mut m) = self.live_cookies.write() {
            m.insert("zpw_sek".into(), v.clone());
        }
        self.zpw_sek = Some(v.clone());
        if let Some(li) = &mut self.login {
            li.zpw_sek = v;
        }
    }

    /// Return the freshest `zpw_sek` (from `Set-Cookie`, `self.zpw_sek`, or `self.login`).
    pub fn current_zpw_sek(&self) -> Option<String> {
        if let Ok(g) = self.live_sek.read() {
            if let Some(s) = &*g {
                if !s.is_empty() && s != "EXPIRED" {
                    return Some(s.clone());
                }
            }
        }
        self.zpw_sek
            .clone()
            .filter(|s| !s.is_empty() && s != "EXPIRED")
            .or_else(|| {
                self.login
                    .as_ref()
                    .map(|l| l.zpw_sek.clone())
                    .filter(|s| !s.is_empty() && s != "EXPIRED")
            })
    }

    /// Synchronize any newly captured `live_sek` back into `self.zpw_sek` and `self.login`.
    pub fn sync_live_sek(&mut self) {
        if let Some(sek) = self.current_zpw_sek() {
            self.zpw_sek = Some(sek.clone());
            if let Some(li) = &mut self.login {
                li.zpw_sek = sek;
            }
        }
    }

    /// Parse `Set-Cookie` headers from a Zalo HTTP response, ignoring deletion cookies (`1970` / empty).
    pub fn ingest_set_cookie_headers(&self, headers: &HeaderMap) {
        for val in headers.get_all("set-cookie") {
            let Ok(raw) = val.to_str() else { continue };
            let lower = raw.to_ascii_lowercase();
            if lower.contains("1970") || lower.contains("max-age=0") {
                continue;
            }
            let first = raw.split(';').next().unwrap_or("").trim();
            let Some((k, v)) = first.split_once('=') else { continue };
            let name = k.trim();
            let value = v.trim().trim_matches('"');
            if name.is_empty() || value.is_empty() || value == "EXPIRED" {
                continue;
            }
            if let Ok(mut m) = self.live_cookies.write() {
                m.insert(name.to_string(), value.to_string());
            }
            if name == "zpw_sek" {
                if let Ok(mut g) = self.live_sek.write() {
                    *g = Some(value.to_string());
                }
                for host in [
                    "https://chat.zalo.me",
                    "https://wpa.zaloapp.com",
                    "https://wpa.chat.zalo.me",
                    &self.domains.auth,
                    &self.domains.chat,
                    &self.domains.conversation,
                    &self.domains.lp_chat,
                    &self.domains.lp_control,
                    &self.domains.convers_api,
                    &self.domains.profile,
                    &self.domains.friend,
                    &self.domains.group,
                    &self.domains.cloud_msg,
                ] {
                    if let Ok(url) = reqwest::Url::parse(host) {
                        self.jar.add_cookie_str(&format!("zpw_sek={value}; Path=/"), &url);
                    }
                }
            }
        }
    }

    /// The stored session is already known to be refused by the server (#21).
    pub fn is_expired_marked(&self) -> bool {
        self.expired_at.is_some()
    }

    /// Record that the server refused these credentials. The file is rewritten with the
    /// marker but **keeps every credential**, so it can still be read if the verdict was
    /// wrong — and it is only replaced once a fresh login succeeds.
    pub fn mark_expired(&mut self) {
        self.expired_at = Some(crate::login::unix_now());
    }

    /// A fresh login succeeded: the marker must not leak into the new session.
    pub fn clear_expired(&mut self) {
        self.expired_at = None;
    }

    /// Ask the server whether the stored session still works.
    ///
    /// Only `error_code` `600` / `-102` mean the session is gone. Transport failures,
    /// HTTP errors, unreadable bodies and unrelated codes are
    /// [`SessionStatus::Unreachable`] — the caller must not touch `session.json` for
    /// those (#21). A session with no credentials has nothing to validate and reports
    /// [`SessionStatus::Expired`].
    pub async fn session_status(&self) -> SessionStatus {
        if self.login.is_none() || self.zpw_sek.is_none() {
            return SessionStatus::Expired;
        }
        let body = serde_json::json!({
            "avatar_size": 160,
            "imei": self.imei,
        });
        let url = match self.encoded_params(&body) {
            Ok(enc) => format!(
                "{}/api/social/profile/me-v2?{}&params={enc}&os=8",
                self.domains.profile,
                self.common_params()
            ),
            Err(_) => format!(
                "{}/api/social/profile/me-v2?{}",
                self.domains.profile,
                self.common_params()
            ),
        };
        let resp = match self.get(&url).await {
            Ok(r) => r,
            Err(e) => return SessionStatus::from_error(&e),
        };
        // `me-v2` answers param-encrypted requests with a base64 body, so decode before
        // reading the code — otherwise a live session looks like a malformed response.
        let dec = self.maybe_decrypt(resp, self.session_key_bytes()).await;
        match dec.get("error_code").and_then(serde_json::Value::as_i64) {
            Some(code) => SessionStatus::from_error_code(code),
            None => SessionStatus::Unreachable,
        }
    }

    /// Fetch the authenticated user's own numeric `userId` and `displayName` from `/api/social/profile/me-v2`.
    pub async fn sync_profile_me(&mut self) -> Option<(String, String)> {
        if self.login.is_none() || self.zpw_sek.is_none() {
            return None;
        }
        let body = serde_json::json!({
            "avatar_size": 160,
            "imei": self.imei,
        });
        let enc = self.encoded_params(&body).ok()?;
        let url = format!(
            "{}/api/social/profile/me-v2?{}&params={enc}&os=8",
            self.domains.profile,
            self.common_params()
        );
        let resp = self.get(&url).await.ok()?;
        let dec = self.maybe_decrypt(resp, self.session_key_bytes()).await;
        let profile = dec
            .get("data")
            .and_then(|d| d.get("profile"))
            .or_else(|| dec.get("profile"))?;
        let uid = profile
            .get("userId")
            .or_else(|| profile.get("uid"))
            .and_then(|v| match v {
                Value::String(s) if !s.is_empty() => Some(s.clone()),
                Value::Number(n) => Some(n.to_string()),
                _ => None,
            })?;
        let phone = profile
            .get("phoneNumber")
            .or_else(|| profile.get("phone"))
            .or_else(|| profile.get("phone_number"))
            .and_then(|v| match v {
                Value::String(s) => Some(s.clone()),
                Value::Number(n) => Some(n.to_string()),
                _ => None,
            })
            .unwrap_or_default();
        let name = profile
            .get("displayName")
            .or_else(|| profile.get("zaloName"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let avatar = profile
            .get("avatar")
            .or_else(|| profile.get("avt"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if let Some(ref mut li) = self.login {
            li.uin = uid.clone();
            if !name.is_empty() {
                li.display_name = name.clone();
            }
            if !phone.is_empty() {
                li.phone = phone;
            }
            if !avatar.is_empty() {
                li.avatar = avatar;
            }
        }
        Some((uid, name))
    }

    /// Clear all session credentials, tokens, keys, and cookies.
    pub fn logout(&mut self) {
        self.login = None;
        self.zpw_sek = None;
        self.secret_key = None;
        self.session_key = None;
        if let Ok(mut g) = self.live_sek.write() {
            *g = None;
        }
        if let Ok(mut m) = self.live_cookies.write() {
            m.clear();
        }
        self.jar = Arc::new(Jar::default());
        let mut default_headers = HeaderMap::new();
        default_headers.insert(
            REFERER,
            HeaderValue::from_static("https://chat.zalo.me/"),
        );
        default_headers.insert(
            ORIGIN,
            HeaderValue::from_static("https://chat.zalo.me"),
        );
        if let Ok(c) = Client::builder()
            .cookie_provider(Arc::clone(&self.jar))
            .default_headers(default_headers)
            .connect_timeout(std::time::Duration::from_secs(10))
            .timeout(std::time::Duration::from_secs(35))
            .user_agent(
                "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
                 (KHTML, like Gecko) ZaloPC/26.10.11 Chrome/122.0.0.0 Electron/29.0.0 Safari/537.36",
            )
            .build()
        {
            self.http = c;
        }
    }

    /// Save the authenticated session to disk so subsequent runs can restore it without re-authenticating.
    pub fn save_to_file(&self, path: &str) -> Result<()> {
        use base64::Engine;
        let latest_sek = self.current_zpw_sek();
        let mut saved_login = self.login.clone();
        if let (Some(li), Some(sek)) = (&mut saved_login, &latest_sek) {
            li.zpw_sek = sek.clone();
        }
        let saved = SavedSession {
            imei: self.imei.clone(),
            computer_name: self.computer_name.clone(),
            secret_key_b64: self.secret_key.as_ref().map(|k| base64::engine::general_purpose::STANDARD.encode(k)),
            session_key: self.session_key.clone(),
            zpw_sek: latest_sek,
            login: saved_login,
            domains: self.domains.clone(),
            expired_at: self.expired_at,
        };
        let text = serde_json::to_string_pretty(&saved)?;
        crate::data_dir::write_private_atomic(std::path::Path::new(path), text.as_bytes())?;
        Ok(())
    }

    /// Load a previously saved session from disk and restore cookies, keys, and endpoints.
    pub fn load_from_file(&mut self, path: &str) -> Result<()> {
        use base64::Engine;
        let text = std::fs::read_to_string(path)?;
        let saved: SavedSession = serde_json::from_str(&text)?;

        self.imei = if saved.imei.len() == 69 {
            saved.imei
        } else {
            random_imei()
        };
        self.computer_name = saved.computer_name;
        self.domains = saved.domains;
        // Older sessions stored the long-polling hosts in `chat` / `conversation`
        // when `zpw_chts_new` / `zpw_ctrs_new` overwrote the REST domains. Rescind
        // them before the verified baseline replaces the REST fields.
        if self.domains.chat.contains("-msg-chat.") && self.domains.lp_chat.is_empty() {
            self.domains.lp_chat = self.domains.chat.clone();
        }
        if self.domains.conversation.contains("-ctl-chat.") && self.domains.lp_control.is_empty() {
            self.domains.lp_control = self.domains.conversation.clone();
        }
        // Verified endpoints win over anything a previous run persisted (#22).
        self.domains.heal_from_verified();
        // Server-owned endpoints are never in the verified table, so fill gaps.
        if self.domains.lp_chat.is_empty() {
            self.domains.lp_chat = default_lp_chat();
        }
        if self.domains.lp_control.is_empty() {
            self.domains.lp_control = default_lp_control();
        }
        if self.domains.ws.is_empty() {
            self.domains.ws = default_ws();
        }
        self.session_key = saved.session_key;
        self.expired_at = saved.expired_at;
        let sek = saved
            .zpw_sek
            .filter(|s| !s.is_empty())
            .or_else(|| saved.login.as_ref().map(|l| l.zpw_sek.clone()))
            .filter(|s| !s.is_empty());

        if let Some(b64) = saved.secret_key_b64 {
            if let Ok(raw) = base64::engine::general_purpose::STANDARD.decode(b64) {
                self.secret_key = Some(raw);
            }
        }

        self.login = saved.login;

        if let Some(s) = sek {
            self.set_zpw_sek(s);
        } else {
            self.login = None;
            anyhow::bail!("session.json thiếu mã xác thực zpw_sek");
        }

        Ok(())
    }

    /// Persist one request/response pair for later schema mining.
    ///
    /// Credentials are redacted; the point is the response *shape*, not the content.
    fn dump_exchange(&self, method: &str, url: &str, status: u16, body: &str) {
        let dir = std::path::Path::new("dump");
        if let Err(e) = std::fs::create_dir_all(dir) {
            eprintln!("! dump dir: {e}");
            return;
        }
        let n = std::fs::read_dir(dir).map(|d| d.count()).unwrap_or(0) + 1;
        // Name the file from the path only; the query carries ids we should not keep.
        let path_part = url.split('?').next().unwrap_or(url);
        let slug: String = path_part
            .split("/api/")
            .nth(1)
            .unwrap_or("root")
            .split('/')
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("-");
        let path = dir.join(format!("{n:02}-{slug}.json"));

        // Parse to a real JSON object so the dump nests properly instead of holding a
        // double-encoded string.
        let body_json = serde_json::from_str::<Value>(body)
            .unwrap_or_else(|_| Value::String(body.to_string()));

        let doc = serde_json::json!({
            "method": method,
            // `dump_raw` keeps the full URL so a signature mismatch can be reproduced
            // offline. Only use it on a throwaway account.
            "url": if self.dump_raw { url.to_string() } else { redact_url(url) },
            "status": status,
            "body": body_json,
        });
        let text = serde_json::to_string_pretty(&doc).unwrap_or_default();
        if let Err(e) = std::fs::write(&path, text) {
            eprintln!("! dump write: {e}");
        } else {
            eprintln!("  dumped → {}", path.display());
        }
    }

    /// `_constructUrlParams(se)` → `zpw_ver=693&zpw_type=30`. That is the whole object.
    pub fn common_params(&self) -> String {
        format!("zpw_ver={ZPW_VER}&zpw_type={ZPW_TYPE}")
    }

    /// The raw session AES key as bytes, for response decryption.
    pub fn session_key_bytes(&self) -> &[u8] {
        self.secret_key.as_deref().unwrap_or(&[])
    }

    /// `getEncodedParams(obj)` → url-encoded `encodeAES(json)`.
    ///
    /// Requires the session key; before login there is nothing to encrypt, so callers
    /// that run pre-auth use plain params instead.
    pub fn encoded_params(&self, v: &Value) -> Result<String> {
        let key = self
            .secret_key
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("no session key yet (pre-login)"))?;
        let b64 = crypto::encode_aes(key, &v.to_string())?;
        Ok(urlencode(&b64))
    }

    /// Plain `params=<url-encoded json>` — accepted by the server when param encryption
    /// is off, which is the MVP path.
    pub fn plain_params(v: &Value) -> String {
        format!("params={}", urlencode(&v.to_string()))
    }

    /// Re-apply the endpoint table from a `getServerInfo` / `getLoginInfo` payload.
    ///
    /// [O] `setEndpoints(chts, ctrs, serviceMapNew, conf, ws)`:
    /// `zpw_chts_new` → chat, `zpw_ctrs_new` → conversation,
    /// `zpw_service_map_new.{friend,conversation,profile,group,file}` → the rest.
    pub fn apply_server_info(&mut self, data: &Value) {
        if let Some(v) = data.get("zpw_chts_new").and_then(first_url) {
            self.domains.lp_chat = v;
        }
        if let Some(v) = data.get("zpw_ctrs_new").and_then(first_url) {
            self.domains.lp_control = v;
        }
        if let Some(arr) = data.get("zpw_ws").and_then(|v| v.as_array()) {
            let list: Vec<String> = arr
                .iter()
                .filter_map(|x| x.as_str())
                .map(|s| s.trim_end_matches('/').to_string())
                .filter(|s| s.starts_with("wss://") || s.starts_with("ws://"))
                .collect();
            if !list.is_empty() {
                self.domains.ws = list;
            }
        }
        if let Some(map) = data
            .get("zpw_service_map_v3")
            .or_else(|| data.get("zpw_service_map_new"))
        {
            self.domains.apply_service_map(map);
        }
    }

    pub async fn get(&self, url: &str) -> Result<Value> {
        self.send(Method::GET, url, None).await
    }

    pub async fn post_form(&self, url: &str, body: String) -> Result<Value> {
        self.send(Method::POST, url, Some(body)).await
    }

    /// True only when an error is a transport failure or HTTP 404 (#27).
    /// Business / auth errors (`SessionError` with `600`, `-102`, etc.) never failover.
    pub fn should_failover_candidate(err: &anyhow::Error) -> bool {
        if err.downcast_ref::<SessionError>().is_some() {
            return false;
        }
        if err.downcast_ref::<reqwest::Error>().is_some() {
            return true;
        }
        let msg = err.to_string();
        msg.starts_with("HTTP 404")
            || msg.contains("HTTP 404 ")
            || msg.contains("connection refused")
            || msg.contains("timed out")
            || msg.contains("dns error")
    }

    /// Try each candidate host for `endpoint` in priority order until one succeeds (#27).
    ///
    /// Remembers the winning host in-memory for subsequent requests in the same session,
    /// without mutating the `session.json` format. Stops immediately on `SessionError`
    /// (`600`, `-102`, or any other non-zero `error_code`).
    pub async fn request_with_candidates<F, Fut>(
        &self,
        endpoint: &str,
        path_and_query: &str,
        mut send_fn: F,
    ) -> Result<Value>
    where
        F: FnMut(String) -> Fut,
        Fut: std::future::Future<Output = Result<Value>>,
    {
        let candidates = self.domains.candidates(endpoint);
        let suffix = if path_and_query.starts_with('/') {
            path_and_query.to_string()
        } else {
            format!("/{path_and_query}")
        };
        let mut last_err: Option<anyhow::Error> = None;
        for host in candidates {
            let url = format!("{host}{suffix}");
            match send_fn(url).await {
                Ok(val) => {
                    self.domains.remember_winner(endpoint, &host);
                    return Ok(val);
                }
                Err(e) => {
                    if !Self::should_failover_candidate(&e) {
                        return Err(e);
                    }
                    crate::logger::log_error(
                        &format!("session::failover::{endpoint}"),
                        &format!("host {host} failed ({e}), trying next candidate"),
                    );
                    last_err = Some(e);
                }
            }
        }
        Err(last_err.unwrap_or_else(|| anyhow::anyhow!("no host candidates for endpoint {endpoint}")))
    }

    /// Perform a `GET` request against `endpoint` with multi-candidate host failover (#27).
    pub async fn get_endpoint(&self, endpoint: &str, path_and_query: &str) -> Result<Value> {
        self.request_with_candidates(endpoint, path_and_query, |url| async move {
            self.get(&url).await
        })
        .await
    }

    /// Perform a `POST` form request against `endpoint` with multi-candidate host failover (#27).
    pub async fn post_endpoint_form(
        &self,
        endpoint: &str,
        path_and_query: &str,
        body: String,
    ) -> Result<Value> {
        self.request_with_candidates(endpoint, path_and_query, |url| {
            let b = body.clone();
            async move { self.post_form(&url, b).await }
        })
        .await
    }

    /// Send an RFC 7578 `multipart/form-data` binary upload (`chunkContent` or `fileContent`)
    /// to Zalo's file/photo servers.
    pub async fn post_multipart(
        &self,
        url: &str,
        field_name: &str,
        file_name: &str,
        mime_type: &str,
        file_bytes: &[u8],
    ) -> Result<Value> {
        let boundary = format!(
            "----WebKitFormBoundary{}",
            crate::param_cipher::random_hex(16)
        );
        let safe_name = file_name.replace(['"', '\r', '\n'], "_");
        let head = format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"{field_name}\"; filename=\"{safe_name}\"\r\nContent-Type: {mime_type}\r\n\r\n"
        );
        let tail = format!("\r\n--{boundary}--\r\n");
        let mut body = Vec::with_capacity(head.len() + file_bytes.len() + tail.len());
        body.extend_from_slice(head.as_bytes());
        body.extend_from_slice(file_bytes);
        body.extend_from_slice(tail.as_bytes());

        let start = std::time::Instant::now();
        let mut rb = self
            .http
            .request(Method::POST, url)
            .timeout(std::time::Duration::from_secs(90))
            .header(
                "Content-Type",
                format!("multipart/form-data; boundary={boundary}"),
            );
        if let Some(sek) = &self.zpw_sek {
            if !sek.is_empty() {
                rb = rb
                    .header("Cookie", format!("zpw_sek={sek}; _zlang=vi"))
                    .header("zpw_sek", sek)
                    .header("Origin", "https://chat.zalo.me")
                    .header("Referer", "https://chat.zalo.me/");
            }
        }
        let resp = rb.body(body).send().await?;
        let latency_ms = start.elapsed().as_millis();
        let status = resp.status();
        let text = resp.text().await?;
        crate::logger::log_request(
            "POST(multipart)",
            url,
            Some(status.as_u16()),
            latency_ms,
            &format!("[{field_name}: {safe_name} ({} bytes)]", file_bytes.len()),
            Some(&text),
            if status.is_success() {
                None
            } else {
                Some("HTTP multipart error")
            },
        );
        if !status.is_success() {
            anyhow::bail!("HTTP {status} from {url}: {}", truncate(&text, 300));
        }
        if text.trim().is_empty() {
            return Ok(serde_json::json!({ "error_code": 0, "data": null }));
        }
        let v: Value = serde_json::from_str(&text)
            .unwrap_or_else(|_| serde_json::json!({ "data": text.trim() }));
        Ok(v)
    }

    async fn send(&self, m: Method, url: &str, body: Option<String>) -> Result<Value> {
        let start = std::time::Instant::now();
        if self.verbose {
            eprintln!("→ {} {}", m, if self.dump_raw { url.to_string() } else { redact_url(url) });
            if let Some(b) = &body {
                eprintln!("  body: {}", truncate(b, 400));
            }
        }
        let method = m.as_str().to_string();
        let mut rb = self.http.request(m, url);
        let is_zalo_host = reqwest::Url::parse(url)
            .map(|u| {
                u.host_str()
                    .map(|h| {
                        h == "zalo.me"
                            || h.ends_with(".zalo.me")
                            || h == "zaloapp.com"
                            || h.ends_with(".zaloapp.com")
                    })
                    .unwrap_or(false)
            })
            .unwrap_or_else(|_| url.contains("zalo.me") || url.contains("zaloapp.com"));
        if is_zalo_host {
            let active_sek = self.current_zpw_sek();
            let mut pairs = Vec::new();
            if let Ok(m) = self.live_cookies.read() {
                for (k, v) in m.iter() {
                    if k != "zpw_sek" && k != "_zlang" && !v.is_empty() {
                        pairs.push(format!("{k}={v}"));
                    }
                }
            }
            if let Some(sek) = &active_sek {
                if !sek.is_empty() {
                    pairs.push(format!("zpw_sek={sek}"));
                    rb = rb.header("zpw_sek", sek);
                }
            }
            pairs.push("_zlang=vi".into());
            rb = rb.header("Cookie", pairs.join("; "));
            rb = rb.header("Origin", "https://chat.zalo.me");
            rb = rb.header("Referer", "https://chat.zalo.me/");
        }
        if let Some(b) = &body {
            rb = rb
                .header("Content-Type", "application/x-www-form-urlencoded")
                .body(b.clone());
        }
        let resp_result = rb.send().await;
        let latency_ms = start.elapsed().as_millis();

        let resp = match resp_result {
            Ok(r) => r,
            Err(e) => {
                crate::logger::log_request(
                    &method,
                    url,
                    None,
                    latency_ms,
                    body.as_deref().unwrap_or(""),
                    None,
                    Some(&e.to_string()),
                );
                return Err(e.into());
            }
        };

        self.ingest_set_cookie_headers(resp.headers());
        let status = resp.status();
        let text = resp.text().await?;
        if self.verbose {
            eprintln!("← {status} {}", truncate(&text, 400));
        }
        if self.dump {
            self.dump_exchange(&method, url, status.as_u16(), &text);
        }

        let is_err = !status.is_success();
        let err_str = if is_err {
            Some(format!("HTTP {status}"))
        } else {
            None
        };

        crate::logger::log_request(
            &method,
            url,
            Some(status.as_u16()),
            latency_ms,
            body.as_deref().unwrap_or(""),
            Some(&text),
            err_str.as_deref(),
        );

        if is_err {
            bail!("HTTP {status} from {url}: {}", truncate(&text, 300));
        }
        if text.trim().is_empty() {
            // Long-polling endpoints (`p4-msg-chat.zaloapp.com`) return HTTP 200 with an empty body `""`
            // on keepalive timeout (`if ("" === e.data) return void this.processEmptyData()`).
            return Ok(serde_json::json!({ "error_code": 0, "data": null }));
        }
        let v: Value = serde_json::from_str(&text).unwrap_or_else(|_| {
            // Encrypted responses arrive as a bare base64 blob, not JSON. Hand the raw
            // text back so callers can decrypt it instead of failing here.
            Value::String(text.clone())
        });
        let code = v.get("error_code").and_then(|c| c.as_i64()).unwrap_or(0);
        if code != 0 {
            let msg = v.get("error_message").and_then(|m| m.as_str()).unwrap_or("");
            crate::logger::log_request(
                &method,
                url,
                Some(status.as_u16()),
                latency_ms,
                body.as_deref().unwrap_or(""),
                Some(&text),
                Some(&format!("server error {code}: {msg}")),
            );
            return Err(SessionError::new(code, msg).at(url).into());
        }
        Ok(v)
    }

    /// `decryptResp` — when the request was param-encrypted, `data` comes back as
    /// base64 AES over the key (AES-128 or AES-256), not plain JSON.
    pub async fn maybe_decrypt(&self, v: Value, key: &[u8]) -> Value {
        let Some(data) = v.get("data") else {
            return v;
        };
        let Some(b64) = data.as_str() else {
            return v;
        };
        if b64.is_empty() || b64.starts_with('{') {
            return v;
        }
        if key.is_empty() {
            return v;
        }
        match crate::crypto::decode_aes(key, b64) {
            Ok(pt) => match serde_json::from_str::<Value>(&pt) {
                Ok(inner) => {
                    if self.verbose {
                        eprintln!("  data decrypted ({} bytes)", pt.len());
                    }
                    // The decrypted payload is itself a full response envelope with its
                    // own error_code — propagate it instead of masking as success.
                    inner
                }
                Err(e) => {
                    if self.verbose {
                        eprintln!("  data decrypted but not JSON: {e} / {}", truncate(&pt, 200));
                    }
                    v
                }
            },
            Err(e) => {
                if self.verbose {
                    eprintln!("  data decrypt failed: {e}");
                }
                v
            }
        }
    }
}

/// Build `{a:1,b:"x"}` maps tersely.
pub fn obj(pairs: &[(&str, Value)]) -> Value {
    let mut m = Map::new();
    for (k, v) in pairs {
        m.insert((*k).to_string(), v.clone());
    }
    Value::Object(m)
}

/// Strip anything that could be a credential before a URL lands in a dump file.
pub fn redact_url(url: &str) -> String {
    const SENSITIVE: &[&str] = &[
        "password", "passw", "zpw_sek", "zpw_enk", "cookie", "token", "signkey", "params", "imei",
    ];
    let Some((base, query)) = url.split_once('?') else {
        return url.to_string();
    };
    let cleaned: Vec<String> = query
        .split('&')
        .map(|kv| {
            let key = kv.split('=').next().unwrap_or("").to_ascii_lowercase();
            if SENSITIVE.iter().any(|s| key.contains(s)) {
                format!("{}=<redacted>", kv.split('=').next().unwrap_or(""))
            } else {
                kv.to_string()
            }
        })
        .collect();
    format!("{base}?{}", cleaned.join("&"))
}

/// Redact a sensitive secret string, showing at most the first 8 characters followed by `…<redacted>`.
pub fn redact_secret(s: &str) -> String {
    let prefix: String = s.chars().take(8).collect();
    format!("{prefix}…<redacted>")
}

pub fn urlencode(s: &str) -> String {    let mut out = String::with_capacity(s.len() * 3);
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

pub fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let t: String = s.chars().take(n).collect();
        format!("{t}…")
    }
}

/// Client-generated device id. Persisted only in memory for now — regenerate per run.
fn random_imei() -> String {
    crate::call::official_imei()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_atomic_save_preserves_saved_format_and_restores_fixture() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "ctdozlite-session-test-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir(&root).unwrap();
        let path = root.join("session.json");
        let mut source = Session::new(false, false, false).unwrap();
        source.computer_name = "fixture-device".into();
        source.secret_key = Some(vec![7; 32]);
        source.session_key = Some("fixture-session-key".into());
        source.zpw_sek = Some("fixture-auth-token".into());
        source.save_to_file(path.to_str().unwrap()).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let saved: SavedSession = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(saved.computer_name, "fixture-device");
        assert_eq!(saved.session_key.as_deref(), Some("fixture-session-key"));
        let mut restored = Session::new(false, false, false).unwrap();
        restored.load_from_file(path.to_str().unwrap()).unwrap();
        assert_eq!(restored.computer_name, source.computer_name);
        assert_eq!(restored.secret_key, source.secret_key);
        assert_eq!(restored.session_key, source.session_key);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn session_new_initializes_zpw_sek_and_headers() {
        let s = Session::new(false, false, false).unwrap();
        assert!(s.zpw_sek.is_none());
        assert!(s.login.is_none());
    }

    #[test]
    fn session_set_zpw_sek_updates_fields() {
        let mut s = Session::new(false, false, false).unwrap();
        s.login = Some(LoginInfo::default());
        s.set_zpw_sek("test_sek_value".into());
        assert_eq!(s.zpw_sek.as_deref(), Some("test_sek_value"));
        assert_eq!(s.login.as_ref().unwrap().zpw_sek, "test_sek_value");
    }

    #[test]
    fn redact_secret_truncates_to_eight_chars() {
        assert_eq!(redact_secret("1234567890abcdef"), "12345678…<redacted>");
        assert_eq!(redact_secret("short"), "short…<redacted>");
        assert_eq!(redact_secret(""), "…<redacted>");
    }

    #[test]
    fn server_map_cannot_override_verified_endpoints() {
        let mut domains = Domains::default();
        domains.apply_service_map(&serde_json::json!({
            "friend": ["https://tt-friend-wpa.zaloapp.com"],
            "profile": "https://tt-profile-wpa.zaloapp.com",
            "chat": "https://tt-chat9-wpa.api.zaloapp.com",
        }));

        assert_eq!(domains.friend, "https://tt-friend-wpa.chat.zalo.me");
        assert_eq!(domains.profile, "https://tt-profile-wpa.chat.zalo.me");
        assert_eq!(domains.chat, crate::endpoints::service("chat").unwrap());
    }

    #[test]
    fn server_map_fills_endpoints_without_a_verified_constant() {
        let mut domains = Domains::default();
        domains.apply_service_map(&serde_json::json!({
            "group_cloud_message": "https://tt-group-cm.example.test/",
        }));

        assert_eq!(domains.cloud_msg, "https://tt-group-cm.example.test");
    }

    #[test]
    fn server_map_fills_an_empty_verified_slot() {
        let mut domains = Domains::default();
        domains.sticker.clear();
        domains.apply_service_map(&serde_json::json!({"sticker": "https://from-server.test"}));

        assert_eq!(domains.sticker, "https://from-server.test");
    }

    #[test]
    fn heal_restores_every_verified_domain() {
        let mut domains = Domains::default();
        domains.chat = "https://p4-msg-chat.zaloapp.com".into();
        domains.conversation = "https://p4-ctl-chat.zaloapp.com".into();
        domains.convers_api = "https://tt-convers-wpa.zaloapp.com/".into();
        domains.friend = "https://tt-friend-wpa.zaloapp.com".into();
        domains.profile = "https://tt-profile-wpa.zaloapp.com".into();
        domains.group = String::new();
        domains.file = "https://wrong.example/".into();
        domains.voice_call = String::new();
        domains.sticker = String::new();

        domains.heal_from_verified();

        for (key, actual) in [
            ("friend", &domains.friend),
            ("profile", &domains.profile),
            ("group", &domains.group),
            ("file", &domains.file),
            ("voice_call", &domains.voice_call),
            ("sticker", &domains.sticker),
            ("chat", &domains.chat),
            ("conversation", &domains.conversation),
            ("conversation", &domains.convers_api),
        ] {
            assert_eq!(
                actual.as_str(),
                crate::endpoints::service(key).unwrap(),
                "endpoint {key} was not healed"
            );
        }
    }

    #[test]
    fn heal_leaves_server_owned_endpoints_untouched() {
        let mut domains = Domains::default();
        domains.lp_chat = "https://p9-msg-chat.zaloapp.com".into();
        domains.lp_control = "https://p9-ctl-chat.zaloapp.com".into();
        domains.cloud_msg = "https://tt-group-cm.example.test".into();
        domains.ws = vec!["wss://ws99-msg.chat.zalo.me".into()];

        domains.heal_from_verified();

        assert_eq!(domains.lp_chat, "https://p9-msg-chat.zaloapp.com");
        assert_eq!(domains.lp_control, "https://p9-ctl-chat.zaloapp.com");
        assert_eq!(domains.cloud_msg, "https://tt-group-cm.example.test");
        assert_eq!(domains.ws, vec!["wss://ws99-msg.chat.zalo.me"]);
    }

    #[test]
    fn apply_server_info_takes_long_polling_hosts_from_the_server() {
        let mut s = Session::new(false, false, false).unwrap();
        s.apply_server_info(&serde_json::json!({
            "zpw_chts_new": ["https://p7-msg-chat.zaloapp.com"],
            "zpw_ctrs_new": ["https://p7-ctl-chat.zaloapp.com"],
            "zpw_ws": ["wss://ws7-msg.chat.zalo.me"],
            "zpw_service_map_v3": {
                "friend": ["https://tt-friend-wpa.zaloapp.com"],
                "group_cloud_message": ["https://tt-group-cm.example.test"],
            },
        }));

        assert_eq!(s.domains.lp_chat, "https://p7-msg-chat.zaloapp.com");
        assert_eq!(s.domains.lp_control, "https://p7-ctl-chat.zaloapp.com");
        assert_eq!(s.domains.ws, vec!["wss://ws7-msg.chat.zalo.me"]);
        assert_eq!(s.domains.friend, "https://tt-friend-wpa.chat.zalo.me");
        assert_eq!(s.domains.cloud_msg, "https://tt-group-cm.example.test");
    }

    #[test]
    fn loading_a_legacy_session_heals_every_verified_domain() {
        let fixture = serde_json::json!({
            "imei": "",
            "computer_name": "DESKTOP-TEST",
            "secret_key_b64": null,
            "session_key": null,
            "zpw_sek": "fixture-sek",
            "login": null,
            "domains": {
                "auth": crate::endpoints::AUTH,
                "wpa_chat": crate::endpoints::WPA_CHAT,
                "chat": "https://p4-msg-chat.zaloapp.com",
                "conversation": "https://p4-ctl-chat.zaloapp.com",
                "lp_chat": "https://p9-msg-chat.zaloapp.com",
                "lp_control": "https://p9-ctl-chat.zaloapp.com",
                "friend": "https://tt-friend-wpa.zaloapp.com",
                "convers_api": "https://tt-convers-wpa.zaloapp.com",
                "profile": "https://tt-profile-wpa.zaloapp.com",
                "group": "https://tt-group-wpa.zaloapp.com",
                "file": "https://tt-files-wpa.zaloapp.com",
                "cloud_msg": "https://tt-group-cm.api.zaloapp.com",
                "voice_call": "https://voicecall-wpa.zaloapp.com",
                "sticker": "https://tt-sticker-wpa.zaloapp.com",
                "ws": ["wss://ws9-msg.chat.zalo.me"],
            },
        });
        let path =
            std::env::temp_dir().join(format!("ctdozlite-legacy-{}.json", std::process::id()));
        std::fs::write(&path, fixture.to_string()).unwrap();

        let mut s = Session::new(false, false, false).unwrap();
        let loaded = s.load_from_file(path.to_str().unwrap());
        let _ = std::fs::remove_file(&path);
        loaded.unwrap();

        for (key, actual) in [
            ("friend", &s.domains.friend),
            ("profile", &s.domains.profile),
            ("group", &s.domains.group),
            ("file", &s.domains.file),
            ("voice_call", &s.domains.voice_call),
            ("sticker", &s.domains.sticker),
            ("chat", &s.domains.chat),
            ("conversation", &s.domains.conversation),
            ("conversation", &s.domains.convers_api),
        ] {
            assert_eq!(
                actual.as_str(),
                crate::endpoints::service(key).unwrap(),
                "endpoint {key} was not healed on load"
            );
        }
        assert_eq!(s.domains.lp_chat, "https://p9-msg-chat.zaloapp.com");
        assert_eq!(s.domains.lp_control, "https://p9-ctl-chat.zaloapp.com");
        assert_eq!(s.domains.cloud_msg, "https://tt-group-cm.api.zaloapp.com");
        assert_eq!(s.domains.ws, vec!["wss://ws9-msg.chat.zalo.me"]);
        assert_eq!(s.zpw_sek.as_deref(), Some("fixture-sek"));
    }

    #[test]
    fn status_maps_error_codes_and_only_auth_codes_expire() {
        assert_eq!(SessionStatus::from_error_code(0), SessionStatus::Valid);
        assert_eq!(SessionStatus::from_error_code(600), SessionStatus::Expired);
        assert_eq!(SessionStatus::from_error_code(-102), SessionStatus::Expired);
        for code in [111, 114, 18022, -1, 500, 401] {
            assert_eq!(
                SessionStatus::from_error_code(code),
                SessionStatus::Unreachable,
                "error_code {code} must not be treated as an expired session"
            );
        }
    }

    #[test]
    fn status_classifies_typed_server_errors_and_bare_failures() {
        let auth: anyhow::Error = SessionError::new(600, "zpw_sek bị thiếu hoặc không đúng").into();
        assert_eq!(SessionStatus::from_error(&auth), SessionStatus::Expired);

        let unrelated: anyhow::Error = SessionError::new(111, "invalid param").into();
        assert_eq!(
            SessionStatus::from_error(&unrelated),
            SessionStatus::Unreachable
        );

        for msg in [
            "HTTP 404 Not Found from https://tt-friend-wpa.chat.zalo.me/api/x",
            "error sending request for url (https://wpa.chat.zalo.me/...): connection refused",
            "error decoding response body",
        ] {
            let e = anyhow::anyhow!("{msg}");
            assert_eq!(
                SessionStatus::from_error(&e),
                SessionStatus::Unreachable,
                "{msg} must not expire the session"
            );
        }
    }

    #[test]
    fn candidates_preserve_priority_order_verified_then_service_map_then_saved() {
        let mut domains = Domains::default();
        domains.chat = "https://legacy-saved-chat.zaloapp.com/".into();
        domains.heal_from_verified();
        domains.apply_service_map(&serde_json::json!({
            "chat": [
                "https://tt-chat4-wpa.api.zaloapp.com/",
                "https://tt-chat5-wpa.api.zaloapp.com"
            ]
        }));

        let list = domains.candidates("chat");
        assert_eq!(
            list,
            vec![
                "https://tt-chat3-wpa.api.zaloapp.com",
                "https://tt-chat4-wpa.api.zaloapp.com",
                "https://tt-chat5-wpa.api.zaloapp.com",
                "https://legacy-saved-chat.zaloapp.com",
            ]
        );
    }

    #[tokio::test]
    async fn request_with_candidates_fails_over_on_404_and_remembers_winner() {
        let mut s = Session::new(false, false, false).unwrap();
        s.domains.apply_service_map(&serde_json::json!({
            "chat": [
                "https://tt-chat4-wpa.api.zaloapp.com",
                "https://tt-chat5-wpa.api.zaloapp.com"
            ]
        }));

        let tried = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let tried_c = tried.clone();
        let res = s
            .request_with_candidates("chat", "/api/message/sms?zpw_ver=694", move |url| {
                let tried_inner = tried_c.clone();
                async move {
                    tried_inner.lock().unwrap().push(url.clone());
                    if url.starts_with("https://tt-chat3-wpa.api.zaloapp.com") {
                        anyhow::bail!("HTTP 404 Not Found from {url}: nginx")
                    } else {
                        Ok(serde_json::json!({ "error_code": 0, "winner": url }))
                    }
                }
            })
            .await
            .unwrap();

        assert_eq!(
            res["winner"],
            "https://tt-chat4-wpa.api.zaloapp.com/api/message/sms?zpw_ver=694"
        );
        assert_eq!(
            *tried.lock().unwrap(),
            vec![
                "https://tt-chat3-wpa.api.zaloapp.com/api/message/sms?zpw_ver=694",
                "https://tt-chat4-wpa.api.zaloapp.com/api/message/sms?zpw_ver=694",
            ]
        );

        // Subsequent request in the same session starts directly with the remembered winner.
        let tried2 = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let tried2_c = tried2.clone();
        let _ = s
            .request_with_candidates("chat", "/api/message/sms?zpw_ver=694", move |url| {
                let tried_inner = tried2_c.clone();
                async move {
                    tried_inner.lock().unwrap().push(url.clone());
                    Ok(serde_json::json!({ "error_code": 0 }))
                }
            })
            .await
            .unwrap();
        assert_eq!(
            *tried2.lock().unwrap(),
            vec!["https://tt-chat4-wpa.api.zaloapp.com/api/message/sms?zpw_ver=694"]
        );
    }

    #[tokio::test]
    async fn request_with_candidates_does_not_failover_on_auth_errors() {
        let mut s = Session::new(false, false, false).unwrap();
        s.domains.apply_service_map(&serde_json::json!({
            "friend": ["https://tt-friend2-wpa.chat.zalo.me"]
        }));

        for code in [600_i64, -102_i64] {
            let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let calls_c = calls.clone();
            let err = s
                .request_with_candidates("friend", "/api/social/friend/getfriends", move |url| {
                    let calls_inner = calls_c.clone();
                    async move {
                        calls_inner.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        Err(SessionError::new(code, "session expired").at(url).into())
                    }
                })
                .await
                .unwrap_err();
            assert_eq!(SessionStatus::from_error(&err), SessionStatus::Expired);
            assert_eq!(
                calls.load(std::sync::atomic::Ordering::SeqCst),
                1,
                "error_code {code} must stop immediately without failover"
            );
        }
    }

    #[test]
    fn ingest_set_cookie_headers_rotates_live_zpw_sek_and_ignores_1970_deletions() {
        let mut s = Session::new(false, false, false).unwrap();
        s.set_zpw_sek("OLD_QR_TICKET_SEK".into());
        assert_eq!(s.current_zpw_sek().as_deref(), Some("OLD_QR_TICKET_SEK"));

        let mut headers = reqwest::header::HeaderMap::new();
        headers.append(
            reqwest::header::SET_COOKIE,
            reqwest::header::HeaderValue::from_static(
                "zpw_sek=NEW_LOGIN_SESSION_SEK; Path=/; Domain=.zalo.me; Secure; HttpOnly",
            ),
        );
        headers.append(
            reqwest::header::SET_COOKIE,
            reqwest::header::HeaderValue::from_static(
                "zpw_sek=; Path=/; Domain=.zaloapp.com; Expires=Thu, 01-Jan-1970 00:00:00 GMT",
            ),
        );
        headers.append(
            reqwest::header::SET_COOKIE,
            reqwest::header::HeaderValue::from_static("zpsid=SID_999; Path=/; Domain=.zalo.me"),
        );

        // Notice `s` is an immutable `&Session`, just like inside `Session::send(&self)` and `get_login_info(&self)`.
        let s_ref: &Session = &s;
        s_ref.ingest_set_cookie_headers(&headers);
        assert_eq!(s_ref.current_zpw_sek().as_deref(), Some("NEW_LOGIN_SESSION_SEK"));

        s.sync_live_sek();
        assert_eq!(s.zpw_sek.as_deref(), Some("NEW_LOGIN_SESSION_SEK"));
        assert_eq!(
            s.live_cookies.read().unwrap().get("zpsid").map(String::as_str),
            Some("SID_999")
        );
    }
}
