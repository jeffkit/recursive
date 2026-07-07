//! `agui-tui` binary entry point — wires the pure [`App`] to a real
//! terminal and to an [`AguiClient`] over SSE.

use std::{io, time::Duration};

use agui_client::{AguiClient, RunAgentInput};
use agui_protocol::{Event, Message};
use anyhow::{Context, Result};
use clap::Parser;
use crossterm::{
    event::{self, KeyCode as CtKeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};
use tokio::sync::mpsc;
use tracing_subscriber::EnvFilter;
use url::Url;
use uuid::Uuid;

use agui_tui::app::{resume_payload, App, Command, KeyCode, KeyInput};
use agui_tui::ui::render;

#[derive(Parser, Debug)]
#[command(name = "agui-tui", about = "Generic terminal UI for any AG-UI server")]
struct Cli {
    /// Endpoint URL to POST `RunAgentInput` to.
    endpoint: Url,

    /// Add a header to every request. Repeatable. Format: `Key: Value`.
    #[arg(long = "header", short = 'H', value_parser = parse_header)]
    headers: Vec<(String, String)>,

    /// Reuse an existing thread_id instead of generating one.
    #[arg(long)]
    thread_id: Option<String>,

    /// Tracing filter. Accepts the standard `RUST_LOG` syntax.
    #[arg(long, default_value = "warn")]
    log: String,
}

fn parse_header(raw: &str) -> Result<(String, String), String> {
    let (k, v) = raw
        .split_once(':')
        .ok_or_else(|| format!("expected `Key: Value`, got `{raw}`"))?;
    Ok((k.trim().to_string(), v.trim().to_string()))
}

/// Logs go to a sibling file because the TUI owns stdout/stderr while
/// raw mode is active. If the file can't be opened we silently drop
/// to in-memory (fmt is only constructed once).
fn init_tracing(filter: &str) {
    let env = EnvFilter::try_new(filter).unwrap_or_else(|_| EnvFilter::new("warn"));
    let subscriber = tracing_subscriber::fmt()
        .with_env_filter(env)
        .with_writer(io::stderr)
        .with_ansi(false)
        .finish();
    let _ = tracing::subscriber::set_global_default(subscriber);
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    init_tracing(&cli.log);

    let thread_id = cli
        .thread_id
        .clone()
        .unwrap_or_else(|| Uuid::new_v4().to_string());

    let mut client = AguiClient::new(cli.endpoint.clone());
    for (k, v) in &cli.headers {
        client = client
            .with_header(k, v)
            .map_err(|e| anyhow::anyhow!("invalid --header `{k}: {v}`: {e}"))?;
    }

    // Terminal setup. We unwind on panic so the user's terminal isn't
    // left in raw mode if anything below blows up.
    enable_raw_mode().context("enable raw mode")?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen).context("enter alt screen")?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).context("ratatui init")?;

    let app = App::new(thread_id);
    let result = run_loop(&mut terminal, app, client).await;

    disable_raw_mode().ok();
    execute!(terminal.backend_mut(), LeaveAlternateScreen).ok();
    terminal.show_cursor().ok();

    result
}

async fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    mut app: App,
    client: AguiClient,
) -> Result<()> {
    // Keypresses arrive on this channel from a blocking poll thread.
    let (key_tx, mut key_rx) = mpsc::unbounded_channel::<KeyInput>();
    let (shutdown_tx, shutdown_rx) = std::sync::mpsc::channel::<()>();
    let key_handle = std::thread::spawn(move || key_pump(key_tx, shutdown_rx));

    // The currently-active server stream, if any. `None` between runs.
    let mut current_rx: Option<mpsc::UnboundedReceiver<Event>> = None;

    loop {
        terminal.draw(|f| render(f, &app))?;

        tokio::select! {
            biased;

            maybe_key = key_rx.recv() => {
                let Some(key) = maybe_key else { break };
                let cmds = app.handle_key(key);
                for cmd in cmds {
                    handle_command(&client, &mut app, &mut current_rx, cmd).await;
                }
                if app.should_quit { break }
            }

            maybe_ev = recv_optional(&mut current_rx) => {
                match maybe_ev {
                    Some(ev) => app.apply_event(ev),
                    None => {
                        // Stream ended — drop it so the next iteration
                        // doesn't keep selecting on a dead receiver.
                        current_rx = None;
                    }
                }
            }
        }
    }

    let _ = shutdown_tx.send(());
    let _ = key_handle.join();
    Ok(())
}

