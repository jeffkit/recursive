//! WeChat iLink daemon — workspace-level singleton.
//!
//! [`WeixinDaemon`] manages a single iLink connection and exposes a
//! [`WeixinRequest`] channel that the agent backend (TUI or headless) listens
//! on. Incoming WeChat messages are parsed as commands (operating on the
//! sender's own session binding — see [`super::session_map`]) or forwarded
//! to the backend worker as agent turns.
//!
//! # Lifecycle
//!
//! 1. Build a [`WeixinDaemon`] via [`WeixinDaemon::new`].
//! 2. Call [`WeixinDaemon::login`] to authenticate (QR code scan or stored
//!    credentials).
//! 3. Call [`WeixinDaemon::start`] to begin polling. This spawns a background
//!    Tokio task; the caller holds a `JoinHandle` and a `mpsc::Receiver<WeixinRequest>`.
//! 4. The caller's run-loop drains [`WeixinRequest`] messages, processes them
//!    via `AgentRuntime::enqueue`, and sends the response back via
//!    `WeixinRequest::reply_tx`.

use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::{mpsc, oneshot};
use tracing::{debug, error, info, warn};
use wechatbot::{BotOptions, WeChatBot};

use super::commands::{parse_command, WeixinCommand, HELP_TEXT};
use super::session_map::{
    render_session_tail, render_sessions_list, resolve_change, WeixinSessionMap,
};

// ---------------------------------------------------------------------------
// WeixinRequest
// ---------------------------------------------------------------------------

/// A request from the WeChat daemon to the agent backend.
///
/// The backend should process `text` against the runtime and reply via
/// `reply_tx` with the agent's final text response (or `None` on error).
pub struct WeixinRequest {
    /// WeChat user ID of the sender.
    pub user_id: String,
    /// The message text to pass to the agent.
    pub text: String,
    /// The session the sender is currently bound to (`None` = no binding:
    /// the backend starts a fresh conversation and binds it). Read from
    /// [`WeixinSessionMap`] when the message is forwarded, so `/c N` and
    /// `/r` take effect on the next message — that is what the command
    /// help promises.
    pub session_id: Option<String>,
    /// Channel for the backend to return the agent's response.
    pub reply_tx: oneshot::Sender<Option<String>>,
}

// ---------------------------------------------------------------------------
// WeixinDaemonOptions
// ---------------------------------------------------------------------------

/// Configuration options for [`WeixinDaemon`].
#[derive(Debug, Clone)]
pub struct WeixinDaemonOptions {
    /// Override the iLink API base URL (default: official Tencent endpoint).
    /// Set this when using an ilink-hub proxy.
    pub base_url: Option<String>,
    /// Path to store/load bot credentials. Defaults to
    /// `~/.recursive/<workspace>/weixin_creds.json`.
    pub cred_path: Option<PathBuf>,
    /// Workspace root (for default cred_path derivation and session listing).
    pub workspace: PathBuf,
}

impl WeixinDaemonOptions {
    pub fn new(workspace: impl Into<PathBuf>) -> Self {
        Self {
            base_url: None,
            cred_path: None,
            workspace: workspace.into(),
        }
    }
}

// ---------------------------------------------------------------------------
// WeixinDaemon
// ---------------------------------------------------------------------------

/// Workspace-level WeChat daemon.
///
/// After [`WeixinDaemon::start`] is called, incoming WeChat messages are
/// delivered as [`WeixinRequest`]s on the returned receiver. The backend
/// worker processes them against `AgentRuntime::enqueue` and sends responses
/// back via the oneshot channel, after which the daemon forwards the reply to
/// WeChat.
pub struct WeixinDaemon {
    bot: Arc<WeChatBot>,
    workspace: PathBuf,
}

