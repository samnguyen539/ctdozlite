use super::*;

/// Thread (id, name, is_group) list the poll timer maps incoming messages onto; the GUI
/// republishes it whenever its threads change.
pub(super) static KNOWN_THREADS: std::sync::Mutex<Vec<(String, String, bool)>> =
    std::sync::Mutex::new(Vec::new());

pub enum WorkerCmd {
    /// Read OS authorization without prompting.
    ReadNotificationAuthorization { request_id: u64 },
    /// Request OS authorization following an explicit Settings click.
    RequestNotificationAuthorization { request_id: u64 },
    RequestQr,
    LoginPassword {
        phone: String,
        pass: String,
    },
    RefreshThreads,
    RefreshThreadsManual,
    PollMessages {
        known_threads: Vec<(String, String, bool)>,
    },
    LoadGroupHistory {
        group_id: String,
        is_group: bool,
        /// Messages the GUI already holds. SQLite is read `have + 300` deep, so each
        /// "Tải thêm" reaches further back instead of re-reading the same newest 500.
        have: usize,
    },
    /// Page group / Cloud history back from the server into SQLite: (id, name, is_group).
    SyncHistory {
        threads: Vec<(String, String, bool)>,
    },
    SendMessage {
        to_id: String,
        text: String,
        is_group: bool,
    },
    SendQuote {
        to_id: String,
        text: String,
        quoted: ChatMessage,
        is_group: bool,
    },
    Forward {
        target_id: String,
        target_is_group: bool,
        msg: ChatMessage,
    },
    React {
        to_id: String,
        is_group: bool,
        msg: ChatMessage,
        reaction: String,
    },
    DeleteForMe {
        to_id: String,
        is_group: bool,
        msg: ChatMessage,
    },
    RecallMessage {
        to_id: String,
        is_group: bool,
        msg: ChatMessage,
    },
    SendSticker {
        to_id: String,
        sticker_id: u64,
        cate_id: u64,
        sticker_type: u32,
        is_group: bool,
    },
    PickAndSendAttachment {
        to_id: String,
        is_group: bool,
        images_only: bool,
        caption: String,
        preset_path: Option<std::path::PathBuf>,
    },
    FetchStickerImage {
        key: String,
        url: String,
    },
    FetchStickerStore,
    FetchStickerPack {
        cate_id: u64,
        name: String,
    },
    SearchStickers {
        keyword: String,
    },
    SuggestComposerStickers {
        keyword: String,
    },
    ParseLinkPreview {
        url: String,
    },
    LookupPhone {
        phone: String,
    },
    StartCall {
        partner_id: String,
        partner_name: String,
        partner_avatar: Option<String>,
        is_video: bool,
        is_group: bool,
    },
    AnswerCall {
        accept: bool,
    },
    SetCallMediaState {
        is_on_mic: bool,
        is_on_cam: bool,
    },
    EndCall,
    AddFriend {
        user_id: String,
        user_name: String,
    },
    CheckFriendStatus {
        user_id: String,
    },
    AcceptFriend {
        user_id: String,
        user_name: String,
    },
    RejectFriend {
        user_id: String,
        user_name: String,
    },
    UndoFriend {
        user_id: String,
        user_name: String,
    },
    PersistThreads(Vec<Thread>),
    CheckForUpdates {
        silent: bool,
    },
    DownloadAndApplyUpdate {
        download_url: String,
        asset_name: String,
        target_version: String,
        expected_sha256: Option<String>,
    },
    Logout,
    /// Delete local message data (`zalo_history.db` + `threads_cache.json`) without
    /// touching credentials. Separate from [`WorkerCmd::Logout`] by design (#21).
    WipeLocalData,
    PinThread {
        thread_id: String,
        is_group: bool,
        pin: bool,
    },
    MarkThreadUnread {
        thread_id: String,
        is_group: bool,
        unread: bool,
    },
    MuteThread {
        thread_id: String,
        is_group: bool,
        duration_sec: i64,
        mute: bool,
    },
    HideThread {
        thread_id: String,
        is_group: bool,
        hide: bool,
    },
    DeleteThread {
        thread_id: String,
        is_group: bool,
        last_cli_msg_id: String,
    },
    FetchSocialProfile {
        target_uid: String,
    },
    FetchMySocialAvatars {
        page: u32,
        count: u32,
    },
    FetchTimelineFeed {
        friend_uids: Vec<String>,
        shared_media_by_uid: std::collections::HashMap<String, Vec<crate::feed::FeedMedia>>,
    },
}

pub(super) static WORKER_TX: std::sync::OnceLock<tokio::sync::mpsc::UnboundedSender<WorkerCmd>> =
    std::sync::OnceLock::new();

pub(super) fn extract_msg_id(resp: &serde_json::Value) -> String {
    resp.get("data")
        .and_then(|d| d.get("msgId"))
        .and_then(|v| match v {
            serde_json::Value::String(s) => Some(s.clone()),
            serde_json::Value::Number(n) => Some(n.to_string()),
            _ => None,
        })
        .unwrap_or_default()
}

pub(super) fn resp_error_code(resp: &serde_json::Value) -> Option<i64> {
    resp.get("error_code")
        .and_then(serde_json::Value::as_i64)
        .filter(|c| *c != 0)
}

/// Immediately decode any disk-cached sticker thumbnails (`stickers_cache/stk_{cid}_{sid}.png`)
/// and push `GuiEvent::StickerImageReady` *before* sending sticker list metadata, returning any
/// missing `(key, url, cache_path)` entries for concurrent network prefetching.
pub(super) fn preload_disk_sticker_thumbnails(
    items: &[crate::chat::StickerItem],
    gui_tx: &GuiTx,
    limit: usize,
) -> Vec<(String, String, std::path::PathBuf)> {
    let cache_dir = std::path::Path::new("stickers_cache");
    let _ = std::fs::create_dir_all(cache_dir);

    let mut missing = Vec::new();
    for item in items.iter().take(limit) {
        let key = format!("stk_{}_{}", item.cate_id, item.sticker_id);
        let cache_path = cache_dir.join(format!("{key}.png"));
        if let Ok(disk_bytes) = std::fs::read(&cache_path) {
            if let Ok(dyn_img) = image::load_from_memory(&disk_bytes) {
                let rgba = dyn_img.to_rgba8();
                let size = [rgba.width() as usize, rgba.height() as usize];
                let pixels = rgba.as_flat_samples();
                let color_img = egui::ColorImage::from_rgba_unmultiplied(size, pixels.as_slice());
                let _ = gui_tx.send(GuiEvent::StickerImageReady {
                    key,
                    image: color_img,
                });
                continue;
            } else {
                let _ = std::fs::remove_file(&cache_path);
            }
        }
        let url = if item.cate_id == 0 && (1..=30).contains(&item.sticker_id) {
            format!(
                "https://stc-chat.zdn.vn/images/stickers/default/thumb/{}.png",
                item.sticker_id
            )
        } else {
            format!(
                "https://api.zaloapp.com/api/emoticon/sticker/webpc?eid={}&size=130",
                item.sticker_id
            )
        };
        missing.push((key, url, cache_path));
    }
    missing
}

/// Concurrently download, cache, and decode missing sticker thumbnails so the popover and search
/// grid render smoothly without waiting for frame-by-frame lazy requests.
async fn prefetch_missing_sticker_thumbnails(
    http: &reqwest::Client,
    missing: Vec<(String, String, std::path::PathBuf)>,
    gui_tx: &GuiTx,
    egui_ctx: &egui::Context,
) {
    if missing.is_empty() {
        return;
    }
    let futs = missing.into_iter().map(|(key, url, cache_path)| {
        let client = http.clone();
        let tx = gui_tx.clone();
        let ctx = egui_ctx.clone();
        async move {
            if let Ok(resp) = client.get(&url).send().await {
                if resp.status().is_success() {
                    if let Ok(b) = resp.bytes().await {
                        let vec = b.to_vec();
                        if let Ok(dyn_img) = image::load_from_memory(&vec) {
                            let _ = std::fs::write(&cache_path, &vec);
                            let rgba = dyn_img.to_rgba8();
                            let size = [rgba.width() as usize, rgba.height() as usize];
                            let pixels = rgba.as_flat_samples();
                            let color_img =
                                egui::ColorImage::from_rgba_unmultiplied(size, pixels.as_slice());
                            let _ = tx.send(GuiEvent::StickerImageReady {
                                key,
                                image: color_img,
                            });
                            ctx.request_repaint();
                        }
                    }
                }
            }
        }
    });
    futures_util::future::join_all(futs).await;
}

