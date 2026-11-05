//! Hub-facing REST extensions: QR login flow, logout, sync/poll, extended status/threads/send,
//! reactions, undo, friend lookup/request and group members.
//!
//! No media is ever written to disk here: only CDN URLs reach `zalo_history.db` / `threads_cache.json`.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock};

use serde_json::{json, Value};
use tokio::sync::Mutex;

use crate::chat::{self, ChatMessage, PollState, Thread};
use crate::scheduler;
use crate::session::Session;
use crate::store::{self, MessageStore};

#[derive(Debug, Clone)]
pub struct QrFlowState {
    pub status: String,
    pub token_id: String,
    pub qr_payload: String,
    pub qr_timeout: u64,
    pub scanned_name: String,
    pub scanned_avatar: String,
    pub error: String,
    pub generation: u64,
}

impl Default for QrFlowState {
    fn default() -> Self {
        Self {
            status: "idle".into(),
            token_id: String::new(),
            qr_payload: String::new(),
            qr_timeout: 0,
            scanned_name: String::new(),
            scanned_avatar: String::new(),
            error: String::new(),
            generation: 0,
        }
    }
}

#[derive(Default)]
struct Hub {
    threads: Vec<Thread>,
    poll: PollState,
}

static QR_STATE: LazyLock<std::sync::Mutex<QrFlowState>> = LazyLock::new(Default::default);

static HUB: LazyLock<Mutex<Hub>> = LazyLock::new(|| {
    Mutex::new(Hub {
        threads: chat::load_threads_cache(),
        poll: PollState::default(),
    })
});
static SYNC_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));
static LAST_POLL_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static LAST_PREVIEW_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

async fn sync_and_persist_session(session: &Arc<Mutex<Session>>) {
    let mut g = session.lock().await;
    let old_sek = g.zpw_sek.clone();
    g.sync_live_sek();
    if g.zpw_sek != old_sek && g.zpw_sek.is_some() {
        let _ = g.save_to_file("session.json");
    }
}

async fn confirm_and_mark_expired(session: &Arc<Mutex<Session>>, s: &Session) -> bool {
    if s.session_status().await == crate::session::SessionStatus::Expired {
        let mut g = session.lock().await;
        g.mark_expired();
        let _ = g.save_to_file("session.json");
        true
    } else {
        false
    }
}
fn qr_state() -> &'static std::sync::Mutex<QrFlowState> {
    &QR_STATE
}

fn hub() -> &'static Mutex<Hub> {
    &HUB
}

fn set_qr(f: impl FnOnce(&mut QrFlowState)) {
    if let Ok(mut g) = qr_state().lock() {
        f(&mut g);
    }
}

fn is_live(s: &Session) -> bool {
    s.login.is_some()
        && (s.zpw_sek.is_some() || s.current_zpw_sek().is_some())
        && !s.is_expired_marked()
}

fn err(code: u16, msg: impl ToString) -> (u16, Value) {
    (code, json!({ "ok": false, "error": msg.to_string() }))
}

fn profile_json(s: &Session) -> Value {
    let li = s.login.as_ref();
    json!({
        "uin": li.map(|l| l.uin.as_str()).unwrap_or(""),
        "display_name": li.map(|l| l.display_name.as_str()).unwrap_or(""),
        "phone": li.map(|l| l.phone.as_str()).unwrap_or(""),
        "avatar": li.map(|l| l.avatar.as_str()).unwrap_or(""),
        "imei": s.imei,
    })
}

fn thread_json(t: &Thread, msg_limit: usize) -> Value {
    let p = &t.presentation;
    let skip = t.messages.len().saturating_sub(msg_limit);
    let last_ts = if p.last_message_ts > 0 {
        p.last_message_ts
    } else {
        t.messages.iter().map(|m| m.ts).max().unwrap_or(0)
    };
    json!({
        "id": t.id, "name": t.name, "avatar": t.avatar, "is_group": t.is_group,
        "preview": t.preview, "time": t.time, "last_msg_ts": last_ts,
        "unread_count": t.unread.count(), "is_pinned": p.pinned, "is_muted": p.muted,
        "is_hidden": p.hidden, "member_count": p.member_count,
        "messages": &t.messages[skip..],
    })
}