impl WeixinDaemon {
    /// Create a new daemon from options.
    pub fn new(opts: WeixinDaemonOptions) -> Self {
        let cred_path = opts.cred_path.unwrap_or_else(|| {
            // Default: ~/.recursive/<workspace_hash>/weixin_creds.json
            crate::paths::user_workspace_dir(&opts.workspace)
                .map(|d| d.join("weixin_creds.json"))
                .unwrap_or_else(|_| PathBuf::from("weixin_creds.json"))
        });

        let bot_opts = BotOptions {
            base_url: opts.base_url,
            cred_path: Some(cred_path.to_string_lossy().into_owned()),
            on_qr_url: Some(Box::new(|url| {
                render_qr_terminal(url);
            })),
            on_error: Some(Box::new(|e| {
                error!("WeChat iLink error: {e}");
            })),
        };

        Self {
            bot: Arc::new(WeChatBot::new(bot_opts)),
            workspace: opts.workspace,
        }
    }

    /// Login via QR code (or stored credentials).
    ///
    /// Prints the QR code to the terminal for the user to scan.
    /// If credentials are already stored and valid, this is a no-op.
    pub async fn login(&self, force: bool) -> wechatbot::Result<wechatbot::Credentials> {
        info!("WeChat: starting login (force={})", force);
        let creds = self.bot.login(force).await?;
        info!(
            "WeChat: logged in as {} (account: {})",
            creds.user_id, creds.account_id
        );
        Ok(creds)
    }

    /// Start the daemon background tasks.
    ///
    /// Returns:
    /// - A `JoinHandle` for the iLink polling task (let it run until dropped).
    /// - A `mpsc::Receiver<WeixinRequest>` for the backend worker to drain.
    pub fn start(
        self,
    ) -> (
        tokio::task::JoinHandle<()>,
        mpsc::UnboundedReceiver<WeixinRequest>,
    ) {
        let (raw_tx, mut raw_rx) = mpsc::unbounded_channel::<RawIncoming>();
        let (req_tx, req_rx) = mpsc::unbounded_channel::<WeixinRequest>();

        let bot = Arc::clone(&self.bot);
        let workspace = self.workspace.clone();

        // Register the message handler (sync closure → mpsc bridge).
        let raw_tx_handler = raw_tx.clone();
        let bot_for_handler = Arc::clone(&bot);
        tokio::spawn(async move {
            bot_for_handler
                .on_message(Box::new(move |msg| {
                    let _ = raw_tx_handler.send(RawIncoming {
                        user_id: msg.user_id.clone(),
                        text: msg.text.clone(),
                    });
                }))
                .await;
        });

        // Spawn the message processor.
        let bot_proc = Arc::clone(&bot);
        tokio::spawn(async move {
            while let Some(incoming) = raw_rx.recv().await {
                let preview: String = incoming.text.chars().take(80).collect();
                debug!("WeChat message from {}: {}", incoming.user_id, preview);

                if let Some(cmd) = parse_command(&incoming.text) {
                    handle_command(cmd, &bot_proc, &incoming, &workspace).await;
                } else {
                    // Regular message — forward to backend worker together
                    // with the sender's current binding, so `/c N` / `/r`
                    // are honoured on the message path.
                    let (reply_tx, reply_rx) = oneshot::channel();
                    let session_id = match WeixinSessionMap::for_workspace(&workspace)
                        .session_of(&incoming.user_id)
                    {
                        Ok(binding) => binding,
                        Err(e) => {
                            warn!("WeChat: session map read failed: {e}");
                            None
                        }
                    };
                    let req = WeixinRequest {
                        user_id: incoming.user_id.clone(),
                        text: incoming.text.clone(),
                        session_id,
                        reply_tx,
                    };
                    if req_tx.send(req).is_err() {
                        warn!("WeChat: backend worker channel closed");
                        break;
                    }
                    // Wait for the response and send it back.
                    match reply_rx.await {
                        Ok(Some(response)) if !response.is_empty() => {
                            if let Err(e) = bot_proc.send(&incoming.user_id, &response).await {
                                error!("WeChat send failed: {e}");
                            }
                        }
                        Ok(None) | Ok(Some(_)) => {
                            debug!("WeChat: empty response for {}", incoming.user_id);
                        }
                        Err(_) => {
                            warn!("WeChat: backend dropped reply channel");
                        }
                    }
                }
            }
        });

        // Spawn the iLink polling loop.
        let polling_handle = tokio::spawn(async move {
            if let Err(e) = self.bot.run().await {
                error!("WeChat polling stopped: {e}");
            }
        });

        (polling_handle, req_rx)
    }
}