/// Helper that turns `Option<&mut Receiver>` into something
/// `tokio::select!` can poll. When there's no receiver, we sleep
/// briefly so the select still has a future to await.
async fn recv_optional(rx: &mut Option<mpsc::UnboundedReceiver<Event>>) -> Option<Event> {
    match rx {
        Some(r) => r.recv().await,
        None => {
            // Park forever — rely on the keypress arm to wake us.
            std::future::pending::<Option<Event>>().await
        }
    }
}

async fn handle_command(
    client: &AguiClient,
    app: &mut App,
    current_rx: &mut Option<mpsc::UnboundedReceiver<Event>>,
    cmd: Command,
) {
    match cmd {
        Command::Quit => {
            // Drop any in-flight stream so the receiver-side task exits.
            *current_rx = None;
        }
        Command::SendUserMessage { text } => {
            let msg_id = format!("u-{}", Uuid::new_v4());
            app.record_user_message(msg_id.clone(), text.clone());
            let run_id = Uuid::new_v4().to_string();
            app.state.run_id = Some(run_id.clone());
            let input = RunAgentInput {
                thread_id: app.state.thread_id.clone(),
                run_id,
                messages: vec![Message {
                    id: msg_id,
                    role: "user".into(),
                    content: Some(text),
                    ..Default::default()
                }],
                tools: vec![],
                context: vec![],
                resume: None,
                state: None,
                interrupt_before: None,
                forwarded_props: None,
            };
            start_run(client, app, current_rx, input).await;
        }
        Command::Resume {
            interrupt_id,
            approve,
        } => {
            let run_id = Uuid::new_v4().to_string();
            app.state.run_id = Some(run_id.clone());
            let input = RunAgentInput {
                thread_id: app.state.thread_id.clone(),
                run_id,
                messages: vec![],
                tools: vec![],
                context: vec![],
                resume: Some(resume_payload(interrupt_id, approve)),
                state: None,
                interrupt_before: None,
                forwarded_props: None,
            };
            start_run(client, app, current_rx, input).await;
        }
    }
}

async fn start_run(
    client: &AguiClient,
    app: &mut App,
    current_rx: &mut Option<mpsc::UnboundedReceiver<Event>>,
    input: RunAgentInput,
) {
    match client.run(input).await {
        Ok(rx) => {
            *current_rx = Some(rx);
            app.state.running = true;
        }
        Err(e) => {
            app.status = Some(format!("client error: {e}"));
            tracing::warn!(error = %e, "client.run failed");
        }
    }
}

/// Blocking thread that converts crossterm key events into our
/// internal [`KeyInput`] enum and pushes them across `tx`. Exits
/// when the shutdown signal fires or the receiver is dropped.
fn key_pump(tx: mpsc::UnboundedSender<KeyInput>, shutdown: std::sync::mpsc::Receiver<()>) {
    loop {
        if shutdown.try_recv().is_ok() {
            break;
        }
        match event::poll(Duration::from_millis(100)) {
            Ok(true) => {}
            _ => continue,
        }
        let evt = match event::read() {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(error = %e, "crossterm read error");
                continue;
            }
        };
        if let event::Event::Key(k) = evt {
            if let Some(mapped) = map_key(k) {
                if tx.send(mapped).is_err() {
                    break;
                }
            }
        }
    }
}

fn map_key(k: KeyEvent) -> Option<KeyInput> {
    if k.kind == KeyEventKind::Release {
        return None;
    }
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
    let code = match k.code {
        CtKeyCode::Char(c) => KeyCode::Char(c),
        CtKeyCode::Enter => KeyCode::Enter,
        CtKeyCode::Tab => KeyCode::Tab,
        CtKeyCode::Backspace => KeyCode::Backspace,
        CtKeyCode::Esc => KeyCode::Esc,
        CtKeyCode::Up => KeyCode::Up,
        CtKeyCode::Down => KeyCode::Down,
        _ => return None,
    };
    Some(KeyInput { code, ctrl })
}
