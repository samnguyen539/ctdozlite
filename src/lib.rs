pub mod audio;
pub mod call;
pub mod chat;
pub mod clock;
pub mod crypto;
pub mod endpoints;
pub mod feed;
pub mod gui;
pub mod history_sync;
pub mod logger;
pub mod login;
pub mod media;
pub mod message_model;
/// Exact quoted-message references in loaded conversations.
pub mod message_navigation;
pub mod param_cipher;
pub mod qr;
pub mod reactions;
pub mod session;
pub mod socket;
pub mod store;
pub mod tui;
pub mod ui;
pub mod unread;
pub mod voip_log;

/// Writable storage selection and private atomic session writes.
pub mod data_dir;
/// Bounded media downloads, cache isolation, and resource admission.
pub mod media_resources;
/// Bounded release downloads and transactional installation.
pub mod updater;
/// Scheduled message queue backed by SQLite (#38, #24).
pub mod scheduler;
/// Hub-facing REST extensions (QR login, sync, poll, actions).
pub mod api_hub_ext;
/// Local HTTP REST API, MCP JSON-RPC 2.0 server, and auto-reply rules (#35).
pub mod api_mcp;

/// Native notification authorization without delivery.
pub mod native_notifications;
