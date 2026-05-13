//! Terminal UI module using ratatui + crossterm.

use crossterm::event::{Event as CrosstermEvent, EventStream, KeyCode, KeyEvent, KeyModifiers};
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use futures_util::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::Terminal;
use tokio_stream::wrappers::ReceiverStream;

use crate::agent::AgentHandle;
use crate::event::Event;

// === AppEvent ===

enum AppEvent {
    Key(KeyEvent),
    Resize(u16, u16),
    AgentEvent(Event),
    AgentGone,
}

// === DisplayState ===

struct DisplayState {
    output_lines: Vec<String>,
    streaming_text: String,
    model_name: String,
    turn_active: bool,
    active_tool: Option<String>,
    total_tokens: u32,
    input: String,
    input_cursor: usize,
    scroll_offset: u16,
}

impl DisplayState {
    fn new(model_name: String) -> Self {
        Self {
            output_lines: Vec::new(),
            streaming_text: String::new(),
            model_name,
            turn_active: false,
            active_tool: None,
            total_tokens: 0,
            input: String::new(),
            input_cursor: 0,
            scroll_offset: 0,
        }
    }
}

// === TuiGuard ===

struct TuiGuard {
    terminal: Terminal<CrosstermBackend<std::io::Stdout>>,
}

impl TuiGuard {
    fn init() -> Result<Self, Box<dyn std::error::Error>> {
        terminal::enable_raw_mode()?;
        crossterm::execute!(std::io::stdout(), EnterAlternateScreen)?;
        let backend = CrosstermBackend::new(std::io::stdout());
        let terminal = Terminal::new(backend)?;
        Ok(Self { terminal })
    }
}

impl Drop for TuiGuard {
    fn drop(&mut self) {
        let _ = self.terminal.show_cursor();
        let _ = crossterm::execute!(std::io::stdout(), LeaveAlternateScreen);
        let _ = terminal::disable_raw_mode();
    }
}

// === Render ===