// ---------------------------------------------------------------------------
// Command handlers
// ---------------------------------------------------------------------------

async fn handle_command(
    cmd: WeixinCommand,
    bot: &Arc<WeChatBot>,
    incoming: &RawIncoming,
    workspace: &std::path::Path,
) {
    let map = WeixinSessionMap::for_workspace(workspace);
    let reply = match cmd {
        WeixinCommand::Help => HELP_TEXT.to_string(),

        // /s — numbered list; each user's own binding is NOT consulted
        // here (the list is workspace-global, like `recursive resume`).
        WeixinCommand::Sessions => render_sessions_list(workspace),

        // /list N — the caller's own session's last N turns.
        WeixinCommand::List { count } => match map.session_of(&incoming.user_id) {
            Ok(Some(session_id)) => render_session_tail(workspace, &session_id, count),
            Ok(None) => {
                "你还没有会话。发送任意消息开始对话，或 /s 查看工作区会话列表。".to_string()
            }
            Err(e) => {
                warn!("WeChat /list: session map error: {e}");
                "读取会话绑定失败，请稍后重试。".to_string()
            }
        },

        // /c N — rebind the caller to the Nth most-recent session.
        WeixinCommand::Change { index } => match resolve_change(workspace, index) {
            Ok(session_id) => match map.bind(&incoming.user_id, &session_id) {
                Ok(()) => format!("✅ 已切换到会话 {session_id}。发送 /list 查看记录。"),
                Err(e) => {
                    warn!("WeChat /c: bind failed: {e}");
                    "切换会话失败，请稍后重试。".to_string()
                }
            },
            Err(msg) => msg,
        },

        // /r — drop the caller's binding; next message starts fresh.
        WeixinCommand::Reset => match map.unbind(&incoming.user_id) {
            Ok(true) => "🔄 已重置你的会话，下一条消息将开始新对话。".to_string(),
            Ok(false) => "你当前没有会话绑定。".to_string(),
            Err(e) => {
                warn!("WeChat /r: unbind failed: {e}");
                "重置失败，请稍后重试。".to_string()
            }
        },
    };

    if let Err(e) = bot.send(&incoming.user_id, &reply).await {
        error!("WeChat command reply failed: {e}");
    }
}

// ---------------------------------------------------------------------------
// QR code rendering
// ---------------------------------------------------------------------------

fn render_qr_terminal(url: &str) {
    // Try terminal QR rendering with the `qrcode` crate.
    // Falls back to plain URL if rendering fails.
    #[cfg(feature = "weixin")]
    {
        use qrcode::{render::unicode, QrCode};
        match QrCode::new(url.as_bytes()) {
            Ok(code) => {
                let image = code
                    .render::<unicode::Dense1x2>()
                    .dark_color(unicode::Dense1x2::Dark)
                    .light_color(unicode::Dense1x2::Light)
                    .build();
                eprintln!("\n{image}\n");
                eprintln!("📱 请用微信扫描上方二维码登录 ClawBot");
            }
            Err(_) => {
                eprintln!("\n📱 微信登录二维码URL: {url}\n请在微信扫描此链接。");
            }
        }
    }
    #[cfg(not(feature = "weixin"))]
    {
        eprintln!("\n📱 微信登录二维码URL: {url}");
    }
}

// ---------------------------------------------------------------------------
// Internal types
// ---------------------------------------------------------------------------

/// A raw incoming WeChat message before command parsing.
struct RawIncoming {
    user_id: String,
    text: String,
}
