use std::io;

use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
    ExecutableCommand,
};
use ratatui::{
    prelude::*,
    widgets::{Block, Borders, Paragraph, Wrap},
};

struct App {
    input: String,
    messages: Vec<String>,
    should_quit: bool,
}

impl App {
    fn new() -> Self {
        Self {
            input: String::new(),
            messages: vec!["Welcome to Recursive TUI. Type a message and press Enter.".into()],
            should_quit: false,
        }
    }

    fn handle_key(&mut self, key: KeyCode) {
        match key {
            KeyCode::Enter => {
                if !self.input.is_empty() {
                    let msg = format!("You: {}", self.input);
                    self.messages.push(msg);
                    self.input.clear();
                }
            }
            KeyCode::Char('q') if self.input.is_empty() => {
                self.should_quit = true;
            }
            KeyCode::Char(c) => {
                self.input.push(c);
            }
            KeyCode::Backspace => {
                self.input.pop();
            }
            KeyCode::Esc => {
                self.should_quit = true;
            }
            _ => {}
        }
    }
}

fn main() -> io::Result<()> {
    // Setup terminal
    enable_raw_mode()?;
    io::stdout().execute(EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;

    let mut app = App::new();

    // Main loop
    loop {
        terminal.draw(|frame| ui(frame, &app))?;

        if event::poll(std::time::Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    app.handle_key(key.code);
                }
            }
        }

        if app.should_quit {
            break;
        }
    }

    // Restore terminal
    disable_raw_mode()?;
    io::stdout().execute(LeaveAlternateScreen)?;
    Ok(())
}

fn ui(frame: &mut Frame, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(3),    // messages
            Constraint::Length(3), // input
        ])
        .split(frame.area());

    // Messages panel
    let messages_text = app.messages.join("\n");
    let messages = Paragraph::new(messages_text)
        .block(Block::default().borders(Borders::ALL).title(" Messages "))
        .wrap(Wrap { trim: false });
    frame.render_widget(messages, chunks[0]);

    // Input panel
    let input = Paragraph::new(app.input.as_str())
        .block(Block::default().borders(Borders::ALL).title(" Input "));
    frame.render_widget(input, chunks[1]);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_new_creates_empty_state() {
        let app = App::new();
        assert!(app.input.is_empty());
        assert!(!app.messages.is_empty()); // welcome message
        assert!(!app.should_quit);
    }

    #[test]
    fn enter_moves_input_to_messages() {
        let mut app = App::new();
        app.input = "hello".to_string();
        app.handle_key(KeyCode::Enter);
        assert!(app.input.is_empty());
        assert!(app.messages.last().unwrap().contains("hello"));
    }

    #[test]
    fn esc_sets_should_quit() {
        let mut app = App::new();
        app.handle_key(KeyCode::Esc);
        assert!(app.should_quit);
    }

    #[test]
    fn char_appends_to_input() {
        let mut app = App::new();
        app.handle_key(KeyCode::Char('h'));
        app.handle_key(KeyCode::Char('i'));
        assert_eq!(app.input, "hi");
    }

    #[test]
    fn backspace_removes_last_char() {
        let mut app = App::new();
        app.input = "hello".to_string();
        app.handle_key(KeyCode::Backspace);
        assert_eq!(app.input, "hell");
    }
}