fn sort_threads(list: &mut [Thread]) {
    list.sort_by(|a, b| {
        let ts_a = a.presentation.last_message_ts.max(a.messages.iter().map(|m| m.ts).max().unwrap_or(0));
        let ts_b = b.presentation.last_message_ts.max(b.messages.iter().map(|m| m.ts).max().unwrap_or(0));
        ts_b.cmp(&ts_a)
            .then_with(|| (!b.messages.is_empty()).cmp(&!a.messages.is_empty()))
            .then_with(|| (!b.preview.is_empty()).cmp(&!a.preview.is_empty()))
    });
}

fn persist(threads: &[Thread]) {
    if let Ok(mut st) = MessageStore::open("zalo_history.db") {
        let _ = store::persist_threads(&mut st, threads);
    }
    chat::save_threads_cache(threads);
}

fn fingerprint(threads: &[Thread]) -> (usize, i64) {
    (
        threads.len()
            + threads.iter().map(|t| t.messages.len() + t.preview.len()).sum::<usize>(),
        threads.iter().flat_map(|t| t.messages.iter().map(|m| m.ts)).max().unwrap_or(0),
    )
}

/// Full friends + groups + last-message sync; replaces the in-memory thread list.
pub async fn full_sync(session: &Arc<Mutex<Session>>) -> anyhow::Result<Vec<Thread>> {
    let _guard = SYNC_LOCK.lock().await;
    let s = session.lock().await.clone();
    if confirm_and_mark_expired(session, &s).await {
        anyhow::bail!("Phiên Zalo đã hết hạn (zpw_sek không còn hiệu lực)");
    }
    let mut list = chat::load_threads_cache();
    match chat::get_friends(&s).await {
        Ok(friends) => {
            for ft in friends {
                if let Some(ex) = list.iter_mut().find(|t| t.id == ft.id) {
                    if !ft.name.is_empty() {
                        ex.name = ft.name;
                    }
                    if chat::should_replace_avatar(ex.avatar.as_deref(), ft.avatar.as_deref()) {
                        ex.avatar = ft.avatar;
                    }
                    ex.is_group = ex.is_group || ft.is_group;
                } else {
                    list.push(ft);
                }
            }
        }
        Err(e) => {
            if crate::session::SessionStatus::from_error(&e) == crate::session::SessionStatus::Expired
                && confirm_and_mark_expired(session, &s).await
            {
                return Err(e);
            }
        }
    }
    if let Some(li) = &s.login {
        if !li.send2me_id.is_empty() && !list.iter().any(|t| t.id == li.send2me_id) {
            list.insert(
                0,
                Thread {
                    presentation: Default::default(),
                    unread: Default::default(),
                    id: li.send2me_id.clone(),
                    name: "Cloud của tôi (Lưu trữ)".into(),
                    preview: "".into(),
                    time: crate::ui::clock(),
                    avatar: None,
                    is_group: false,
                    messages: Vec::new(),
                },
            );
        }
    }
    store::load_history_into_threads("zalo_history.db", &mut list, MessageStore::PRELOAD_MSGS);
    let mut poll = PollState::default();
    let _ = chat::poll_new_messages(&s, &mut poll, &mut list).await;
    chat::deduplicate_threads(&mut list, None);
    sort_threads(&mut list);
    persist(&list);
    sync_and_persist_session(session).await;
    let now = now_ms();
    LAST_POLL_MS.store(now, std::sync::atomic::Ordering::Relaxed);
    LAST_PREVIEW_MS.store(now, std::sync::atomic::Ordering::Relaxed);
    let mut h = hub().lock().await;
    h.threads = list.clone();
    if !poll.last_action_id.is_empty() {
        h.poll = poll;
    }
    Ok(list)
}