pub(super) fn spawn_session_worker(session: Session, gui_tx: GuiTx, egui_ctx: egui::Context) {
    let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::unbounded_channel::<WorkerCmd>();
    let _ = WORKER_TX.set(cmd_tx);

    let session = std::sync::Arc::new(tokio::sync::Mutex::new(session));
    let poll_state =
        std::sync::Arc::new(tokio::sync::Mutex::new(crate::chat::PollState::default()));
    let ctrl_poll_state =
        std::sync::Arc::new(tokio::sync::Mutex::new(crate::call::CtrlPollState::default()));

    std::thread::Builder::new()
        .name("zalo-worker".into())
        .spawn(move || {
            // Lightweight 2-thread async runtime to keep memory & CPU footprint minimal
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .unwrap();

            rt.block_on(async move {
                {
                    let s = session.lock().await.clone();
                    let _ = crate::login::get_server_info(&s).await;
                }
                // Message poll timer. It used to live in the GUI's update(), which forced a full
                // repaint every 1.2 s just to fire it. The PollMessages handler skips while a
                // long-poll is in flight and when logged out.
                tokio::spawn(async {
                    let mut tick = tokio::time::interval(Duration::from_millis(1200));
                    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                    loop {
                        tick.tick().await;
                        let known_threads =
                            KNOWN_THREADS.lock().map(|k| k.clone()).unwrap_or_default();
                        if let Some(tx) = WORKER_TX.get() {
                            let _ = tx.send(WorkerCmd::PollMessages { known_threads });
                        }
                    }
                });

                // Scheduled messages worker (#38) + optional HTTP REST API server (#35)
                if let Ok(port_str) = std::env::var("CTDOZLITE_API_PORT") {
                    if let Ok(port) = port_str.parse::<u16>() {
                        if port > 0 {
                            let s_api = session.clone();
                            tokio::spawn(async move {
                                let _ = crate::api_mcp::run_local_http_server(
                                    s_api,
                                    port,
                                    "zalo_history.db".into(),
                                )
                                .await;
                            });
                        }
                    }
                }
                {
                    let s_sched = session.clone();
                    let gui_tx_sched = gui_tx.clone();
                    let ctx_sched = egui_ctx.clone();
                    tokio::spawn(async move {
                        let mut tick = tokio::time::interval(Duration::from_secs(2));
                        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                        loop {
                            tick.tick().await;
                            let now_ts = crate::login::unix_now() as i64;
                            let due = crate::scheduler::claim_due_scheduled_messages(
                                "zalo_history.db",
                                now_ts,
                            )
                            .unwrap_or_default();
                            if due.is_empty() {
                                continue;
                            }
                            let s = s_sched.lock().await.clone();
                            if s.login.is_none() {
                                for item in due {
                                    let _ = crate::scheduler::mark_scheduled_failed(
                                        "zalo_history.db",
                                        item.id,
                                        "Chưa đăng nhập phiên Zalo",
                                    );
                                }
                                let _ = gui_tx_sched.send(GuiEvent::ScheduledListUpdated);
                                ctx_sched.request_repaint();
                                continue;
                            }
                            for item in due {
                                let client_id = crate::chat::next_client_message_id();
                                let send_res = if item.is_group {
                                    crate::chat::send_group_message_with_client_id(&s, &item.thread_id, &item.text, client_id)
                                        .await
                                } else {
                                    crate::chat::send_message_with_client_id(&s, &item.thread_id, &item.text, client_id).await
                                };
                                match send_res {
                                    Ok(resp) => {
                                        if let Some(code) = resp_error_code(&resp) {
                                            let _ = crate::scheduler::mark_scheduled_failed(
                                                "zalo_history.db",
                                                item.id,
                                                &format!("Mã lỗi Zalo {code}"),
                                            );
                                        } else {
                                            let msg_id = extract_msg_id(&resp);
                                            let _ = crate::scheduler::mark_scheduled_sent(
                                                "zalo_history.db",
                                                item.id,
                                            );
                                            let _ = gui_tx_sched.send(GuiEvent::MessageSent {
                                                client_message_id: client_id.to_string(),
                                                thread_id: item.thread_id.clone(),
                                                text: item.text.clone(),
                                                msg_id,
                                            });
                                        }
                                    }
                                    Err(e) => {
                                        let _ = crate::scheduler::mark_scheduled_failed(
                                            "zalo_history.db",
                                            item.id,
                                            &e.to_string(),
                                        );
                                    }
                                }
                            }
                            let _ = gui_tx_sched.send(GuiEvent::ScheduledListUpdated);
                            ctx_sched.request_repaint();
                        }
                    });
                }
                {
                    let s_kw = session.clone();
                    let gui_tx_kw = gui_tx.clone();
                    let ctx_kw = egui_ctx.clone();
                    tokio::spawn(async move {
                        let s = s_kw.lock().await.clone();
                        if s.login.is_some() {
                            if let Some(tx) = WORKER_TX.get() {
                                let _ = tx.send(WorkerCmd::RefreshThreads);
                            }
                            if let Ok(kws) = crate::chat::fetch_sticker_suggest_keywords(&s).await {
                                if !kws.is_empty() {
                                    let _ = gui_tx_kw
                                        .send(GuiEvent::StickerSuggestKeywordsLoaded(kws));
                                    ctx_kw.request_repaint();
                                }
                            }
                        }
                    });
                }

                let call_bridge = std::sync::Arc::new(crate::call::CallBridge::spawn(
                    session.clone(),
                    gui_tx.clone(),
                    egui_ctx.clone(),
                ));

                // 1) Real-time `zpw_ws` WebSocket loop for instant chat messages + VoIP events
                {
                    let s_ws = session.clone();
                    let cb = call_bridge.clone();
                    let gui_tx_ws = gui_tx.clone();
                    let ctx_ws = egui_ctx.clone();
                    tokio::spawn(async move {
                        let mut ws_rx: Option<
                            tokio::sync::mpsc::UnboundedReceiver<crate::socket::WsEvent>,
                        > = None;
                        let mut ws_sek = String::new();
                        loop {
                            let s_snap = s_ws.lock().await.clone();
                            if s_snap.login.is_some() {
                                let cur_sek = s_snap.zpw_sek.clone().unwrap_or_default();
                                if !cur_sek.is_empty() && (ws_rx.is_none() || ws_sek != cur_sek) {
                                    ws_sek = cur_sek;
                                    ws_rx = Some(crate::socket::spawn(s_snap.clone()));
                                }
                                if let Some(rx) = ws_rx.as_mut() {
                                    match tokio::time::timeout(
                                        Duration::from_millis(1500),
                                        rx.recv(),
                                    )
                                    .await
                                    {
                                        Ok(Some(ev)) => {
                                            let data_node = ev
                                                .payload
                                                .get("data")
                                                .unwrap_or(&ev.payload);
                                            for voip_ev in
                                                crate::call::extract_voip_controls(data_node)
                                            {
                                                cb.handle_control_event(voip_ev);
                                            }
                                            if ev.is_chat_ish() {
                                                let mut ws_threads = Vec::new();
                                                if let Some(li) = s_snap.login.as_ref() {
                                                    if !li.send2me_id.is_empty() {
                                                        ws_threads.push(Thread {
                                                            presentation: Default::default(),
                                                            unread: Default::default(),
                                                            id: li.send2me_id.clone(),
                                                            name: "Cloud của tôi (Lưu trữ)"
                                                                .into(),
                                                            preview: String::new(),
                                                            time: String::new(),
                                                            avatar: None,
                                                            is_group: false,
                                                            messages: Vec::new(),
                                                        });
                                                    }
                                                }
                                                crate::chat::merge_previews(
                                                    &mut ws_threads,
                                                    &ev.payload,
                                                );
                                                ws_threads.retain(|t| !t.messages.is_empty());
                                                if !ws_threads.is_empty() {
                                                    crate::chat::resolve_thread_names(
                                                        &s_snap,
                                                        &mut ws_threads,
                                                    )
                                                    .await;
                                                    let _ = gui_tx_ws.send(
                                                        GuiEvent::ThreadsUpdated(ws_threads),
                                                    );
                                                    ctx_ws.request_repaint();
                                                }
                                            }
                                        }
                                        Ok(None) => {
                                            ws_rx = None;
                                            tokio::time::sleep(Duration::from_secs(2)).await;
                                        }
                                        Err(_) => {}
                                    }
                                } else {
                                    tokio::time::sleep(Duration::from_secs(2)).await;
                                }
                            } else {
                                ws_rx = None;
                                ws_sek.clear();
                                tokio::time::sleep(Duration::from_secs(3)).await;
                            }
                        }
                    });
                }

                // 2) Background HTTP long-poll fallback for VoIP control events (`lp_control`)
                {
                    let s_ctrl = session.clone();
                    let cps = ctrl_poll_state.clone();
                    let cb = call_bridge.clone();
                    tokio::spawn(async move {
                        loop {
                            let s_snap = s_ctrl.lock().await.clone();
                            if s_snap.login.is_some() {
                                let voip_res = {
                                    let mut st = cps.lock().await;
                                    crate::call::poll_voip_controls(&s_snap, &mut st).await
                                };
                                if let Ok(events) = voip_res {
                                    for ev in events {
                                        cb.handle_control_event(ev);
                                    }
                                } else {
                                    tokio::time::sleep(Duration::from_secs(5)).await;
                                }
                            } else {
                                tokio::time::sleep(Duration::from_secs(3)).await;
                            }
                        }
                    });
                }

                // 3) Periodic GitHub Releases API update checker (polls every 30 minutes)
                {
                    let gui_tx_upd = gui_tx.clone();
                    let ctx_upd = egui_ctx.clone();
                    tokio::spawn(async move {
                        let mut ticker = tokio::time::interval(Duration::from_secs(30 * 60));
                        // First tick fires immediately; skip it because ZaloGuiApp::new already
                        // dispatches an initial CheckForUpdates on startup.
                        ticker.tick().await;
                        loop {
                            ticker.tick().await;
                            let res = fetch_github_latest_release().await;
                            let _ = gui_tx_upd.send(GuiEvent::UpdateCheckFinished {
                                result: res,
                                silent: true,
                            });
                            ctx_upd.request_repaint();
                        }
                    });
                }

                let mut current_qr_task: Option<tokio::task::JoinHandle<()>> = None;

                while let Some(cmd) = cmd_rx.recv().await {
                    match cmd {
                        WorkerCmd::ReadNotificationAuthorization { request_id }
                        | WorkerCmd::RequestNotificationAuthorization { request_id } => {
                            let request = matches!(
                                cmd, WorkerCmd::RequestNotificationAuthorization { .. }
                            );
                            let tx = gui_tx.clone();
                            tokio::spawn(async move {
                                let result = if request {
                                    crate::native_notifications::request_if_user_action(true).await
                                } else {
                                    crate::native_notifications::status().await
                                };
                                // Closing the UI receiver is normal during shutdown.
                                let _ = tx.send(GuiEvent::NotificationAuthorization {
                                    request_id, result,
                                });
                            });
                        }
                        WorkerCmd::PersistThreads(threads_snapshot) => {
                            tokio::task::spawn_blocking(move || {
                                if let Ok(mut store) =
                                    crate::store::MessageStore::open("zalo_history.db")
                                {
                                    let _ =
                                        crate::store::persist_threads(&mut store, &threads_snapshot);
                                }
                                crate::chat::save_threads_cache(&threads_snapshot);
                            });
                        }
                        WorkerCmd::Logout => {
                            if let Some(task) = current_qr_task.take() {
                                task.abort();
                            }
                            // Credentials and the account-scoped thread cache only:
                            // message history is removed by `WipeLocalData` (#21).
                            let _ = std::fs::remove_file("session.json");
                            let _ = std::fs::remove_file("threads_cache.json");
                            {
                                let mut s = session.lock().await;
                                s.logout();
                            }
                            {
                                let mut ps = poll_state.lock().await;
                                *ps = crate::chat::PollState::default();
                            }
                            let _ = gui_tx.send(GuiEvent::LoggedOut);
                            egui_ctx.request_repaint();
                        }
                        WorkerCmd::WipeLocalData => {
                            let _ = std::fs::remove_file("zalo_history.db");
                            let _ = std::fs::remove_file("threads_cache.json");
                            let _ = gui_tx.send(GuiEvent::LocalDataWiped);
                            egui_ctx.request_repaint();
                        }
                        WorkerCmd::PinThread {
                            thread_id,
                            is_group,
                            pin,
                        } => {
                            let session_clone = session.clone();
                            tokio::spawn(async move {
                                let s = session_clone.lock().await.clone();
                                let _ = crate::chat::update_pinned_conversation(
                                    &s, &thread_id, is_group, pin,
                                )
                                .await;
                            });
                        }
                        WorkerCmd::MarkThreadUnread {
                            thread_id,
                            is_group,
                            unread,
                        } => {
                            let session_clone = session.clone();
                            tokio::spawn(async move {
                                let s = session_clone.lock().await.clone();
                                let _ =
                                    crate::chat::set_unread_mark(&s, &thread_id, is_group, unread)
                                        .await;
                            });
                        }
                        WorkerCmd::MuteThread {
                            thread_id,
                            is_group,
                            duration_sec,
                            mute,
                        } => {
                            let session_clone = session.clone();
                            tokio::spawn(async move {
                                let s = session_clone.lock().await.clone();
                                let _ = crate::chat::set_mute_conversation(
                                    &s,
                                    &thread_id,
                                    is_group,
                                    duration_sec,
                                    mute,
                                )
                                .await;
                            });
                        }
                        WorkerCmd::HideThread {
                            thread_id,
                            is_group,
                            hide,
                        } => {
                            let session_clone = session.clone();
                            tokio::spawn(async move {
                                let s = session_clone.lock().await.clone();
                                let _ = crate::chat::set_hidden_conversation(
                                    &s, &thread_id, is_group, hide,
                                )
                                .await;
                            });
                        }
                        WorkerCmd::DeleteThread {
                            thread_id,
                            is_group,
                            last_cli_msg_id,
                        } => {
                            let session_clone = session.clone();
                            tokio::spawn(async move {
                                let s = session_clone.lock().await.clone();
                                let _ = crate::chat::delete_conversation(
                                    &s,
                                    &thread_id,
                                    is_group,
                                    &last_cli_msg_id,
                                )
                                .await;
                            });
                        }
                        WorkerCmd::RequestQr => {
                            if let Some(task) = current_qr_task.take() {
                                task.abort();
                            }
                            let session_clone = session.clone();
                            let poll_clone = poll_state.clone();
                            let gui_tx_clone = gui_tx.clone();
                            let ctx_clone = egui_ctx.clone();

                            let handle = tokio::spawn(async move {
                                {
                                    let mut s = session_clone.lock().await;
                                    s.logout();
                                }
                                {
                                    let mut ps = poll_clone.lock().await;
                                    *ps = crate::chat::PollState::default();
                                }
                                let _ = gui_tx_clone
                                    .send(GuiEvent::StatusUpdate("Đang xin mã QR mới…".into()));
                                ctx_clone.request_repaint();

                                let s_snap = { session_clone.lock().await.clone() };
                                let q_res = crate::qr::request_qr(&s_snap, "vi").await;

                                match q_res {
                                    Ok(q) => {
                                        if let Some(payload) = q.payload() {
                                            if let Ok(code) = qrcode::QrCode::new(payload.as_bytes())
                                            {
                                                let w = code.width();
                                                let quiet_zone = 4;
                                                let total_w = w + quiet_zone * 2;
                                                let mut pixels =
                                                    Vec::with_capacity(total_w * total_w * 4);
                                                for y in 0..total_w {
                                                    for x in 0..total_w {
                                                        let in_qr = x >= quiet_zone
                                                            && x < quiet_zone + w
                                                            && y >= quiet_zone
                                                            && y < quiet_zone + w;
                                                        let dark = in_qr
                                                            && (code[(
                                                                x - quiet_zone,
                                                                y - quiet_zone,
                                                            )] == qrcode::Color::Dark);
                                                        let c = if dark {
                                                            [15, 23, 42, 255]
                                                        } else {
                                                            [255, 255, 255, 255]
                                                        };
                                                        pixels.extend_from_slice(&c);
                                                    }
                                                }
                                                let img = egui::ColorImage::from_rgba_unmultiplied(
                                                    [total_w, total_w],
                                                    &pixels,
                                                );
                                                let _ = gui_tx_clone.send(GuiEvent::QrReady {
                                                    qr_login: q.clone(),
                                                    qr_image: img,
                                                });
                                                ctx_clone.request_repaint();

                                                let scan_res =
                                                    crate::qr::wait_for_scan(&s_snap, &q).await;

                                                if let Ok(scanned) = scan_res {
                                                    let name = scanned
                                                        .get("display_name")
                                                        .and_then(|v| v.as_str())
                                                        .unwrap_or("Zalo User");
                                                    let _ =
                                                        gui_tx_clone.send(GuiEvent::QrScanned {
                                                            name: name.to_string(),
                                                        });
                                                    ctx_clone.request_repaint();

                                                    let confirm_res =
                                                        crate::qr::confirm(&s_snap, &q).await;

                                                    if let Ok(resp) = confirm_res {
                                                        let mut s = session_clone.lock().await;
                                                        if let Some(sek) =
                                                            crate::qr::extract_zpw_sek(&resp)
                                                        {
                                                            s.set_zpw_sek(sek);
                                                        }
                                                        s.sync_live_sek();
                                                        if let Some(data) = resp.get("data") {
                                                            crate::login::apply_login_config(
                                                                &mut s, data,
                                                            );
                                                        }
                                                        let _ = s.save_to_file("session.json");

                                                        if let Ok(info) =
                                                            crate::login::get_login_info(&s).await
                                                        {
                                                            if let Some(sek) =
                                                                crate::qr::extract_zpw_sek(&info)
                                                            {
                                                                s.set_zpw_sek(sek);
                                                            }
                                                            s.sync_live_sek();
                                                            if let Some(cfg) = info.get("data") {
                                                                crate::login::apply_login_config(
                                                                    &mut s, cfg,
                                                                );
                                                                s.apply_server_info(cfg);
                                                            }
                                                        }
                                                        s.sync_live_sek();
                                                        let _ = s.sync_profile_me().await;
                                                        s.sync_live_sek();
                                                        if let Some(li) = &mut s.login {
                                                            if li.display_name.is_empty()
                                                                && !name.is_empty()
                                                            {
                                                                li.display_name = name.to_string();
                                                            }
                                                        }
                                                        let _ = s.save_to_file("session.json");
                                                        let uin = s
                                                            .login
                                                            .as_ref()
                                                            .map(|l| l.uin.clone())
                                                            .unwrap_or_default();
                                                        let dname = s
                                                            .login
                                                            .as_ref()
                                                            .map(|l| l.display_name.clone())
                                                            .unwrap_or_default();
                                                        let u_ava = s
                                                            .login
                                                            .as_ref()
                                                            .map(|l| l.avatar.clone())
                                                            .filter(|a| !a.is_empty());
                                                        let _ = gui_tx_clone.send(
                                                            GuiEvent::LoginSuccess {
                                                                name: dname,
                                                                uin,
                                                                avatar: u_ava,
                                                            },
                                                        );
                                                        ctx_clone.request_repaint();

                                                        let mut list = crate::chat::get_friends(&s)
                                                            .await
                                                            .unwrap_or_default();
                                                        crate::store::load_history_into_threads(
                                                            "zalo_history.db",
                                                            &mut list,
                                                            crate::store::MessageStore::PRELOAD_MSGS,
                                                        );
                                                        {
                                                            let mut ps = poll_clone.lock().await;
                                                            let _ = crate::chat::poll_new_messages(
                                                                &s, &mut ps, &mut list,
                                                            )
                                                            .await;
                                                        }
                                                        let _ = gui_tx_clone
                                                            .send(GuiEvent::ThreadsUpdated(list));
                                                        ctx_clone.request_repaint();

                                                        let s_kw = s.clone();
                                                        let tx_kw = gui_tx_clone.clone();
                                                        let ctx_kw = ctx_clone.clone();
                                                        tokio::spawn(async move {
                                                            if let Ok(kws) = crate::chat::fetch_sticker_suggest_keywords(&s_kw).await {
                                                                if !kws.is_empty() {
                                                                    let _ = tx_kw.send(GuiEvent::StickerSuggestKeywordsLoaded(kws));
                                                                    ctx_kw.request_repaint();
                                                                }
                                                            }
                                                        });
                                                    }
                                                }
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        let _ = gui_tx_clone
                                            .send(GuiEvent::LoginFailed(e.to_string()));
                                        ctx_clone.request_repaint();
                                    }
                                }
                            });
                            current_qr_task = Some(handle);
                        }
                        ref cmd @ (WorkerCmd::RefreshThreads | WorkerCmd::RefreshThreadsManual) => {
                            let manual = matches!(cmd, WorkerCmd::RefreshThreadsManual);
                            let session_clone = session.clone();
                            let poll_clone = poll_state.clone();
                            let gui_tx_clone = gui_tx.clone();
                            let ctx_clone = egui_ctx.clone();
                            tokio::spawn(async move {
                                let mut s = session_clone.lock().await.clone();
                                if s.login.is_some() {
                                    match s.session_status().await {
                                        crate::session::SessionStatus::Valid => {}
                                        crate::session::SessionStatus::Expired => {
                                            // Keep `session.json`: the old credentials stay
                                            // readable until a new login replaces the file (#21).
                                            {
                                                let mut sl = session_clone.lock().await;
                                                sl.mark_expired();
                                                let _ = sl.save_to_file("session.json");
                                                sl.logout();
                                            }
                                            let _ = gui_tx_clone.send(GuiEvent::SessionExpired(
                                                "Phiên đăng nhập (zpw_sek) đã hết hạn trên máy chủ Zalo"
                                                    .into(),
                                            ));
                                            ctx_clone.request_repaint();
                                            if let Some(tx) = WORKER_TX.get() {
                                                let _ = tx.send(WorkerCmd::RequestQr);
                                            }
                                            return;
                                        }
                                        crate::session::SessionStatus::Unreachable => {
                                            // Offline: keep the session, say so, and let the
                                            // next poll retry.
                                            crate::logger::log_error(
                                                "gui::refresh_threads",
                                                "không kết nối được máy chủ Zalo",
                                            );
                                            let _ = gui_tx_clone.send(GuiEvent::SessionUnreachable(
                                                "Không kết nối được máy chủ Zalo".into(),
                                            ));
                                            ctx_clone.request_repaint();
                                            return;
                                        }
                                    }
                                    if let Some((uid, name)) = s.sync_profile_me().await {
                                        {
                                            let mut sl = session_clone.lock().await;
                                            if let Some(ref mut li) = sl.login {
                                                li.uin = uid.clone();
                                                if !name.is_empty() {
                                                    li.display_name = name.clone();
                                                }
                                            }
                                            let _ = sl.save_to_file("session.json");
                                        }
                                        let u_ava = s
                                            .login
                                            .as_ref()
                                            .map(|l| l.avatar.clone())
                                            .filter(|a| !a.is_empty());
                                        let _ = gui_tx_clone.send(GuiEvent::LoginSuccess {
                                            name,
                                            uin: uid,
                                            avatar: u_ava,
                                        });
                                    }
                                    let _ = gui_tx_clone.send(GuiEvent::StatusUpdate(
                                        "⏳ Đang làm mới danh bạ & nhóm từ máy chủ Zalo (1/3)…".into(),
                                    ));
                                    ctx_clone.request_repaint();
                                    let mut list = crate::chat::load_threads_cache();
                                    let mut errors = Vec::new();
                                    match crate::chat::get_friends(&s).await {
                                        Ok(friends) => {
                                            for ft in friends {
                                                if let Some(ex) = list.iter_mut().find(|t| t.id == ft.id) {
                                                    if !ft.name.is_empty() {
                                                        ex.name = ft.name;
                                                    }
                                                    if crate::chat::should_replace_avatar(
                                                        ex.avatar.as_deref(),
                                                        ft.avatar.as_deref(),
                                                    ) {
                                                        ex.avatar = ft.avatar;
                                                    }
                                                    ex.is_group = ex.is_group || ft.is_group;
                                                } else {
                                                    list.push(ft);
                                                }
                                            }
                                        }
                                        Err(e) => {
                                            crate::logger::log_error("gui::refresh_friends", &e.to_string());
                                            errors.push(format!("Danh bạ: {e}"));
                                        }
                                    }
                                    let _ = gui_tx_clone.send(GuiEvent::StatusUpdate(
                                        "⏳ Đang tải danh sách nhóm Zalo (2/3)…".into(),
                                    ));
                                    ctx_clone.request_repaint();
                                    match crate::chat::get_groups(&s).await {
                                        Ok(groups) => {
                                            for gt in groups {
                                                if let Some(ex) = list.iter_mut().find(|t| t.id == gt.id) {
                                                    if !gt.name.is_empty() {
                                                        ex.name = gt.name;
                                                    }
                                                    if crate::chat::should_replace_avatar(
                                                        ex.avatar.as_deref(),
                                                        gt.avatar.as_deref(),
                                                    ) {
                                                        ex.avatar = gt.avatar;
                                                    }
                                                    ex.is_group = true;
                                                } else {
                                                    list.push(gt);
                                                }
                                            }
                                        }
                                        Err(e) => {
                                            crate::logger::log_error("gui::refresh_groups", &e.to_string());
                                            errors.push(format!("Nhóm: {e}"));
                                        }
                                    }
                                    crate::store::load_history_into_threads(
                                        "zalo_history.db",
                                        &mut list,
                                        crate::store::MessageStore::PRELOAD_MSGS,
                                    );
                                    let _ = gui_tx_clone.send(GuiEvent::StatusUpdate(
                                        format!("⏳ Đang làm mới tin nhắn cho {} hội thoại (3/3)…", list.len()),
                                    ));
                                    ctx_clone.request_repaint();
                                    let mut fresh_ps = crate::chat::PollState::default();
                                    if let Err(e) =
                                        crate::chat::poll_new_messages(&s, &mut fresh_ps, &mut list)
                                            .await
                                    {
                                        crate::logger::log_error("gui::refresh_poll", &e.to_string());
                                        errors.push(format!("Tin nhắn: {e}"));
                                    }
                                    {
                                        let mut ps = poll_clone.lock().await;
                                        *ps = fresh_ps;
                                    }
                                    let total_msgs: usize =
                                        list.iter().map(|t| t.messages.len()).sum();
                                    let n_threads = list.len();
                                    let _ = gui_tx_clone.send(GuiEvent::ThreadsUpdated(list));
                                    let _ = gui_tx_clone.send(GuiEvent::RefreshFinished {
                                        threads: n_threads,
                                        messages: total_msgs,
                                        errors,
                                        manual,
                                    });
                                    ctx_clone.request_repaint();
                                }
                            });
                        }
                        WorkerCmd::PollMessages { known_threads } => {
                            static IS_POLLING: std::sync::atomic::AtomicBool =
                                std::sync::atomic::AtomicBool::new(false);
                            if IS_POLLING.swap(true, std::sync::atomic::Ordering::SeqCst) {
                                continue;
                            }

                            let session_clone = session.clone();
                            let poll_clone = poll_state.clone();
                            let gui_tx_clone = gui_tx.clone();
                            let ctx_clone = egui_ctx.clone();
                            tokio::spawn(async move {
                                let s = session_clone.lock().await.clone();
                                if s.login.is_some() {
                                    let mut temp_threads: Vec<Thread> = known_threads
                                        .into_iter()
                                        .map(|(id, name, is_group)| Thread {
                                            presentation: Default::default(),
                                            unread: Default::default(),
                                            id,
                                            name,
                                            preview: String::new(),
                                            time: String::new(),
                                            avatar: None,
                                            is_group,
                                            messages: Vec::new(),
                                        })
                                        .collect();

                                    let mut poll_err: Option<anyhow::Error> = None;
                                    for _ in 0..3 {
                                        let poll_res = {
                                            let mut ps = poll_clone.lock().await;
                                            crate::chat::get_chat_from_polling(&s, &mut ps).await
                                        };
                                        match poll_res {
                                            Ok(lp_resp) => {
                                                crate::chat::merge_previews(
                                                    &mut temp_threads,
                                                    &lp_resp,
                                                );
                                                let data_node =
                                                    lp_resp.get("data").unwrap_or(&lp_resp);
                                                let more = data_node
                                                    .get("more")
                                                    .and_then(serde_json::Value::as_i64)
                                                    .unwrap_or(0);
                                                let has_reset = data_node
                                                    .get("resetLastActionId")
                                                    .and_then(serde_json::Value::as_str)
                                                    .map(|r| r != "0" && !r.is_empty())
                                                    .unwrap_or(false);
                                                if more == 0 && !has_reset {
                                                    break;
                                                }
                                            }
                                            Err(e) => {
                                                poll_err = Some(e);
                                                break;
                                            }
                                        }
                                    }

                                    match poll_err {
                                        Some(e) => {
                                            if crate::session::SessionStatus::from_error(&e)
                                                == crate::session::SessionStatus::Expired
                                            {
                                                {
                                                    let mut sl = session_clone.lock().await;
                                                    sl.mark_expired();
                                                    let _ = sl.save_to_file("session.json");
                                                    sl.logout();
                                                }
                                                let _ = gui_tx_clone.send(GuiEvent::SessionExpired(
                                                    "Phiên đăng nhập (zpw_sek) đã hết hạn".into(),
                                                ));
                                                ctx_clone.request_repaint();
                                                if let Some(tx) = WORKER_TX.get() {
                                                    let _ = tx.send(WorkerCmd::RequestQr);
                                                }
                                            } else {
                                                // The session is untouched: report and retry on
                                                // the next poll instead of logging the user out.
                                                crate::logger::log_error(
                                                    "gui::poll_messages",
                                                    &e.to_string(),
                                                );
                                                let _ = gui_tx_clone.send(
                                                    GuiEvent::SessionUnreachable(e.to_string()),
                                                );
                                                ctx_clone.request_repaint();
                                            }
                                        }
                                        None => {
                                            temp_threads.retain(|t| !t.messages.is_empty());
                                            if !temp_threads.is_empty() {
                                                crate::chat::resolve_thread_names(
                                                    &s,
                                                    &mut temp_threads,
                                                )
                                                .await;
                                                let rules = crate::api_mcp::load_autoreply_rules();
                                                if rules.iter().any(|r| r.enabled) {
                                                    for t in &temp_threads {
                                                        for m in &t.messages {
                                                            if !m.is_outgoing {
                                                                if let Some(matched) = crate::api_mcp::match_autoreply_rule(&rules, &m.text, t.is_group) {
                                                                    let reply_text = matched.reply_text;
                                                                    let client_id = crate::chat::next_client_message_id();
                                                                    let res = if t.is_group {
                                                                        crate::chat::send_group_message_with_client_id(&s, &t.id, &reply_text, client_id).await
                                                                    } else {
                                                                        crate::chat::send_message_with_client_id(&s, &t.id, &reply_text, client_id).await
                                                                    };
                                                                    if let Ok(resp) = res {
                                                                        if resp_error_code(&resp).is_none() {
                                                                            let msg_id = extract_msg_id(&resp);
                                                                            let _ = gui_tx_clone.send(GuiEvent::MessageSent {
                                                                                client_message_id: client_id.to_string(),
                                                                                thread_id: t.id.clone(),
                                                                                text: reply_text,
                                                                                msg_id,
                                                                            });
                                                                        }
                                                                    }
                                                                }
                                                            }
                                                        }
                                                    }
                                                }
                                                let _ = gui_tx_clone
                                                    .send(GuiEvent::ThreadsUpdated(temp_threads));
                                                ctx_clone.request_repaint();
                                            }
                                        }
                                    }
                                }
                                IS_POLLING.store(false, std::sync::atomic::Ordering::SeqCst);
                            });
                        }
                        WorkerCmd::SyncHistory { threads } => {
                            use std::sync::atomic::Ordering;
                            if crate::history_sync::RUNNING.swap(true, Ordering::SeqCst) {
                                continue;
                            }
                            crate::history_sync::CANCEL.store(false, Ordering::Relaxed);
                            let session_clone = session.clone();
                            let gui_tx_clone = gui_tx.clone();
                            let ctx_clone = egui_ctx.clone();
                            tokio::spawn(async move {
                                let s = session_clone.lock().await.clone();
                                let total = threads.len();
                                let mut added = 0;
                                let mut errors = Vec::new();
                                for (i, (id, name, is_group)) in threads.into_iter().enumerate() {
                                    if crate::history_sync::CANCEL.load(Ordering::Relaxed) {
                                        break;
                                    }
                                    let _ = gui_tx_clone.send(GuiEvent::StatusUpdate(format!(
                                        "Đang đồng bộ {}/{total} · {name}",
                                        i + 1
                                    )));
                                    ctx_clone.request_repaint();
                                    let mut t = Thread {
                                        id,
                                        name,
                                        preview: String::new(),
                                        time: String::new(),
                                        avatar: None,
                                        is_group,
                                        messages: Vec::new(),
                                        ..Default::default()
                                    };
                                    match crate::history_sync::sync_thread(&s, &mut t).await {
                                        Ok(n) => added += n,
                                        Err(e) => errors.push(format!("{}: {e}", t.name)),
                                    }
                                }
                                let _ = gui_tx_clone.send(GuiEvent::SyncFinished {
                                    added,
                                    errors,
                                    cancelled: crate::history_sync::CANCEL.load(Ordering::Relaxed),
                                });
                                ctx_clone.request_repaint();
                                crate::history_sync::RUNNING.store(false, Ordering::SeqCst);
                            });
                        }
                        WorkerCmd::LoadGroupHistory {
                            group_id,
                            is_group,
                            have,
                        } => {
                            let session_clone = session.clone();
                            let gui_tx_clone = gui_tx.clone();
                            let ctx_clone = egui_ctx.clone();
                            tokio::spawn(async move {
                                let s = session_clone.lock().await.clone();
                                if s.login.is_some() {
                                    let is_send2me = s
                                        .login
                                        .as_ref()
                                        .map(|l| !l.send2me_id.is_empty() && l.send2me_id == group_id)
                                        .unwrap_or(false);
                                    let t = Thread {
                                        presentation: Default::default(),
                                        unread: Default::default(),
                                        id: group_id.clone(),
                                        name: String::new(),
                                        preview: String::new(),
                                        time: String::new(),
                                        avatar: None,
                                        is_group: is_group && !is_send2me,
                                        messages: Vec::new(),
                                    };
                                    let mut t_list = vec![t];
                                    crate::store::load_history_into_threads(
                                        "zalo_history.db",
                                        &mut t_list,
                                        (have + 300).max(500),
                                    );
                                    let Some(mut t) =
                                        t_list.into_iter().find(|x| x.id == group_id)
                                    else {
                                        return;
                                    };
                                    if !is_group && !is_send2me {
                                        let mut one_list = vec![t.clone()];
                                        if let Ok(resp) = crate::chat::get_last_msgs(
                                            &s,
                                            std::slice::from_ref(&group_id),
                                        )
                                        .await
                                        {
                                            crate::chat::merge_previews(&mut one_list, &resp);
                                        }
                                        if let Ok(resp_all) =
                                            crate::chat::get_last_msgs(&s, &[]).await
                                        {
                                            crate::chat::merge_previews(&mut one_list, &resp_all);
                                        }
                                        if let Some(updated) =
                                            one_list.into_iter().find(|x| x.id == group_id)
                                        {
                                            t = updated;
                                        }
                                    }
                                    if is_group || is_send2me {
                                        if let Ok(cm_resp) = crate::chat::get_cloud_messages(
                                            &s,
                                            &group_id,
                                            "9223372036854775808",
                                            false,
                                            50,
                                        )
                                        .await
                                        {
                                            crate::chat::merge_group_history(&mut t, &cm_resp);
                                        }
                                        // Backfill up to 5 pages (250 messages) of older cloud history
                                        let mut cursor_id = t
                                            .messages
                                            .first()
                                            .map(|m| m.id.as_str())
                                            .filter(|id| {
                                                !id.is_empty()
                                                    && id.chars().all(|c| c.is_ascii_digit())
                                            })
                                            .unwrap_or("9223372036854775808")
                                            .to_string();
                                        for page in 1..=5 {
                                            let prev_len = t.messages.len();
                                            let Ok(cm_old) = crate::chat::get_cloud_messages(
                                                &s,
                                                &group_id,
                                                &cursor_id,
                                                true,
                                                50,
                                            )
                                            .await
                                            else {
                                                break;
                                            };
                                            crate::chat::merge_group_history(&mut t, &cm_old);
                                            let new_len = t.messages.len();
                                            let _ = gui_tx_clone.send(GuiEvent::StatusUpdate(
                                                format!(
                                                    "⏳ Đang đồng bộ lịch sử cũ (trang {page}/5 — {new_len} tin nhắn)…"
                                                ),
                                            ));
                                            ctx_clone.request_repaint();
                                            let data_node = cm_old
                                                .pointer("/data/data")
                                                .or_else(|| cm_old.get("data"))
                                                .unwrap_or(&cm_old);
                                            let has_more = data_node
                                                .get("hasMore")
                                                .and_then(|v| v.as_i64())
                                                .unwrap_or(0);
                                            let next_cursor = data_node
                                                .get("lastMsgId")
                                                .and_then(|v| {
                                                    v.as_str().map(str::to_string).or_else(|| {
                                                        v.as_u64().map(|n| n.to_string())
                                                    })
                                                })
                                                .filter(|s| {
                                                    !s.is_empty()
                                                        && s != "0"
                                                        && s.chars().all(|c| c.is_ascii_digit())
                                                })
                                                .or_else(|| {
                                                    t.messages
                                                        .first()
                                                        .map(|m| m.id.clone())
                                                        .filter(|id| {
                                                            !id.is_empty()
                                                                && id
                                                                    .chars()
                                                                    .all(|c| c.is_ascii_digit())
                                                        })
                                                });
                                            let Some(nc) = next_cursor else {
                                                break;
                                            };
                                            if has_more == 0 || nc == cursor_id || new_len == prev_len {
                                                break;
                                            }
                                            cursor_id = nc;
                                        }
                                    }
                                    if is_group
                                        && !is_send2me
                                        && (crate::chat::is_generic_thread_name(&t.name)
                                            || t.avatar.as_deref().unwrap_or("").is_empty())
                                    {
                                        crate::chat::resolve_thread_names(
                                            &s,
                                            std::slice::from_mut(&mut t),
                                        )
                                        .await;
                                    }
                                    let cnt = t.messages.len();
                                    if !t.messages.is_empty() || !t.name.is_empty() {
                                        if let Ok(mut store) =
                                            crate::store::MessageStore::open("zalo_history.db")
                                        {
                                            let _ = crate::store::persist_threads(
                                                &mut store,
                                                std::slice::from_ref(&t),
                                            );
                                        }
                                        let _ = gui_tx_clone.send(GuiEvent::ThreadsUpdated(vec![t]));
                                    }
                                    let _ = gui_tx_clone.send(GuiEvent::StatusUpdate(format!(
                                        "✅ Đã đồng bộ {cnt} tin nhắn lịch sử"
                                    )));
                                    ctx_clone.request_repaint();
                                }
                            });
                        }
                        WorkerCmd::SendMessage {
                            to_id,
                            text,
                            is_group,
                        } => {
                            let session_clone = session.clone();
                            let gui_tx_clone = gui_tx.clone();
                            let ctx_clone = egui_ctx.clone();
                            tokio::spawn(async move {
                                let s = session_clone.lock().await.clone();
                                let client_id = crate::chat::next_client_message_id();
                                if let Some(found_url) = crate::chat::extract_first_url(&text) {
                                    let meta = crate::chat::parse_link_metadata(&s, &found_url)
                                        .await
                                        .unwrap_or_else(|_| crate::chat::LinkMetadata {
                                            url: found_url.clone(),
                                            title: found_url.clone(),
                                            desc: String::new(),
                                            thumb: String::new(),
                                            src: "Liên kết".into(),
                                        });
                                    let _ =
                                        gui_tx_clone.send(GuiEvent::LinkPreviewReady(meta.clone()));
                                    match crate::chat::send_link_message_with_client_id(
                                        &s, &to_id, &text, &meta, is_group, client_id)
                                    .await
                                    {
                                        Ok(resp) => {
                                            let msg_id = extract_msg_id(&resp);
                                            let now = crate::ui::clock();
                                            let final_id = if msg_id.is_empty() {
                                                format!("out_{client_id}")
                                            } else {
                                                msg_id
                                            };
                                            let msg = ChatMessage {
                                                id: final_id,
                                                client_message_id: client_id.to_string(),
                                                sender: "Tôi".into(),
                                                text,
                                                time: now,
                                                is_outgoing: true,
                                                ts: (crate::login::unix_now() * 1000) as i64,
                                                link_url: Some(meta.url),
                                                link_title: if meta.title.is_empty() {
                                                    None
                                                } else {
                                                    Some(meta.title)
                                                },
                                                link_desc: if meta.desc.is_empty() {
                                                    None
                                                } else {
                                                    Some(meta.desc)
                                                },
                                                link_thumb: if meta.thumb.is_empty() {
                                                    None
                                                } else {
                                                    Some(meta.thumb)
                                                },
                                                link_src: if meta.src.is_empty() {
                                                    None
                                                } else {
                                                    Some(meta.src)
                                                },
                                                ..Default::default()
                                            };
                                            let _ = gui_tx_clone.send(GuiEvent::AttachmentSent {
                                                thread_id: to_id,
                                                msg,
                                            });
                                            ctx_clone.request_repaint();
                                            return;
                                        }
                                        Err(e) => {
                                            let err_s = e.to_string();
                                            crate::logger::log_error("gui::send_link", &err_s);
                                            let _ = gui_tx_clone.send(GuiEvent::SendFailed {
                                                thread_id: to_id,
                                                text,
                                                error: err_s,
                                            });
                                            ctx_clone.request_repaint();
                                            return;
                                        }
                                    }
                                }

                                let res = if is_group {
                                    crate::chat::send_group_message_with_client_id(&s, &to_id, &text, client_id).await
                                } else {
                                    crate::chat::send_message_with_client_id(&s, &to_id, &text, client_id).await
                                };
                                match res {
                                    Ok(resp) => {
                                        let msg_id = extract_msg_id(&resp);
                                        let _ = gui_tx_clone.send(GuiEvent::MessageSent {
                                            client_message_id: client_id.to_string(),
                                            thread_id: to_id,
                                            text,
                                            msg_id,
                                        });
                                        ctx_clone.request_repaint();
                                    }
                                    Err(e) => {
                                        let err_s = e.to_string();
                                        crate::logger::log_error("gui::send_message", &err_s);
                                        let _ = gui_tx_clone.send(GuiEvent::SendFailed {
                                            thread_id: to_id,
                                            text,
                                            error: err_s.clone(),
                                        });
                                        if crate::session::SessionStatus::from_error(&e)
                                            == crate::session::SessionStatus::Expired
                                        {
                                            {
                                                let mut sl = session_clone.lock().await;
                                                sl.mark_expired();
                                                let _ = sl.save_to_file("session.json");
                                                sl.logout();
                                            }
                                            let _ = gui_tx_clone.send(GuiEvent::SessionExpired(
                                                "Không thể gửi vì phiên zpw_sek đã hết hạn".into(),
                                            ));
                                            if let Some(tx) = WORKER_TX.get() {
                                                let _ = tx.send(WorkerCmd::RequestQr);
                                            }
                                        }
                                        ctx_clone.request_repaint();
                                    }
                                }
                            });
                        }
                        WorkerCmd::SendQuote {
                            to_id,
                            text,
                            quoted,
                            is_group,
                        } => {
                            let session_clone = session.clone();
                            let gui_tx_clone = gui_tx.clone();
                            let ctx_clone = egui_ctx.clone();
                            tokio::spawn(async move {
                                let s = session_clone.lock().await.clone();
                                let client_id = crate::chat::next_client_message_id();
                                match crate::chat::send_quote_message_with_client_id(
                                    &s, &to_id, &text, &quoted, is_group, client_id)
                                .await
                                {
                                    Ok(resp) => {
                                        let _ = gui_tx_clone.send(GuiEvent::QuoteSent {
                                            client_message_id: client_id.to_string(),
                                            thread_id: to_id,
                                            text,
                                            msg_id: extract_msg_id(&resp),
                                            quoted,
                                        });
                                    }
                                    Err(e) => {
                                        let err_s = e.to_string();
                                        crate::logger::log_error("gui::send_quote", &err_s);
                                        let _ = gui_tx_clone.send(GuiEvent::SendFailed {
                                            thread_id: to_id,
                                            text,
                                            error: err_s,
                                        });
                                    }
                                }
                                ctx_clone.request_repaint();
                            });
                        }
                        WorkerCmd::Forward {
                            target_id,
                            target_is_group,
                            msg,
                        } => {
                            let session_clone = session.clone();
                            let gui_tx_clone = gui_tx.clone();
                            let ctx_clone = egui_ctx.clone();
                            tokio::spawn(async move {
                                let s = session_clone.lock().await.clone();
                                let client_id = crate::chat::next_client_message_id();
                                let res = crate::chat::forward_message_with_client_id(
                                    &s,
                                    &target_id,
                                    target_is_group,
                                    &msg, client_id)
                                .await;
                                match res {
                                    Ok(resp) if resp_error_code(&resp).is_none() => {
                                        let msg_id = extract_msg_id(&resp);
                                        if let Some(sid) = msg.sticker_id.filter(|v| *v > 0) {
                                            let _ = gui_tx_clone.send(GuiEvent::StickerSent {
                                                client_message_id: client_id.to_string(),
                                                thread_id: target_id,
                                                msg_id,
                                                sticker_id: sid,
                                                cate_id: msg.sticker_cate_id.unwrap_or(0),
                                                sticker_type: msg.sticker_type.unwrap_or(7),
                                            });
                                        } else {
                                            let _ = gui_tx_clone.send(GuiEvent::MessageSent {
                                                client_message_id: client_id.to_string(),
                                                thread_id: target_id,
                                                text: msg.text.clone(),
                                                msg_id,
                                            });
                                        }
                                        let _ = gui_tx_clone.send(GuiEvent::StatusUpdate(
                                            "Đã chuyển tiếp tin nhắn".into(),
                                        ));
                                    }
                                    Ok(resp) => {
                                        let err_s = format!(
                                            "server error {}",
                                            resp_error_code(&resp).unwrap_or(0)
                                        );
                                        crate::logger::log_error("gui::forward", &err_s);
                                        let _ = gui_tx_clone.send(GuiEvent::StatusUpdate(format!(
                                            "Chuyển tiếp thất bại: {err_s}"
                                        )));
                                    }
                                    Err(e) => {
                                        crate::logger::log_error("gui::forward", &e.to_string());
                                        let _ = gui_tx_clone.send(GuiEvent::StatusUpdate(format!(
                                            "Chuyển tiếp thất bại: {e}"
                                        )));
                                    }
                                }
                                ctx_clone.request_repaint();
                            });
                        }
                        WorkerCmd::React {
                            to_id,
                            is_group,
                            msg,
                            reaction,
                        } => {
                            let session_clone = session.clone();
                            let gui_tx_clone = gui_tx.clone();
                            let ctx_clone = egui_ctx.clone();
                            tokio::spawn(async move {
                                let s = session_clone.lock().await.clone();
                                let res = crate::chat::send_reaction(
                                    &s, &to_id, is_group, &msg, &reaction,
                                )
                                .await;
                                match res {
                                    Ok(resp) if resp_error_code(&resp).is_none() => {
                                        let _ = gui_tx_clone.send(GuiEvent::Reacted {
                                            thread_id: to_id,
                                            msg_id: msg.id.clone(),
                                            ts: msg.ts,
                                            reaction,
                                        });
                                    }
                                    Ok(resp) => {
                                        let err_s = format!(
                                            "server error {}",
                                            resp_error_code(&resp).unwrap_or(0)
                                        );
                                        crate::logger::log_error("gui::react", &err_s);
                                        let _ = gui_tx_clone.send(GuiEvent::StatusUpdate(format!(
                                            "Thả cảm xúc thất bại: {err_s}"
                                        )));
                                    }
                                    Err(e) => {
                                        crate::logger::log_error("gui::react", &e.to_string());
                                        let _ = gui_tx_clone.send(GuiEvent::StatusUpdate(format!(
                                            "Thả cảm xúc thất bại: {e}"
                                        )));
                                    }
                                }
                                ctx_clone.request_repaint();
                            });
                        }
                        WorkerCmd::DeleteForMe {
                            to_id,
                            is_group,
                            msg,
                        } => {
                            let session_clone = session.clone();
                            let gui_tx_clone = gui_tx.clone();
                            let ctx_clone = egui_ctx.clone();
                            tokio::spawn(async move {
                                let s = session_clone.lock().await.clone();
                                let local_only = msg.id.is_empty()
                                    || msg.id.starts_with("out_")
                                    || msg.id.starts_with("local_");
                                let res: anyhow::Result<serde_json::Value> = if local_only {
                                    Ok(serde_json::json!({}))
                                } else {
                                    crate::chat::delete_message_for_me(&s, &to_id, is_group, &msg)
                                        .await
                                };
                                match res {
                                    Ok(resp) if resp_error_code(&resp).is_none() => {
                                        let (tid, mid, ts) =
                                            (to_id.clone(), msg.id.clone(), msg.ts);
                                        let _ = tokio::task::spawn_blocking(move || {
                                            if let Ok(mut store) =
                                                crate::store::MessageStore::open("zalo_history.db")
                                            {
                                                let _ = store.delete_message(&tid, &mid, ts);
                                            }
                                        })
                                        .await;
                                        let _ = gui_tx_clone.send(GuiEvent::MessageDeleted {
                                            thread_id: to_id,
                                            msg_id: msg.id.clone(),
                                            ts: msg.ts,
                                        });
                                    }
                                    Ok(resp) => {
                                        let err_s = format!(
                                            "server error {}",
                                            resp_error_code(&resp).unwrap_or(0)
                                        );
                                        crate::logger::log_error("gui::delete", &err_s);
                                        let _ = gui_tx_clone.send(GuiEvent::StatusUpdate(format!(
                                            "Xóa tin nhắn thất bại: {err_s}"
                                        )));
                                    }
                                    Err(e) => {
                                        crate::logger::log_error("gui::delete", &e.to_string());
                                        let _ = gui_tx_clone.send(GuiEvent::StatusUpdate(format!(
                                            "Xóa tin nhắn thất bại: {e}"
                                        )));
                                    }
                                }
                                ctx_clone.request_repaint();
                            });
                        }
                        WorkerCmd::RecallMessage {
                            to_id,
                            is_group,
                            msg,
                        } => {
                            let session_clone = session.clone();
                            let gui_tx_clone = gui_tx.clone();
                            let ctx_clone = egui_ctx.clone();
                            tokio::spawn(async move {
                                let s = session_clone.lock().await.clone();
                                let local_only = msg.id.is_empty()
                                    || msg.id.starts_with("out_")
                                    || msg.id.starts_with("local_");
                                let res: anyhow::Result<serde_json::Value> = if local_only {
                                    Ok(serde_json::json!({}))
                                } else {
                                    crate::chat::recall_message(&s, &to_id, is_group, &msg).await
                                };
                                match res {
                                    Ok(resp) if resp_error_code(&resp).is_none() => {
                                        let _ = gui_tx_clone.send(GuiEvent::MessageRecalled {
                                            thread_id: to_id,
                                            msg_id: msg.id.clone(),
                                            ts: msg.ts,
                                        });
                                    }
                                    Ok(resp) => {
                                        let err_s = format!(
                                            "server error {}",
                                            resp_error_code(&resp).unwrap_or(0)
                                        );
                                        crate::logger::log_error("gui::recall", &err_s);
                                        let _ = gui_tx_clone.send(GuiEvent::StatusUpdate(format!(
                                            "Đã thu hồi cục bộ (Máy chủ: {err_s})"
                                        )));
                                    }
                                    Err(e) => {
                                        crate::logger::log_error("gui::recall", &e.to_string());
                                        let _ = gui_tx_clone.send(GuiEvent::StatusUpdate(format!(
                                            "Không thể thu hồi trên máy chủ: {e}"
                                        )));
                                    }
                                }
                                ctx_clone.request_repaint();
                            });
                        }
                        WorkerCmd::SendSticker {
                            to_id,
                            sticker_id,
                            cate_id,
                            sticker_type,
                            is_group,
                        } => {
                            let session_clone = session.clone();
                            let gui_tx_clone = gui_tx.clone();
                            let ctx_clone = egui_ctx.clone();
                            tokio::spawn(async move {
                                let s = session_clone.lock().await.clone();
                                let client_id = crate::chat::next_client_message_id();
                                let res = crate::chat::send_sticker_with_client_id(
                                    &s,
                                    &to_id,
                                    sticker_id,
                                    cate_id,
                                    sticker_type,
                                    is_group, client_id)
                                .await;
                                match res {
                                    Ok(resp) => {
                                        let msg_id = resp
                                            .get("data")
                                            .and_then(|d| d.get("msgId"))
                                            .and_then(|v| match v {
                                                serde_json::Value::String(s) => Some(s.clone()),
                                                serde_json::Value::Number(n) => Some(n.to_string()),
                                                _ => None,
                                            })
                                            .unwrap_or_default();
                                        let _ = gui_tx_clone.send(GuiEvent::StickerSent {
                                            client_message_id: client_id.to_string(),
                                            thread_id: to_id,
                                            msg_id,
                                            sticker_id,
                                            cate_id,
                                            sticker_type,
                                        });
                                        ctx_clone.request_repaint();
                                    }
                                    Err(e) => {
                                        let err_s = e.to_string();
                                        crate::logger::log_error("gui::send_sticker", &err_s);
                                        let _ = gui_tx_clone.send(GuiEvent::SendFailed {
                                            thread_id: to_id,
                                            text: String::new(),
                                            error: err_s.clone(),
                                        });
                                        if crate::session::SessionStatus::from_error(&e)
                                            == crate::session::SessionStatus::Expired
                                        {
                                            {
                                                let mut sl = session_clone.lock().await;
                                                sl.mark_expired();
                                                let _ = sl.save_to_file("session.json");
                                                sl.logout();
                                            }
                                            let _ = gui_tx_clone.send(GuiEvent::SessionExpired(
                                                "Không thể gửi nhãn dán vì phiên zpw_sek đã hết hạn"
                                                    .into(),
                                            ));
                                            if let Some(tx) = WORKER_TX.get() {
                                                let _ = tx.send(WorkerCmd::RequestQr);
                                            }
                                        }
                                        ctx_clone.request_repaint();
                                    }
                                }
                            });
                        }
                        WorkerCmd::FetchStickerImage { key, url } => {
                            let session_clone = session.clone();
                            let gui_tx_clone = gui_tx.clone();
                            let ctx_clone = egui_ctx.clone();
                            tokio::spawn(async move {
                                let cache_dir = std::path::Path::new("stickers_cache");
                                let _ = std::fs::create_dir_all(cache_dir);
                                let cache_path = cache_dir.join(format!("{key}.png"));

                                // Bytes come from the disk cache, a local file or the network. They
                                // used to be decoded once to validate and again to use; now once, on
                                // the blocking pool so a large photo cannot stall the 2-thread runtime.
                                let (bytes_opt, from_network) =
                                    if let Ok(disk_bytes) = std::fs::read(&cache_path) {
                                        (Some(disk_bytes), false)
                                    } else if !url.starts_with("http://")
                                        && !url.starts_with("https://")
                                    {
                                        (std::fs::read(&url).ok(), false)
                                    } else {
                                        let client = { session_clone.lock().await.clone().http };
                                        let body = match client.get(&url).send().await {
                                            Ok(resp) if resp.status().is_success() => {
                                                resp.bytes().await.ok()
                                            }
                                            _ => None,
                                        };
                                        (body.map(|b| b.to_vec()), true)
                                    };
                                let Some(bytes) = bytes_opt else {
                                    return;
                                };

                                let _ = tokio::task::spawn_blocking(move || {
                                    let Ok(mut dyn_img) = image::load_from_memory(&bytes) else {
                                        if !from_network {
                                            // Corrupt cache entry: drop it so the next request refetches.
                                            let _ = std::fs::remove_file(&cache_path);
                                        }
                                        return;
                                    };
                                    if from_network {
                                        let _ = std::fs::write(&cache_path, &bytes);
                                    }
                                    // Bubbles draw images at most 240 pt wide; full-size photos kept as
                                    // textures cost ~8 MB each (1080p) and are never freed.
                                    // The viewer's `hd_` textures stay full size.
                                    if !key.starts_with("ava_")
                                        && !key.starts_with("hd_")
                                        && (dyn_img.width() > 480 || dyn_img.height() > 480)
                                    {
                                        dyn_img = dyn_img.thumbnail(480, 480);
                                    }
                                    {
                                        let mut rgba = dyn_img.to_rgba8();
                                        // Apply smooth anti-aliased circular alpha mask for avatar textures (`ava_*`)
                                        if key.starts_with("ava_") {
                                            let side = rgba.width().min(rgba.height()).clamp(32, 160);
                                            let x0 = (rgba.width() - side) / 2;
                                            let y0 = (rgba.height() - side) / 2;
                                            let cropped = image::imageops::crop_imm(
                                                &rgba, x0, y0, side, side,
                                            )
                                            .to_image();
                                            rgba = if side > 128 {
                                                image::imageops::resize(
                                                    &cropped,
                                                    128,
                                                    128,
                                                    image::imageops::FilterType::Triangle,
                                                )
                                            } else {
                                                cropped
                                            };
                                            let w = rgba.width() as f32;
                                            let h = rgba.height() as f32;
                                            let cx = (w - 1.0) * 0.5;
                                            let cy = (h - 1.0) * 0.5;
                                            let r = w.min(h) * 0.5;
                                            for (x, y, px) in rgba.enumerate_pixels_mut() {
                                                let dx = x as f32 - cx;
                                                let dy = y as f32 - cy;
                                                let dist = (dx * dx + dy * dy).sqrt();
                                                if dist > r {
                                                    px[3] = 0;
                                                } else if dist > r - 1.4 {
                                                    let cov = ((r - dist) / 1.4).clamp(0.0, 1.0);
                                                    px[3] = (px[3] as f32 * cov) as u8;
                                                }
                                            }
                                        }
                                        let size =
                                            [rgba.width() as usize, rgba.height() as usize];
                                        let pixels = rgba.as_flat_samples();
                                        let color_img = egui::ColorImage::from_rgba_unmultiplied(
                                            size,
                                            pixels.as_slice(),
                                        );
                                        let _ = gui_tx_clone.send(GuiEvent::StickerImageReady {
                                            key,
                                            image: color_img,
                                        });
                                        ctx_clone.request_repaint();
                                    }
                                })
                                .await;
                            });
                        }
                        WorkerCmd::FetchStickerStore => {
                            let session_clone = session.clone();
                            let gui_tx_clone = gui_tx.clone();
                            let ctx_clone = egui_ctx.clone();
                            tokio::spawn(async move {
                                let s = session_clone.lock().await.clone();
                                match crate::chat::fetch_sticker_store_packs(&s).await {
                                    Ok(packs) => {
                                        let _ = gui_tx_clone
                                            .send(GuiEvent::StickerStoreLoaded(packs));
                                    }
                                    Err(e) => {
                                        crate::logger::log_error(
                                            "gui::fetch_sticker_store",
                                            &e.to_string(),
                                        );
                                        let _ = gui_tx_clone
                                            .send(GuiEvent::StickerStoreLoaded(Vec::new()));
                                        let _ = gui_tx_clone.send(GuiEvent::StatusUpdate(format!(
                                            "Không tải được kho nhãn dán: {e}"
                                        )));
                                    }
                                }
                                ctx_clone.request_repaint();
                            });
                        }
                        WorkerCmd::FetchStickerPack { cate_id, name } => {
                            let session_clone = session.clone();
                            let gui_tx_clone = gui_tx.clone();
                            let ctx_clone = egui_ctx.clone();
                            tokio::spawn(async move {
                                let s = session_clone.lock().await.clone();
                                match crate::chat::fetch_sticker_pack_items(&s, cate_id, &name)
                                    .await
                                {
                                    Ok(items) => {
                                        let _ = gui_tx_clone.send(
                                            GuiEvent::StickerPackItemsLoaded {
                                                cate_id,
                                                name,
                                                items,
                                            },
                                        );
                                    }
                                    Err(e) => {
                                        crate::logger::log_error(
                                            "gui::fetch_sticker_pack",
                                            &e.to_string(),
                                        );
                                        let _ = gui_tx_clone.send(GuiEvent::StatusUpdate(format!(
                                            "Không tải được bộ nhãn \"{name}\": {e}"
                                        )));
                                    }
                                }
                                ctx_clone.request_repaint();
                            });
                        }
                        WorkerCmd::SearchStickers { keyword } => {
                            let session_clone = session.clone();
                            let gui_tx_clone = gui_tx.clone();
                            let ctx_clone = egui_ctx.clone();
                            tokio::spawn(async move {
                                let s = session_clone.lock().await.clone();
                                match crate::chat::search_stickers_by_keyword(&s, &keyword).await {
                                    Ok(items) => {
                                        let missing = preload_disk_sticker_thumbnails(
                                            &items,
                                            &gui_tx_clone,
                                            24,
                                        );
                                        let _ = gui_tx_clone.send(GuiEvent::StickerSearchResults {
                                            keyword,
                                            items,
                                        });
                                        ctx_clone.request_repaint();
                                        prefetch_missing_sticker_thumbnails(
                                            &s.http,
                                            missing,
                                            &gui_tx_clone,
                                            &ctx_clone,
                                        )
                                        .await;
                                    }
                                    Err(e) => {
                                        crate::logger::log_error(
                                            "gui::search_stickers",
                                            &e.to_string(),
                                        );
                                        let _ = gui_tx_clone.send(GuiEvent::StickerSearchResults {
                                            keyword,
                                            items: Vec::new(),
                                        });
                                        ctx_clone.request_repaint();
                                    }
                                }
                            });
                        }
                        WorkerCmd::SuggestComposerStickers { keyword } => {
                            let session_clone = session.clone();
                            let gui_tx_clone = gui_tx.clone();
                            let ctx_clone = egui_ctx.clone();
                            tokio::spawn(async move {
                                let s = session_clone.lock().await.clone();
                                let items = crate::chat::search_stickers_by_keyword(&s, &keyword)
                                    .await
                                    .unwrap_or_default();
                                let missing =
                                    preload_disk_sticker_thumbnails(&items, &gui_tx_clone, 16);
                                let _ =
                                    gui_tx_clone.send(GuiEvent::ComposerStickerSuggestions {
                                        keyword,
                                        items,
                                    });
                                ctx_clone.request_repaint();
                                prefetch_missing_sticker_thumbnails(
                                    &s.http,
                                    missing,
                                    &gui_tx_clone,
                                    &ctx_clone,
                                )
                                .await;
                            });
                        }
                        WorkerCmd::ParseLinkPreview { url } => {
                            let session_clone = session.clone();
                            let gui_tx_clone = gui_tx.clone();
                            let ctx_clone = egui_ctx.clone();
                            tokio::spawn(async move {
                                let s = session_clone.lock().await.clone();
                                if let Ok(meta) = crate::chat::parse_link_metadata(&s, &url).await {
                                    let _ = gui_tx_clone.send(GuiEvent::LinkPreviewReady(meta));
                                    ctx_clone.request_repaint();
                                }
                            });
                        }
                        WorkerCmd::PickAndSendAttachment {
                            to_id,
                            is_group,
                            images_only,
                            caption,
                            preset_path,
                        } => {
                            let session_clone = session.clone();
                            let gui_tx_clone = gui_tx.clone();
                            let ctx_clone = egui_ctx.clone();
                            tokio::spawn(async move {
                                let picked_path = if let Some(p) = preset_path {
                                    Some(p)
                                } else {
                                    tokio::task::spawn_blocking(move || {
                                        pick_native_file_dialog(images_only)
                                    })
                                    .await
                                    .ok()
                                    .flatten()
                                };

                                let Some(path) = picked_path else {
                                    return;
                                };
                                let fname = path
                                    .file_name()
                                    .and_then(|n| n.to_str())
                                    .unwrap_or("tập tin")
                                    .to_string();
                                let _ = gui_tx_clone.send(GuiEvent::StatusUpdate(format!(
                                    "⏳ Đang tải lên và gửi '{fname}'…"
                                )));
                                ctx_clone.request_repaint();

                                let s = session_clone.lock().await.clone();
                                match crate::chat::send_attachment(
                                    &s, &to_id, is_group, &path, &caption,
                                )
                                .await
                                {
                                    Ok((_resp, cm)) => {
                                        let _ = gui_tx_clone.send(GuiEvent::AttachmentSent {
                                            thread_id: to_id,
                                            msg: cm,
                                        });
                                    }
                                    Err(e) => {
                                        let err_s = e.to_string();
                                        crate::logger::log_error("gui::send_attachment", &err_s);
                                        let _ = gui_tx_clone.send(GuiEvent::SendFailed {
                                            thread_id: to_id,
                                            text: caption,
                                            error: format!("Lỗi gửi '{fname}': {err_s}"),
                                        });
                                    }
                                }
                                ctx_clone.request_repaint();
                            });
                        }
                        WorkerCmd::StartCall {
                            partner_id,
                            partner_name,
                            partner_avatar,
                            is_video,
                            is_group,
                        } => {
                            let kind_label = if is_video { "Gọi video" } else { "Gọi thoại" };
                            call_bridge.make_call(
                                &partner_id,
                                &partner_name,
                                partner_avatar.as_deref(),
                                is_video,
                                is_group,
                            );
                            let status_msg = format!(
                                "📞 Đang khởi tạo {kind_label} trực tiếp (Pure-Rust) tới {partner_name} ({partner_id})…"
                            );
                            let _ = gui_tx.send(GuiEvent::StatusUpdate(status_msg));
                            egui_ctx.request_repaint();
                        }
                        WorkerCmd::AnswerCall { accept } => {
                            call_bridge.answer_call(accept);
                            egui_ctx.request_repaint();
                        }
                        WorkerCmd::SetCallMediaState {
                            is_on_mic,
                            is_on_cam,
                        } => {
                            call_bridge.set_media_state(is_on_mic, is_on_cam);
                            egui_ctx.request_repaint();
                        }
                        WorkerCmd::EndCall => {
                            call_bridge.end_call();
                            let _ = gui_tx.send(GuiEvent::CallStateChanged("free".into()));
                            let _ = gui_tx.send(GuiEvent::StatusUpdate(
                                "Đã kết thúc cuộc gọi".into(),
                            ));
                            egui_ctx.request_repaint();
                        }
                        WorkerCmd::CheckFriendStatus { user_id } => {
                            let session_clone = session.clone();
                            let gui_tx_clone = gui_tx.clone();
                            let ctx_clone = egui_ctx.clone();
                            tokio::spawn(async move {
                                let s = session_clone.lock().await.clone();
                                let status_res =
                                    crate::chat::check_friendship_status(&s, &user_id).await;
                                let online_opt =
                                    crate::chat::get_user_online_status(&s, &user_id).await.ok();
                                if let Ok(status) = status_res {
                                    let _ = gui_tx_clone.send(GuiEvent::FriendStatusChecked {
                                        user_id,
                                        status,
                                        online: online_opt,
                                    });
                                    ctx_clone.request_repaint();
                                }
                            });
                        }
                        WorkerCmd::AcceptFriend { user_id, user_name } => {
                            let session_clone = session.clone();
                            let gui_tx_clone = gui_tx.clone();
                            let ctx_clone = egui_ctx.clone();
                            tokio::spawn(async move {
                                let s = session_clone.lock().await.clone();
                                match crate::chat::accept_friend_request(&s, &user_id).await {
                                    Ok(_) => {
                                        let _ = gui_tx_clone.send(GuiEvent::FriendStatusChecked {
                                            user_id,
                                            status: crate::chat::FriendshipStatus::Friend,
                                            online: None,
                                        });
                                        let _ = gui_tx_clone.send(GuiEvent::StatusUpdate(format!(
                                            "Đã đồng ý kết bạn với {user_name}!"
                                        )));
                                    }
                                    Err(e) => {
                                        let _ = gui_tx_clone.send(GuiEvent::StatusUpdate(format!(
                                            "Không thể đồng ý kết bạn với {user_name}: {e}"
                                        )));
                                    }
                                }
                                ctx_clone.request_repaint();
                            });
                        }
                        WorkerCmd::RejectFriend { user_id, user_name } => {
                            let session_clone = session.clone();
                            let gui_tx_clone = gui_tx.clone();
                            let ctx_clone = egui_ctx.clone();
                            tokio::spawn(async move {
                                let s = session_clone.lock().await.clone();
                                if crate::chat::reject_friend_request(&s, &user_id)
                                    .await
                                    .is_ok()
                                {
                                    let _ = gui_tx_clone.send(GuiEvent::FriendStatusChecked {
                                        user_id,
                                        status: crate::chat::FriendshipStatus::Stranger,
                                        online: None,
                                    });
                                    let _ = gui_tx_clone.send(GuiEvent::StatusUpdate(format!(
                                        "Đã từ chối lời mời kết bạn của {user_name}."
                                    )));
                                    ctx_clone.request_repaint();
                                }
                            });
                        }
                        WorkerCmd::UndoFriend { user_id, user_name } => {
                            let session_clone = session.clone();
                            let gui_tx_clone = gui_tx.clone();
                            let ctx_clone = egui_ctx.clone();
                            tokio::spawn(async move {
                                let s = session_clone.lock().await.clone();
                                if crate::chat::undo_friend_request(&s, &user_id).await.is_ok() {
                                    let _ = gui_tx_clone.send(GuiEvent::FriendStatusChecked {
                                        user_id,
                                        status: crate::chat::FriendshipStatus::Stranger,
                                        online: None,
                                    });
                                    let _ = gui_tx_clone.send(GuiEvent::StatusUpdate(format!(
                                        "Đã hủy lời mời kết bạn tới {user_name}."
                                    )));
                                    ctx_clone.request_repaint();
                                }
                            });
                        }
                        WorkerCmd::AddFriend { user_id, user_name } => {
                            let session_clone = session.clone();
                            let gui_tx_clone = gui_tx.clone();
                            let ctx_clone = egui_ctx.clone();
                            tokio::spawn(async move {
                                let s = session_clone.lock().await.clone();
                                // Check if there is an incoming request we can accept directly
                                if crate::chat::accept_friend_request(&s, &user_id).await.is_ok() {
                                    let _ = gui_tx_clone.send(GuiEvent::FriendStatusChecked {
                                        user_id,
                                        status: crate::chat::FriendshipStatus::Friend,
                                        online: None,
                                    });
                                    let _ = gui_tx_clone.send(GuiEvent::StatusUpdate(format!(
                                        "Đã đồng ý kết bạn với {user_name}! Bạn có thể gọi lại ngay."
                                    )));
                                    ctx_clone.request_repaint();
                                    return;
                                }
                                match crate::chat::send_friend_request(
                                    &s,
                                    &user_id,
                                    "Xin chào, kết bạn với mình nhé!",
                                )
                                .await
                                {
                                    Ok(_) => {
                                        let _ = gui_tx_clone.send(GuiEvent::FriendStatusChecked {
                                            user_id: user_id.clone(),
                                            status: crate::chat::FriendshipStatus::OutgoingPending,
                                            online: None,
                                        });
                                        let _ = gui_tx_clone.send(GuiEvent::StatusUpdate(format!(
                                            "Đã gửi lời mời kết bạn tới {user_name} ({user_id})."
                                        )));
                                    }
                                    Err(e) => {
                                        let _ = gui_tx_clone.send(GuiEvent::StatusUpdate(format!(
                                            "Không thể gửi lời mời kết bạn tới {user_name}: {e}"
                                        )));
                                    }
                                }
                                ctx_clone.request_repaint();
                            });
                        }
                        WorkerCmd::LookupPhone { phone } => {
                            let session_clone = session.clone();
                            let gui_tx_clone = gui_tx.clone();
                            let ctx_clone = egui_ctx.clone();
                            tokio::spawn(async move {
                                let s = session_clone.lock().await.clone();
                                match crate::chat::find_user_by_phone(&s, &phone).await {
                                    Ok(profile) => {
                                        let _ = gui_tx_clone
                                            .send(GuiEvent::PhoneLookupSuccess(profile));
                                    }
                                    Err(e) => {
                                        crate::logger::log_error(
                                            "gui::lookup_phone",
                                            &e.to_string(),
                                        );
                                        let _ = gui_tx_clone
                                            .send(GuiEvent::PhoneLookupFailed(e.to_string()));
                                    }
                                }
                                ctx_clone.request_repaint();
                            });
                        }
                        WorkerCmd::LoginPassword { phone, pass } => {
                            let session_clone = session.clone();
                            let poll_clone = poll_state.clone();
                            let gui_tx_clone = gui_tx.clone();
                            let ctx_clone = egui_ctx.clone();
                            tokio::spawn(async move {
                                let _ = gui_tx_clone.send(GuiEvent::StatusUpdate(
                                    "Đang xác thực tài khoản…".into(),
                                ));
                                ctx_clone.request_repaint();
                                let mut s = session_clone.lock().await;
                                match crate::login::login_by_password(&mut s, &phone, &pass, "VN")
                                    .await
                                {
                                    Ok(_) => {
                                        if let Ok(info) = crate::login::get_login_info(&s).await {
                                            if let Some(cfg) = info.get("data") {
                                                crate::login::apply_login_config(&mut s, cfg);
                                                s.apply_server_info(cfg);
                                                let _ = s.save_to_file("session.json");

                                                let uin = s
                                                    .login
                                                    .as_ref()
                                                    .map(|l| l.uin.clone())
                                                    .unwrap_or_default();
                                                let name = s
                                                    .login
                                                    .as_ref()
                                                    .map(|l| l.display_name.clone())
                                                    .unwrap_or_default();
                                                let u_ava = s
                                                    .login
                                                    .as_ref()
                                                    .map(|l| l.avatar.clone())
                                                    .filter(|a| !a.is_empty());
                                                let _ = gui_tx_clone.send(GuiEvent::LoginSuccess {
                                                    name,
                                                    uin,
                                                    avatar: u_ava,
                                                });
                                                ctx_clone.request_repaint();

                                                if let Ok(mut list) =
                                                    crate::chat::get_friends(&s).await
                                                {
                                                    {
                                                        let mut ps = poll_clone.lock().await;
                                                        *ps = crate::chat::PollState::default();
                                                        let _ = crate::chat::poll_new_messages(
                                                            &s, &mut ps, &mut list,
                                                        )
                                                        .await;
                                                    }
                                                    let _ = gui_tx_clone
                                                        .send(GuiEvent::ThreadsUpdated(list));
                                                    ctx_clone.request_repaint();
                                                }
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        let _ = gui_tx_clone
                                            .send(GuiEvent::LoginFailed(e.to_string()));
                                        ctx_clone.request_repaint();
                                    }
                                }
                            });
                        }
                        WorkerCmd::CheckForUpdates { silent } => {
                            let gui_tx_clone = gui_tx.clone();
                            let ctx_clone = egui_ctx.clone();
                            tokio::spawn(async move {
                                let res = fetch_github_latest_release().await;
                                let _ = gui_tx_clone.send(GuiEvent::UpdateCheckFinished {
                                    result: res,
                                    silent,
                                });
                                ctx_clone.request_repaint();
                            });
                        }
                        WorkerCmd::DownloadAndApplyUpdate {
                            download_url,
                            asset_name,
                            target_version,
                            expected_sha256,
                        } => {
                            let gui_tx_clone = gui_tx.clone();
                            let ctx_clone = egui_ctx.clone();
                            tokio::spawn(async move {
                                let _ = gui_tx_clone.send(GuiEvent::UpdateDownloadStatus {
                                    downloading: true,
                                    message: format!(
                                        "Đang tải bản cập nhật v{target_version}…"
                                    ),
                                });
                                ctx_clone.request_repaint();
                                match apply_github_release_update(
                                    &download_url,
                                    &asset_name,
                                    &target_version,
                                    expected_sha256.as_deref(),
                                    &gui_tx_clone,
                                    &ctx_clone,
                                )
                                .await
                                {
                                    Ok(restarting) => {
                                        if restarting {
                                            let _ = gui_tx_clone.send(
                                                GuiEvent::UpdateAppliedAndRestarting(target_version),
                                            );
                                        } else {
                                            let _ = gui_tx_clone.send(
                                                GuiEvent::UpdateDownloadStatus {
                                                    downloading: false,
                                                    message: format!(
                                                        "Đã tải và mở bộ cài đặt {asset_name} (v{target_version})"
                                                    ),
                                                },
                                            );
                                        }
                                    }
                                    Err(e) => {
                                        let _ = gui_tx_clone.send(GuiEvent::UpdateDownloadStatus {
                                            downloading: false,
                                            message: format!("Lỗi cập nhật: {e}"),
                                        });
                                    }
                                }
                                ctx_clone.request_repaint();
                            });
                        }
                        WorkerCmd::FetchSocialProfile { target_uid } => {
                            let session_clone = session.clone();
                            let gui_tx_clone = gui_tx.clone();
                            let ctx_clone = egui_ctx.clone();
                            tokio::spawn(async move {
                                let s = session_clone.lock().await.clone();
                                let res = s
                                    .get_friend_social_profile(&target_uid)
                                    .await
                                    .map_err(|e| e.to_string());
                                let _ = gui_tx_clone.send(GuiEvent::SocialProfileLoaded(res));
                                ctx_clone.request_repaint();
                            });
                        }
                        WorkerCmd::FetchMySocialAvatars { page, count } => {
                            let session_clone = session.clone();
                            let gui_tx_clone = gui_tx.clone();
                            let ctx_clone = egui_ctx.clone();
                            tokio::spawn(async move {
                                let s = session_clone.lock().await.clone();
                                let res = s
                                    .get_my_social_avatar_list(page, count)
                                    .await
                                    .map_err(|e| e.to_string());
                                let _ = gui_tx_clone.send(GuiEvent::SocialAvatarsLoaded(res));
                                ctx_clone.request_repaint();
                            });
                        }
                        WorkerCmd::FetchTimelineFeed {
                            friend_uids,
                            shared_media_by_uid,
                        } => {
                            let session_clone = session.clone();
                            let gui_tx_clone = gui_tx.clone();
                            let ctx_clone = egui_ctx.clone();
                            tokio::spawn(async move {
                                let s = session_clone.lock().await.clone();
                                let res = s
                                    .fetch_timeline_feed(&friend_uids, &shared_media_by_uid)
                                    .await
                                    .map_err(|e| e.to_string());
                                let _ = gui_tx_clone.send(GuiEvent::SocialTimelineLoaded(res));
                                ctx_clone.request_repaint();
                            });
                        }
                    }
                }
            });
        })
        .expect("Failed to spawn zalo-worker thread");
}

const GITHUB_REPO: &str = "ctdoteam1110/ctdozlite";

pub(crate) async fn fetch_github_latest_release() -> Result<UpdateCheckResult, String> {
    let current_ver = env!("CARGO_PKG_VERSION").to_string();
    let client = reqwest::Client::builder()
        .user_agent(format!("ctdozlite/{current_ver}"))
        .timeout(Duration::from_secs(12))
        .build()
        .map_err(|e| format!("Khởi tạo HTTP client thất bại: {e}"))?;

    let latest_url = format!("https://api.github.com/repos/{GITHUB_REPO}/releases/latest");
    let mut rel_json: Option<serde_json::Value> = None;

    if let Ok(resp) = client
        .get(&latest_url)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .send()
        .await
    {
        if resp.status().is_success() {
            if let Ok(v) = resp.json::<serde_json::Value>().await {
                rel_json = Some(v);
            }
        }
    }

    if rel_json.is_none() {
        let list_url =
            format!("https://api.github.com/repos/{GITHUB_REPO}/releases?per_page=5");
        if let Ok(resp) = client
            .get(&list_url)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .send()
            .await
        {
            if resp.status().is_success() {
                if let Ok(arr) = resp.json::<serde_json::Value>().await {
                    if let Some(first) = arr
                        .as_array()
                        .and_then(|a| {
                            a.iter()
                                .find(|r| {
                                    !r.get("draft").and_then(|v| v.as_bool()).unwrap_or(false)
                                })
                                .or_else(|| a.first())
                        })
                        .cloned()
                    {
                        rel_json = Some(first);
                    }
                }
            }
        }
    }

    // Rate-limit resilient fallback: fetch static `latest.json` directly from GitHub Release CDN
    if rel_json.is_none() {
        let cdn_manifest_url =
            format!("https://github.com/{GITHUB_REPO}/releases/latest/download/latest.json");
        if let Ok(resp) = client.get(&cdn_manifest_url).send().await {
            if resp.status().is_success() {
                if let Ok(manifest) = resp.json::<serde_json::Value>().await {
                    rel_json = Some(manifest);
                }
            }
        }
    }

    let Some(rel) = rel_json else {
        return Err("Không truy cập được máy chủ GitHub Releases API".into());
    };

    let mut meta = crate::updater::parse_github_release_value(
        &rel,
        &current_ver,
        GITHUB_REPO,
        std::env::consts::OS,
        std::env::consts::ARCH,
    );

    if meta.expected_sha256.is_none() {
        if let (Some(ref s_url), Some(ref b_name)) = (&meta.sha256sums_url, &meta.asset_name) {
            if let Ok(resp) = client.get(s_url).send().await {
                if let Ok(txt) = resp.text().await {
                    meta.expected_sha256 =
                        crate::updater::parse_sha256sums_for_asset(&txt, b_name);
                }
            }
        }
    }

    Ok(UpdateCheckResult {
        current_version: meta.current_version,
        latest_version: meta.latest_version,
        has_update: meta.has_update,
        release_title: meta.release_title,
        release_notes: meta.release_notes,
        html_url: meta.html_url,
        download_url: meta.download_url,
        asset_name: meta.asset_name,
        asset_size: meta.asset_size,
        expected_sha256: meta.expected_sha256,
        published_at: meta.published_at,
    })
}

/// Stream-download the release asset from GitHub Releases with real-time progress reporting,
/// verify binary integrity (PE/ELF magic + SHA-256 checksum when provided) via [`crate::updater`],
/// and atomically replace the running executable in-place with automatic rollback on failure.
async fn apply_github_release_update(
    download_url: &str,
    asset_name: &str,
    target_version: &str,
    expected_sha256: Option<&str>,
    gui_tx: &GuiTx,
    ctx: &egui::Context,
) -> Result<bool, String> {
    let client = reqwest::Client::builder()
        .user_agent(format!("ctdozlite/{target_version}"))
        .timeout(Duration::from_secs(240))
        .build()
        .map_err(|e| format!("Khởi tạo HTTP client thất bại: {e}"))?;

    let req = crate::updater::DownloadRequest {
        url: download_url,
        asset_name,
        expected_sha256,
        expected_size: None,
        limits: crate::updater::DownloadLimits::default(),
    };

    let bytes = crate::updater::download_release(&client, &req, |downloaded, total| {
        let _ = gui_tx.send(GuiEvent::UpdateDownloadProgress { downloaded, total });
        ctx.request_repaint();
    })
    .await?;

    let name_low = asset_name.to_lowercase();
    let is_portable_bin = (cfg!(target_os = "windows")
        && name_low.ends_with(".exe")
        && !name_low.contains("setup"))
        || (cfg!(target_os = "linux") && name_low.ends_with(".appimage"));

    if is_portable_bin {
        if let Ok(exe_path) = std::env::current_exe() {
            #[cfg(target_os = "windows")]
            {
                // On Windows a running .exe cannot be overwritten directly, so rename current exe
                // to `.exe.old` and atomically place the new portable binary at `exe_path`.
                let new_path = exe_path.with_extension("exe.new");
                let old_path = exe_path.with_extension("exe.old");
                if std::fs::write(&new_path, &bytes).is_ok() {
                    let _ = std::fs::remove_file(&old_path);
                    if std::fs::rename(&exe_path, &old_path).is_ok() {
                        if std::fs::rename(&new_path, &exe_path).is_ok() {
                            let args: Vec<String> = std::env::args().skip(1).collect();
                            if std::process::Command::new(&exe_path).args(&args).spawn().is_ok() {
                                return Ok(true);
                            }
                            let _ = std::fs::rename(&old_path, &exe_path);
                        } else {
                            let _ = std::fs::rename(&old_path, &exe_path);
                        }
                    }
                    let _ = std::fs::remove_file(&new_path);
                }
            }
            #[cfg(not(target_os = "windows"))]
            {
                let args: Vec<String> = std::env::args().skip(1).collect();
                if crate::updater::install_release(
                    &bytes,
                    &crate::updater::InstallTarget::Portable(exe_path),
                    |installed_path| {
                        std::process::Command::new(installed_path)
                            .args(&args)
                            .spawn()
                            .map(|_| ())
                            .map_err(|e| e.to_string())
                    },
                )
                .is_ok()
                {
                    return Ok(true);
                }
            }
        }
    }

    // Fallback / Installer mode: save to Downloads (or current directory) via transactional installer
    let dl_dir = std::env::var("USERPROFILE")
        .ok()
        .map(|p| std::path::PathBuf::from(p).join("Downloads"))
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .map(|h| std::path::PathBuf::from(h).join("Downloads"))
        })
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let _ = std::fs::create_dir_all(&dl_dir);
    let dest_path = dl_dir.join(asset_name);

    crate::updater::install_release(
        &bytes,
        &crate::updater::InstallTarget::Installer(dest_path),
        |installer_path| {
            #[cfg(target_os = "windows")]
            {
                std::process::Command::new(installer_path)
                    .spawn()
                    .map(|_| ())
                    .map_err(|e| e.to_string())
            }
            #[cfg(target_os = "macos")]
            {
                std::process::Command::new("open")
                    .arg(installer_path)
                    .spawn()
                    .map(|_| ())
                    .map_err(|e| e.to_string())
            }
            #[cfg(not(any(target_os = "windows", target_os = "macos")))]
            {
                std::process::Command::new("xdg-open")
                    .arg(installer_path)
                    .spawn()
                    .map(|_| ())
                    .map_err(|e| e.to_string())
            }
        },
    )?;
    Ok(false)
}

