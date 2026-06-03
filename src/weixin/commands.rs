//! WeChat command parser.
//!
//! Parses `/`-prefixed commands that users can send from WeChat to control
//! the Recursive session. Regular messages (no `/` prefix) are passed
//! through as normal agent input.

/// A parsed WeChat control command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WeixinCommand {
    /// `/l` or `/list` — show last N turns of the current session.
    List { count: usize },
    /// `/s` or `/sessions` — list all sessions in the workspace.
    Sessions,
    /// `/c N` or `/change N` — switch to session N (1-based index).
    Change { index: usize },
    /// `/r` or `/reset` — reset the current session (start fresh).
    Reset,
    /// `/help` — show available commands.
    Help,
}

/// Try to parse a WeChat command from a text message.
///
/// Returns `Some(WeixinCommand)` when the text starts with `/` and matches
/// a known command. Returns `None` for regular (non-command) messages.
pub fn parse_command(text: &str) -> Option<WeixinCommand> {
    let text = text.trim();
    if !text.starts_with('/') {
        return None;
    }

    let parts: Vec<&str> = text.splitn(3, ' ').collect();
    let cmd = parts[0].to_lowercase();

    match cmd.as_str() {
        "/l" | "/list" => {
            let count = parts.get(1).and_then(|s| s.parse().ok()).unwrap_or(10);
            Some(WeixinCommand::List { count })
        }
        "/s" | "/sessions" => Some(WeixinCommand::Sessions),
        "/c" | "/change" => {
            let index = parts.get(1).and_then(|s| s.parse::<usize>().ok())?;
            Some(WeixinCommand::Change { index })
        }
        "/r" | "/reset" => Some(WeixinCommand::Reset),
        "/help" | "/h" => Some(WeixinCommand::Help),
        _ => None,
    }
}

/// Help text sent back to the WeChat user.
pub const HELP_TEXT: &str = "\
📱 Recursive WeChat 命令：
/l [N]    — 列出最近 N 条对话（默认10条）
/s        — 列出所有会话
/c N      — 切换到第 N 个会话
/r        — 重置当前会话（清空上下文）
/help     — 显示此帮助

发送普通消息与 Agent 对话。";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_list_default() {
        assert_eq!(parse_command("/l"), Some(WeixinCommand::List { count: 10 }));
        assert_eq!(
            parse_command("/list"),
            Some(WeixinCommand::List { count: 10 })
        );
    }

    #[test]
    fn test_list_with_count() {
        assert_eq!(
            parse_command("/l 5"),
            Some(WeixinCommand::List { count: 5 })
        );
    }

    #[test]
    fn test_sessions() {
        assert_eq!(parse_command("/s"), Some(WeixinCommand::Sessions));
        assert_eq!(parse_command("/sessions"), Some(WeixinCommand::Sessions));
    }

    #[test]
    fn test_change() {
        assert_eq!(
            parse_command("/c 2"),
            Some(WeixinCommand::Change { index: 2 })
        );
        assert_eq!(
            parse_command("/change 3"),
            Some(WeixinCommand::Change { index: 3 })
        );
        assert_eq!(parse_command("/c"), None);
    }

    #[test]
    fn test_reset() {
        assert_eq!(parse_command("/r"), Some(WeixinCommand::Reset));
        assert_eq!(parse_command("/reset"), Some(WeixinCommand::Reset));
    }

    #[test]
    fn test_regular_message() {
        assert_eq!(parse_command("hello"), None);
        assert_eq!(parse_command("what is rust?"), None);
    }

    #[test]
    fn test_unknown_command() {
        assert_eq!(parse_command("/unknown"), None);
    }
}