/// One lightweight poll round; returns whether anything changed.
pub async fn poll_once(session: &Arc<Mutex<Session>>) -> anyhow::Result<bool> {
    let _guard = SYNC_LOCK.lock().await;
    let now = now_ms();
    let last = LAST_POLL_MS.load(std::sync::atomic::Ordering::Relaxed);
    if now.saturating_sub(last) < 2500 {
        return Ok(false);
    }
    LAST_POLL_MS.store(now, std::sync::atomic::Ordering::Relaxed);
    let s = session.lock().await.clone();
    if s.login.is_none() {
        anyhow::bail!("chưa đăng nhập");
    }
    let mut h = hub().lock().await;
    if h.threads.is_empty() {
        h.threads = chat::load_threads_cache();
        store::load_history_into_threads("zalo_history.db", &mut h.threads, MessageStore::PRELOAD_MSGS);
    }
    let before = fingerprint(&h.threads);
    let Hub { threads, poll } = &mut *h;
    let mut got_lp = false;
    for _ in 0..3 {
        match chat::get_chat_from_polling(&s, poll).await {
            Ok(lp_resp) => {
                got_lp = true;
                chat::merge_previews(threads, &lp_resp);
                let data_node = lp_resp.get("data").unwrap_or(&lp_resp);
                let more = data_node.get("more").and_then(Value::as_i64).unwrap_or(0);
                let has_reset = data_node
                    .get("resetLastActionId")
                    .and_then(Value::as_str)
                    .map(|r| r != "0" && !r.is_empty())
                    .unwrap_or(false);
                if more == 0 && !has_reset {
                    break;
                }
            }
            Err(e) => {
                if crate::session::SessionStatus::from_error(&e) == crate::session::SessionStatus::Expired
                    && confirm_and_mark_expired(session, &s).await
                {
                    return Err(e);
                }
                break;
            }
        }
    }
    let last_prev = LAST_PREVIEW_MS.load(std::sync::atomic::Ordering::Relaxed);
    if got_lp || now.saturating_sub(last_prev) >= 20_000 {
        LAST_PREVIEW_MS.store(now, std::sync::atomic::Ordering::Relaxed);
        match chat::get_last_msgs(&s, &[]).await {
            Ok(preview_resp) => chat::merge_previews(threads, &preview_resp),
            Err(e) => {
                if crate::session::SessionStatus::from_error(&e) == crate::session::SessionStatus::Expired
                    && confirm_and_mark_expired(session, &s).await
                {
                    return Err(e);
                }
            }
        }
    }
    let changed = before != fingerprint(threads);
    if changed {
        chat::resolve_thread_names(&s, threads).await;
        chat::deduplicate_threads(threads, None);
        sort_threads(threads);
        persist(threads);
    }
    drop(h);
    sync_and_persist_session(session).await;
    Ok(changed)
}

/// Startup refresh + background standby poll loop.
pub fn spawn_background(session: Arc<Mutex<Session>>) {
    tokio::spawn(async move {
        {
            let mut s = session.lock().await;
            if is_live(&s) {
                let _ = crate::login::get_server_info(&mut s).await;
                let _ = s.sync_profile_me().await;
                s.sync_live_sek();
                let _ = s.save_to_file("session.json");
            }
        }
        if is_live(&*session.lock().await) {
            let _ = full_sync(&session).await;
        }
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(8)).await;
            let last = LAST_POLL_MS.load(std::sync::atomic::Ordering::Relaxed);
            if now_ms().saturating_sub(last) >= 6000 && is_live(&*session.lock().await) {
                let _ = poll_once(&session).await;
            }
        }
    });
}

