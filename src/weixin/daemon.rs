//! WeChat iLink daemon — workspace-level singleton.
//!
//! [`WeixinDaemon`] manages a single iLink connection and exposes a
//! [`WeixinRequest`] channel that the agent backend (TUI or headless) listens
//! on. Incoming WeChat messages are parsed as commands or forwarded to the
//! current session's runtime.
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
        let req_tx_proc = req_tx.clone();
        tokio::spawn(async move {
            while let Some(incoming) = raw_rx.recv().await {
                let preview: String = incoming.text.chars().take(80).collect();
                debug!("WeChat message from {}: {}", incoming.user_id, preview);

                if let Some(cmd) = parse_command(&incoming.text) {
                    handle_command(cmd, &bot_proc, &incoming, &workspace, &req_tx_proc).await;
                } else {
                    // Regular message — forward to backend worker.
                    let (reply_tx, reply_rx) = oneshot::channel();
                    let req = WeixinRequest {
                        user_id: incoming.user_id.clone(),
                        text: incoming.text.clone(),
                        reply_tx,
                    };
                    if req_tx_proc.send(req).is_err() {
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
    _req_tx: &mpsc::UnboundedSender<WeixinRequest>,
) {
    let reply = match cmd {
        WeixinCommand::Help => HELP_TEXT.to_string(),

        WeixinCommand::Sessions => list_sessions(workspace),

        WeixinCommand::List { count } => {
            // Session history listing is handled by the backend via a
            // dedicated control command.  For now, inform the user.
            format!("最近 {count} 条对话记录查询正在开发中，请使用 TUI 查看完整历史。")
        }

        WeixinCommand::Change { index } => {
            format!("切换会话功能即将到来。当前仅支持单会话模式。(要切到第 {index} 个会话)")
        }

        WeixinCommand::Reset => {
            // Reset is handled by the backend; send a special marker message.
            // For now just acknowledge.
            "🔄 会话重置功能即将到来。".to_string()
        }
    };

    if let Err(e) = bot.send(&incoming.user_id, &reply).await {
        error!("WeChat command reply failed: {e}");
    }
}

fn list_sessions(workspace: &std::path::Path) -> String {
    let sessions_dir = match crate::paths::user_workspace_dir(workspace) {
        Ok(d) => d.join("sessions"),
        Err(_) => return "暂无会话记录。".to_string(),
    };
    let Ok(entries) = std::fs::read_dir(&sessions_dir) else {
        return "暂无会话记录。".to_string();
    };

    let mut sessions: Vec<(std::time::SystemTime, String)> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .filter_map(|e| {
            let modified = e.metadata().ok()?.modified().ok()?;
            let name = e.file_name().to_string_lossy().to_string();
            Some((modified, name))
        })
        .collect();

    sessions.sort_by_key(|s: &(std::time::SystemTime, String)| std::cmp::Reverse(s.0));
    sessions.truncate(10);

    if sessions.is_empty() {
        return "暂无会话记录。".to_string();
    }

    let mut lines = vec!["📋 工作区会话列表：".to_string()];
    for (i, (modified, id)) in sessions.iter().enumerate() {
        let ago = format_elapsed(*modified);
        let short_id = if id.len() > 12 { &id[..12] } else { id };
        lines.push(format!("[{}] {} ({})", i + 1, short_id, ago));
    }
    lines.push("\n发送 /c N 切换会话".to_string());
    lines.join("\n")
}

fn format_elapsed(modified: std::time::SystemTime) -> String {
    let Ok(elapsed) = modified.elapsed() else {
        return "未知".to_string();
    };
    let secs = elapsed.as_secs();
    if secs < 60 {
        format!("{secs}秒前")
    } else if secs < 3600 {
        format!("{}分钟前", secs / 60)
    } else if secs < 86400 {
        format!("{}小时前", secs / 3600)
    } else {
        format!("{}天前", secs / 86400)
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