fn render(state: &DisplayState, terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>) {
    let _ = terminal.draw(|frame| {
        let size = frame.area();
        let chunks = Layout::vertical([
            Constraint::Min(3),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(size);

        // Output area
        let mut lines: Vec<Line> = state
            .output_lines
            .iter()
            .map(|l| Line::from(Span::raw(l.clone())))
            .collect();

        if !state.streaming_text.is_empty() {
            lines.push(Line::from(Span::styled(
                &state.streaming_text,
                Style::default().fg(Color::Green),
            )));
        }

        if let Some(ref tool) = state.active_tool {
            lines.push(Line::from(Span::styled(
                format!("  [tool: {}]", tool),
                Style::default().fg(Color::Yellow),
            )));
        }

        let output = Paragraph::new(lines)
            .block(Block::default().borders(Borders::NONE))
            .wrap(Wrap { trim: false })
            .scroll((state.scroll_offset, 0));
        frame.render_widget(output, chunks[0]);

        // Status bar
        let status_text = if state.turn_active {
            "busy"
        } else {
            "idle"
        };
        let status = Line::from(vec![
            Span::styled(
                &state.model_name,
                Style::default().fg(Color::Cyan),
            ),
            Span::raw(" | "),
            Span::styled(
                status_text,
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::raw(format!(" | tok: {}", state.total_tokens)),
        ]);
        frame.render_widget(
            Paragraph::new(status),
            chunks[1],
        );

        // Input area
        let input_display = format!("> {}", &state.input);
        let input_widget = Paragraph::new(input_display)
            .style(Style::default().fg(Color::White));
        frame.render_widget(input_widget, chunks[2]);

        // Place cursor after "> " + input_cursor chars
        let cursor_x = 2 + state.input_cursor as u16;
        let _ = frame.set_cursor_position((cursor_x, chunks[2].y));
    });
}

// === Key handling ===

fn handle_key_event(key: KeyEvent, state: &mut DisplayState) -> Option<TuiAction> {
    match (key.modifiers, key.code) {
        (KeyModifiers::CONTROL, KeyCode::Char('c')) => {
            if state.turn_active {
                Some(TuiAction::Interrupt)
            } else {
                Some(TuiAction::Quit)
            }
        }
        (KeyModifiers::CONTROL, KeyCode::Char('d')) => Some(TuiAction::Quit),
        (_, KeyCode::Enter) => {
            if !state.input.is_empty() {
                let text = state.input.clone();
                state.input.clear();
                state.input_cursor = 0;
                Some(TuiAction::Submit(text))
            } else {
                None
            }
        }
        (_, KeyCode::Char(c)) => {
            state.input.insert(state.input_cursor, c);
            state.input_cursor += 1;
            None
        }
        (_, KeyCode::Backspace) => {
            if state.input_cursor > 0 {
                state.input_cursor -= 1;
                state.input.remove(state.input_cursor);
            }
            None
        }
        (_, KeyCode::Left) => {
            if state.input_cursor > 0 {
                state.input_cursor -= 1;
            }
            None
        }
        (_, KeyCode::Right) => {
            if state.input_cursor < state.input.len() {
                state.input_cursor += 1;
            }
            None
        }
        (_, KeyCode::Up) => {
            state.scroll_offset = state.scroll_offset.saturating_add(1);
            None
        }
        (_, KeyCode::Down) => {
            state.scroll_offset = state.scroll_offset.saturating_sub(1);
            None
        }
        _ => None,
    }
}

enum TuiAction {
    Submit(String),
    Interrupt,
    Quit,
}

// === Agent event handling ===

fn handle_agent_event(event: Event, state: &mut DisplayState) {
    match event {
        Event::TurnStarted => {
            state.turn_active = true;
            state.active_tool = None;
        }
        Event::TextDelta(delta) => {
            state.streaming_text.push_str(&delta);
        }
        Event::TextDone(text) => {
            state.streaming_text.clear();
            state.output_lines.push(text);
        }
        Event::ToolCallBegin { name, .. } => {
            state.active_tool = Some(name.clone());
            state.output_lines.push(format!("  [calling {}]", name));
        }
        Event::ToolCallEnd {
            name, is_error, ..
        } => {
            state.active_tool = None;
            if is_error {
                state.output_lines.push(format!("  [{} failed]", name));
            } else {
                state.output_lines.push(format!("  [{} done]", name));
            }
        }
        Event::TurnComplete { usage } => {
            state.turn_active = false;
            if let Some(u) = usage {
                if let Some(total) = u.total_tokens {
                    state.total_tokens = state.total_tokens.saturating_add(total);
                } else {
                    let sum = u.input_tokens.unwrap_or(0) + u.output_tokens.unwrap_or(0);
                    state.total_tokens = state.total_tokens.saturating_add(sum);
                }
            }
        }
        Event::TurnInterrupted => {
            state.turn_active = false;
            if !state.streaming_text.is_empty() {
                state
                    .output_lines
                    .push(std::mem::take(&mut state.streaming_text));
            }
            state.output_lines.push("[interrupted]".to_string());
        }
        Event::Error(msg) => {
            state.turn_active = false;
            state.output_lines.push(format!("[error] {}", msg));
        }
        Event::ApprovalRequired { description, .. } => {
            state.output_lines.push(format!("[approval] {}", description));
        }
    }
}

// === Main TUI loop ===

pub async fn run_tui(handle: AgentHandle, model_name: String) -> Result<(), Box<dyn std::error::Error>> {
    let mut guard = TuiGuard::init()?;
    let mut state = DisplayState::new(model_name);

    let crossterm_stream = EventStream::new();

    // Merge agent and crossterm events into one stream
    let (agent_tx, agent_rx) = tokio::sync::mpsc::channel::<AppEvent>(64);
    let agent_rx_stream = ReceiverStream::new(agent_rx);

    // Spawn a task to forward agent events
    let agent_forward_tx = agent_tx.clone();
    let event_handle = handle.clone();
    tokio::spawn(async move {
        while let Some(event) = event_handle.recv_event().await {
            if agent_forward_tx.send(AppEvent::AgentEvent(event)).await.is_err() {
                break;
            }
        }
        let _ = agent_forward_tx.send(AppEvent::AgentGone).await;
    });

    // Also forward crossterm events
    let crossterm_forward_tx = agent_tx.clone();
    tokio::spawn(async move {
        let mut stream = crossterm_stream.boxed();
        while let Some(event) = stream.next().await {
            let app_event = match event {
                Ok(CrosstermEvent::Key(key)) => AppEvent::Key(key),
                Ok(CrosstermEvent::Resize(w, h)) => AppEvent::Resize(w, h),
                _ => continue,
            };
            if crossterm_forward_tx.send(app_event).await.is_err() {
                break;
            }
        }
    });

    // Drain merged event stream
    let mut merged = agent_rx_stream.boxed();

    loop {
        let event = match merged.next().await {
            Some(e) => e,
            None => break,
        };

        match event {
            AppEvent::Key(key) => {
                if let Some(action) = handle_key_event(key, &mut state) {
                    match action {
                        TuiAction::Submit(text) => {
                            state.output_lines.push(format!("> {}", text));
                            if handle.user_turn(text).await.is_err() {
                                state.output_lines.push("[agent closed]".to_string());
                                break;
                            }
                        }
                        TuiAction::Interrupt => {
                            let _ = handle.interrupt().await;
                        }
                        TuiAction::Quit => break,
                    }
                }
            }
            AppEvent::Resize(_, _) => {}
            AppEvent::AgentEvent(event) => {
                handle_agent_event(event, &mut state);
            }
            AppEvent::AgentGone => {
                state.output_lines.push("[agent shut down]".to_string());
                render(&state, &mut guard.terminal);
                break;
            }
        }

        render(&state, &mut guard.terminal);
    }

    Ok(())
}