async fn qr_start(session: &Arc<Mutex<Session>>) -> (u16, Value) {
    {
        let s = session.lock().await;
        if is_live(&s) {
            return (200, json!({ "ok": true, "qr_status": "connected", "logged_in": true }));
        }
    }
    if let Ok(g) = qr_state().lock() {
        if (g.status == "waiting_scan" || g.status == "scanned" || g.status == "authenticating")
            && !g.qr_payload.is_empty()
        {
            return (
                200,
                json!({
                    "ok": true,
                    "token_id": g.token_id,
                    "qr_payload": g.qr_payload,
                    "qr_timeout": g.qr_timeout
                }),
            );
        }
    }
    let mut my_gen = 0u64;
    set_qr(|g| {
        g.generation = g.generation.wrapping_add(1);
        my_gen = g.generation;
        g.status = "starting".into();
        g.error.clear();
    });
    let snap = {
        let s = session.lock().await;
        match s.fresh_for_qr() {
            Ok(x) => x,
            Err(e) => return (500, json!({ "ok": false, "error": e.to_string() })),
        }
    };
    let q = match crate::qr::request_qr(&snap, "vi").await {
        Ok(q) => q,
        Err(e) => {
            set_qr(|g| {
                if g.generation == my_gen {
                    g.status = "error".into();
                    g.error = e.to_string();
                }
            });
            return err(502, e);
        }
    };
    set_qr(|g| {
        if g.generation == my_gen {
            g.status = "waiting_scan".into();
            g.token_id = q.token_id.clone();
            g.qr_payload = q.qr_payload.clone();
            g.qr_timeout = q.qr_timeout;
            g.scanned_name.clear();
            g.scanned_avatar.clear();
            g.error.clear();
        }
    });
    let resp = json!({
        "ok": true,
        "token_id": q.token_id,
        "qr_payload": q.qr_payload,
        "qr_timeout": q.qr_timeout
    });
    let sess = session.clone();
    tokio::spawn(async move {
        if let Err(e) = qr_finish(&sess, &snap, &q, my_gen).await {
            set_qr(|g| {
                if g.generation == my_gen && g.status != "connected" {
                    g.status = if e.to_string().contains("hết thời gian") {
                        "expired"
                    } else {
                        "error"
                    }
                    .into();
                    g.error = e.to_string();
                }
            });
        }
    });
    (200, resp)
}

async fn qr_finish(
    sess: &Arc<Mutex<Session>>,
    snap: &Session,
    q: &crate::qr::QrLogin,
    my_gen: u64,
) -> anyhow::Result<()> {
    let scanned = crate::qr::wait_for_scan(snap, q).await?;
    let pick = |k: &[&str]| {
        k.iter()
            .find_map(|x| scanned.get(*x).and_then(Value::as_str))
            .unwrap_or("")
            .to_string()
    };
    let (name, ava) = (
        pick(&["display_name", "displayName"]),
        pick(&["avatar", "avt"]),
    );
    set_qr(|g| {
        if g.generation == my_gen {
            g.status = "scanned".into();
            g.scanned_name = name.clone();
            g.scanned_avatar = ava.clone();
        }
    });
    let resp = crate::qr::confirm(snap, q).await?;
    set_qr(|g| {
        if g.generation == my_gen {
            g.status = "authenticating".into();
        }
    });
    let mut s = snap.clone();
    if let Some(sek) = crate::qr::extract_zpw_sek(&resp) {
        s.set_zpw_sek(sek);
    }
    s.sync_live_sek();
    if let Some(d) = resp.get("data") {
        crate::login::apply_login_config(&mut s, d);
    }
    for attempt in 0..2 {
        if let Ok(info) = crate::login::get_login_info(&s).await {
            if let Some(sek) = crate::qr::extract_zpw_sek(&info) {
                s.set_zpw_sek(sek);
            }
            s.sync_live_sek();
            if let Some(cfg) = info.get("data") {
                crate::login::apply_login_config(&mut s, cfg);
                s.apply_server_info(cfg);
                s.sync_live_sek();
                if let Some(sek) = s.current_zpw_sek() {
                    s.set_zpw_sek(sek);
                }
            }
            if s.secret_key.is_some() {
                break;
            }
        }
        if attempt == 0 {
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        }
    }
    s.sync_live_sek();
    if let Some(sek) = s.current_zpw_sek() {
        s.set_zpw_sek(sek);
    }
    if let Some(li) = &mut s.login {
        if li.display_name.is_empty() && !name.is_empty() {
            li.display_name = name.clone();
        }
        if li.avatar.is_empty() && !ava.is_empty() {
            li.avatar = ava.clone();
        }
    }
    let _ = s.sync_profile_me().await;
    s.sync_live_sek();
    if let Some(sek) = s.current_zpw_sek() {
        s.set_zpw_sek(sek);
    }
    if let Some(li) = &mut s.login {
        if li.display_name.is_empty() && !name.is_empty() {
            li.display_name = name;
        }
        if li.avatar.is_empty() && !ava.is_empty() {
            li.avatar = ava;
        }
    }
    if std::path::Path::new("session.json").exists() {
        let mut should_backup = false;
        if let Ok(raw) = std::fs::read_to_string("session.json") {
            if let Ok(v) = serde_json::from_str::<Value>(&raw) {
                let has_sek = v
                    .get("zpw_sek")
                    .and_then(Value::as_str)
                    .map(|x| !x.is_empty() && x != "EXPIRED")
                    .unwrap_or(false)
                    || v.get("login")
                        .and_then(|l| l.get("zpw_sek"))
                        .and_then(Value::as_str)
                        .map(|x| !x.is_empty() && x != "EXPIRED")
                        .unwrap_or(false);
                should_backup = has_sek;
            }
        }
        if should_backup {
            let _ = std::fs::copy("session.json", "session.json.bak");
        }
    }
    s.save_to_file("session.json")?;
    {
        let mut global_s = sess.lock().await;
        *global_s = s;
    }
    set_qr(|g| {
        g.status = "connected".into();
        g.error.clear();
    });
    let sess_bg = sess.clone();
    tokio::spawn(async move {
        let _ = full_sync(&sess_bg).await;
    });
    Ok(())
}

async fn qr_status(session: &Arc<Mutex<Session>>) -> Value {
    let g = qr_state().lock().map(|g| g.clone()).unwrap_or_default();
    let s = session.lock().await;
    let mut v = json!({
        "ok": true, "qr_status": g.status, "token_id": g.token_id, "qr_payload": g.qr_payload,
        "scanned_name": g.scanned_name, "scanned_avatar": g.scanned_avatar,
        "logged_in": is_live(&s), "error": g.error,
    });
    if let (Some(o), Some(p)) = (v.as_object_mut(), profile_json(&s).as_object()) {
        o.extend(p.clone());
    }
    v
}

fn msg_from(v: &Value) -> ChatMessage {
    let text = |k: &str| match v.get(k) {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    };
    ChatMessage {
        id: text("id").if_empty(text("msg_id")),
        sender: text("sender"),
        text: text("text"),
        ts: v.get("ts").and_then(|t| t.as_i64().or_else(|| t.as_str().and_then(|s| s.parse().ok()))).unwrap_or(0),
        is_outgoing: v.get("is_outgoing").and_then(Value::as_bool).unwrap_or(false),
        ..Default::default()
    }
}

trait IfEmpty { fn if_empty(self, alt: String) -> String; }
impl IfEmpty for String {
    fn if_empty(self, alt: String) -> String { if self.is_empty() { alt } else { self } }
}

async fn send(session: &Arc<Mutex<Session>>, p: &Value) -> (u16, Value) {
    let to = p.get("to_id").or_else(|| p.get("thread_id")).and_then(Value::as_str).unwrap_or("");
    let text = p.get("text").and_then(Value::as_str).unwrap_or("");
    let grp = p.get("is_group").and_then(Value::as_bool).unwrap_or(false);
    let file = p.get("file_path").and_then(Value::as_str).unwrap_or("");
    let sticker = p.get("sticker_id").and_then(Value::as_u64).unwrap_or(0);
    if to.is_empty() {
        return err(400, "to_id is required");
    }
    let s = session.lock().await.clone();
    let res = if !file.is_empty() {
        chat::send_attachment(&s, to, grp, std::path::Path::new(file), text).await.map(|(v, _)| v)
    } else if sticker > 0 {
        let cate = p.get("cate_id").and_then(Value::as_u64).unwrap_or(0);
        let ty = p.get("sticker_type").and_then(Value::as_u64).unwrap_or(0) as u32;
        chat::send_sticker(&s, to, sticker, cate, ty, grp).await
    } else if let Some(q) = p.get("quote").filter(|q| q.is_object()) {
        chat::send_quote_message(&s, to, text, &msg_from(q), grp).await
    } else if text.is_empty() {
        return err(400, "text, file_path or sticker_id is required");
    } else if grp {
        chat::send_group_message(&s, to, text).await
    } else {
        chat::send_message(&s, to, text).await
    };
    sync_and_persist_session(session).await;
    match res {
        Ok(r) => (200, json!({ "ok": true, "to_id": to, "response": r })),
        Err(e) => err(502, e),
    }
}

async fn group_members(session: &Arc<Mutex<Session>>, gid: &str) -> (u16, Value) {
    let s = session.lock().await.clone();
    let (infos, _) = match chat::fetch_group_profiles(&s, &[gid.to_string()]).await {
        Ok(r) => r,
        Err(e) => return err(502, e),
    };
    let clean = gid.trim_start_matches("group_").trim_start_matches('g');
    let Some(info) = infos.get(clean).or_else(|| infos.get(gid)) else {
        return err(404, "group not found");
    };
    let uids: Vec<String> = info.get("memVerList").and_then(Value::as_array).map(|a| {
        a.iter().filter_map(Value::as_str).map(|x| x.split('_').next().unwrap_or("").to_string()).filter(|x| !x.is_empty()).collect()
    }).unwrap_or_default();
    let owner = info.get("creatorId").map(|v| v.as_str().map(String::from).unwrap_or_else(|| v.to_string())).unwrap_or_default();
    let admins: Vec<String> = info.get("adminIds").and_then(Value::as_array).map(|a| {
        a.iter().map(|v| v.as_str().map(String::from).unwrap_or_else(|| v.to_string())).collect()
    }).unwrap_or_default();
    let mut profiles = serde_json::Map::new();
    for chunk in uids.chunks(50) {
        if let Ok(m) = chat::get_mini_profiles(&s, chunk).await {
            profiles.extend(m);
        }
    }
    let members: Vec<Value> = uids.iter().map(|u| {
        let pr = profiles.get(u).or_else(|| profiles.get(&format!("{u}_0")));
        let f = |ks: &[&str]| pr.and_then(|p| ks.iter().find_map(|k| p.get(*k).and_then(Value::as_str))).unwrap_or("");
        let role = if *u == owner { "owner" } else if admins.contains(u) { "admin" } else { "member" };
        json!({ "uid": u, "name": f(&["displayName", "zaloName", "name"]), "avatar": f(&["avatar", "avt"]), "role": role })
    }).collect();
    (200, json!({ "ok": true, "group_id": gid, "members": members }))
}

/// Route Hub-specific endpoints; `None` means "not handled here".
pub async fn route(
    session: &Arc<Mutex<Session>>,
    method: &str,
    path: &str,
    q: &HashMap<String, String>,
    body: &str,
    db_path: &str,
) -> Option<(u16, Value)> {
    let payload = || serde_json::from_str::<Value>(body).unwrap_or_else(|_| json!({}));
    let limit = |d: usize| q.get("limit").and_then(|s| s.parse().ok()).unwrap_or(d);
    Some(match (method, path) {
        ("GET", "/api/v1/status") | ("GET", "/health") => {
            let s = session.lock().await;
            let threads = MessageStore::open(db_path).and_then(|st| st.all_threads()).map(|v| v.len()).unwrap_or(0);
            let pending = scheduler::list_pending_scheduled(db_path, None).map(|v| v.len()).unwrap_or(0);
            let mut v = json!({
                "ok": true, "app": "ctdozlite", "version": env!("CARGO_PKG_VERSION"),
                "logged_in": is_live(&s), "session_expired": s.is_expired_marked(),
                "threads_in_db": threads, "pending_scheduled": pending,
            });
            if let (Some(o), Some(p)) = (v.as_object_mut(), profile_json(&s).as_object()) {
                o.extend(p.clone());
            }
            (200, v)
        }
        ("POST", "/api/v1/qr/start") => qr_start(session).await,
        ("GET", "/api/v1/qr/status") => (200, qr_status(session).await),
        ("POST", "/api/v1/logout") => {
            session.lock().await.logout();
            let _ = std::fs::remove_file("session.json");
            let _ = std::fs::remove_file("threads_cache.json");
            *hub().lock().await = Hub::default();
            set_qr(|g| {
                let next_gen = g.generation.wrapping_add(1);
                *g = QrFlowState {
                    generation: next_gen,
                    ..Default::default()
                };
            });
            (200, json!({ "ok": true }))
        }
        ("POST", "/api/v1/sync") => match full_sync(session).await {
            Ok(t) => (200, json!({ "ok": true, "thread_count": t.len(), "threads": t.iter().map(|x| thread_json(x, 0)).collect::<Vec<_>>() })),
            Err(e) if crate::session::SessionStatus::from_error(&e) == crate::session::SessionStatus::Expired => err(401, e),
            Err(e) => err(502, e),
        },
        ("POST", "/api/v1/poll") => match poll_once(session).await {
            Ok(changed) => {
                let h = hub().lock().await;
                (200, json!({ "ok": true, "changed": changed, "threads": h.threads.iter().map(|x| thread_json(x, 20)).collect::<Vec<_>>() }))
            }
            Err(e) if crate::session::SessionStatus::from_error(&e) == crate::session::SessionStatus::Expired => err(401, e),
            Err(e) => err(502, e),
        },
        ("GET", "/api/v1/threads") => {
            let mut t = chat::load_threads_cache();
            store::load_history_into_threads(db_path, &mut t, 50);
            sort_threads(&mut t);
            t.truncate(limit(500));
            (200, json!({ "ok": true, "threads": t.iter().map(|x| thread_json(x, 50)).collect::<Vec<_>>() }))
        }
        ("GET", p) if p.starts_with("/api/v1/threads/") && p.ends_with("/messages") => {
            let tid = p.trim_start_matches("/api/v1/threads/").trim_end_matches("/messages").trim_matches('/');
            let mut t = vec![Thread { id: tid.into(), ..Default::default() }];
            store::load_history_into_threads(db_path, &mut t, limit(60));
            let msgs = t.pop().map(|x| x.messages).unwrap_or_default();
            (200, json!({ "ok": true, "thread_id": tid, "messages": msgs }))
        }
        ("POST", "/api/v1/messages/send") => send(session, &payload()).await,
        ("POST", "/api/v1/messages/reaction") | ("POST", "/api/v1/messages/undo") => {
            let p = payload();
            let to = p.get("to_id").and_then(Value::as_str).unwrap_or("");
            let grp = p.get("is_group").and_then(Value::as_bool).unwrap_or(false);
            let m = msg_from(&p);
            if to.is_empty() || m.id.is_empty() {
                return Some(err(400, "to_id and msg_id are required"));
            }
            let s = session.lock().await.clone();
            let r = if path.ends_with("reaction") {
                chat::send_reaction(&s, to, grp, &m, p.get("reaction").and_then(Value::as_str).unwrap_or("/-heart")).await
            } else {
                chat::recall_message(&s, to, grp, &m).await
            };
            match r { Ok(v) => (200, json!({ "ok": true, "response": v })), Err(e) => err(502, e) }
        }
        ("GET", "/api/v1/friends/lookup") => {
            let s = session.lock().await.clone();
            match chat::find_user_by_phone(&s, q.get("phone").map(String::as_str).unwrap_or("")).await {
                Ok(r) => (200, json!({ "ok": true, "profile": r })),
                Err(e) => err(502, e),
            }
        }
        ("POST", "/api/v1/friends/request") => {
            let p = payload();
            let s = session.lock().await.clone();
            let to = p.get("to_id").and_then(Value::as_str).unwrap_or("");
            match chat::send_friend_request(&s, to, p.get("msg").and_then(Value::as_str).unwrap_or("")).await {
                Ok(v) => (200, json!({ "ok": true, "response": v })),
                Err(e) => err(502, e),
            }
        }
        ("GET", p) if p.starts_with("/api/v1/groups/") && p.ends_with("/members") => {
            let gid = p.trim_start_matches("/api/v1/groups/").trim_end_matches("/members").trim_matches('/');
            group_members(session, gid).await
        }
        _ => return None,
    })
}
