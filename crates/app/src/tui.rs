//! Ratatui TUI client for the Tasks platform.
//!
//! Provides a terminal UI showing task list, session output, merge queue,
//! and system status. Subscribes to the event bus for live updates.

use std::collections::{HashMap, VecDeque};
use std::io::{self, Stdout};
use std::sync::Arc;
use std::time::Duration;

use crossterm::event::{Event as CtEvent, EventStream, KeyCode, KeyEvent, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use futures::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Terminal;

use events::{Event as PlatformEvent, EventBus, EventType};
use models::merge_queue::MergeStatus;
use models::task::TaskState;
use server::Server;

/// Maximum number of log lines to keep per session.
const MAX_SESSION_LINES: usize = 500;

/// TUI application state.
pub struct TuiState {
    /// Snapshot of tasks: (id, title, state).
    pub tasks: Vec<(String, String, TaskState)>,
    /// Which task is selected in the list.
    pub task_list_state: ListState,
    /// Session output lines per task_id.
    pub session_logs: HashMap<String, VecDeque<String>>,
    /// Merge queue entries: (id, task_id, status).
    pub merge_entries: Vec<(String, String, MergeStatus)>,
    /// Current mode.
    pub mode: server::Mode,
    /// Active session count.
    pub active_sessions: usize,
    /// Max sessions.
    pub max_sessions: u32,
    /// Number of projects.
    pub project_count: usize,
    /// Whether the app should quit.
    pub should_quit: bool,
}

impl TuiState {
    fn new(max_sessions: u32) -> Self {
        let mut state = ListState::default();
        state.select(Some(0));
        Self {
            tasks: Vec::new(),
            task_list_state: state,
            session_logs: HashMap::new(),
            merge_entries: Vec::new(),
            mode: server::Mode::Pause,
            active_sessions: 0,
            max_sessions,
            project_count: 0,
            should_quit: false,
        }
    }

    fn selected_task_id(&self) -> Option<&str> {
        let idx = self.task_list_state.selected()?;
        self.tasks.get(idx).map(|(id, _, _)| id.as_str())
    }

    fn select_next(&mut self) {
        if self.tasks.is_empty() {
            return;
        }
        let i = self
            .task_list_state
            .selected()
            .map(|i| (i + 1).min(self.tasks.len() - 1))
            .unwrap_or(0);
        self.task_list_state.select(Some(i));
    }

    fn select_prev(&mut self) {
        if self.tasks.is_empty() {
            return;
        }
        let i = self
            .task_list_state
            .selected()
            .map(|i| i.saturating_sub(1))
            .unwrap_or(0);
        self.task_list_state.select(Some(i));
    }

    fn append_session_log(&mut self, task_id: &str, line: String) {
        let log = self
            .session_logs
            .entry(task_id.to_string())
            .or_insert_with(|| VecDeque::with_capacity(MAX_SESSION_LINES));
        if log.len() >= MAX_SESSION_LINES {
            log.pop_front();
        }
        log.push_back(line);
    }
}

/// Refresh the TUI state from server state.
async fn refresh_from_server(tui: &mut TuiState, server: &Server) {
    let state = server.state.read().await;

    // Tasks — sort: running first, then by created_at desc.
    let mut tasks: Vec<(String, String, TaskState)> = state
        .tasks
        .values()
        .map(|t| (t.id.clone(), t.title.clone(), t.state))
        .collect();
    tasks.sort_by(|a, b| {
        let a_active = matches!(a.2, TaskState::Running | TaskState::Question);
        let b_active = matches!(b.2, TaskState::Running | TaskState::Question);
        b_active.cmp(&a_active)
    });
    tui.tasks = tasks;

    // Mode
    tui.mode = state.mode;

    // Active sessions
    tui.active_sessions = state
        .tasks
        .values()
        .filter(|t| matches!(t.state, TaskState::Running | TaskState::Question | TaskState::Testing))
        .count();

    // Projects
    tui.project_count = state.projects.len();

    // Merge queue
    tui.merge_entries = state
        .merge_queue
        .entries()
        .iter()
        .map(|e| (e.id.clone(), e.task_id.clone(), e.status))
        .collect();

    // Preserve selection
    if let Some(selected) = tui.task_list_state.selected() {
        if selected >= tui.tasks.len() && !tui.tasks.is_empty() {
            tui.task_list_state.select(Some(tui.tasks.len() - 1));
        }
    }
}

/// Handle a platform event — update TUI state.
fn handle_platform_event(tui: &mut TuiState, event: &PlatformEvent) {
    match &event.event_type {
        EventType::AgentMessage => {
            if let Some(text) = event.data.get("text").and_then(|v| v.as_str()) {
                // Trim long lines for display (UTF-8 safe).
                let line = match text.char_indices().nth(200) {
                    Some((i, _)) => format!("{}...", &text[..i]),
                    None => text.to_string(),
                };
                tui.append_session_log(&event.task, line);
            }
        }
        _ => {}
    }
}

/// Handle keyboard input.
async fn handle_key(tui: &mut TuiState, key: KeyEvent, server: &Server) {
    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => tui.should_quit = true,
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            tui.should_quit = true
        }
        KeyCode::Down | KeyCode::Char('j') => tui.select_next(),
        KeyCode::Up | KeyCode::Char('k') => tui.select_prev(),
        KeyCode::Char('p') => {
            let _ = server
                .set_mode(server::Mode::Pause, &events::Actor::Human)
                .await;
        }
        KeyCode::Char('s') => {
            let _ = server
                .set_mode(server::Mode::Stop, &events::Actor::Human)
                .await;
        }
        KeyCode::Char('g') => {
            let _ = server
                .set_mode(server::Mode::Play, &events::Actor::Human)
                .await;
        }
        _ => {}
    }
}

/// Draw the TUI.
fn draw(terminal: &mut Terminal<CrosstermBackend<Stdout>>, tui: &TuiState) -> io::Result<()> {
    terminal.draw(|f| {
        let size = f.area();

        // Main layout: status bar, body, keybindings.
        let main_chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1), // status bar
                Constraint::Min(5),   // body
                Constraint::Length(1), // keybindings
            ])
            .split(size);

        // Status bar
        draw_status_bar(f, main_chunks[0], tui);

        // Body: left column (tasks + merge queue) | right pane (session output)
        let body_chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(35), Constraint::Percentage(65)])
            .split(main_chunks[1]);

        // Left column: tasks on top, merge queue on bottom.
        let left_chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(70), Constraint::Percentage(30)])
            .split(body_chunks[0]);

        draw_task_list(f, left_chunks[0], tui);
        draw_merge_queue(f, left_chunks[1], tui);
        draw_session_output(f, body_chunks[1], tui);

        // Keybindings bar
        draw_keybindings(f, main_chunks[2]);
    })?;
    Ok(())
}

fn draw_status_bar(f: &mut ratatui::Frame, area: Rect, tui: &TuiState) {
    let mode_str = match tui.mode {
        server::Mode::Stop => ("■ Stop", Color::Red),
        server::Mode::Pause => ("▮▮ Pause", Color::Yellow),
        server::Mode::Play => ("▶ Play", Color::Green),
    };

    let line = Line::from(vec![
        Span::styled(
            format!(" {} ", mode_str.0),
            Style::default().fg(Color::Black).bg(mode_str.1),
        ),
        Span::raw(format!(
            "  {}/{} sessions  │  {} projects  │  {} tasks",
            tui.active_sessions,
            tui.max_sessions,
            tui.project_count,
            tui.tasks.len(),
        )),
    ]);
    f.render_widget(Paragraph::new(line), area);
}

fn draw_task_list(f: &mut ratatui::Frame, area: Rect, tui: &TuiState) {
    let items: Vec<ListItem> = tui
        .tasks
        .iter()
        .map(|(id, title, state)| {
            let (icon, color) = match state {
                TaskState::Running => ("●", Color::Green),
                TaskState::Question => ("?", Color::Yellow),
                TaskState::Waiting => ("○", Color::DarkGray),
                TaskState::Blocked => ("⊘", Color::DarkGray),
                TaskState::Testing => ("◎", Color::Cyan),
                TaskState::AwaitingMerge => ("✓", Color::Blue),
                TaskState::Completed => ("✓", Color::Green),
                TaskState::Failed => ("✗", Color::Red),
                TaskState::Cancelled => ("—", Color::DarkGray),
                TaskState::Conflict => ("!", Color::Red),
            };

            // Truncate title to fit (UTF-8 safe).
            let display_title = match title.char_indices().nth(29) {
                Some((i, _)) if title.len() > i => format!("{}…", &title[..i]),
                _ => title.clone(),
            };

            // Extract issue number from task ID if possible.
            let num = id.rsplit('-').next().unwrap_or("");

            ListItem::new(Line::from(vec![
                Span::styled(format!("{icon} "), Style::default().fg(color)),
                Span::styled(
                    format!("#{num} "),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::raw(display_title),
            ]))
        })
        .collect();

    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(" Tasks "))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));

    f.render_stateful_widget(list, area, &mut tui.task_list_state.clone());
}

fn draw_merge_queue(f: &mut ratatui::Frame, area: Rect, tui: &TuiState) {
    let items: Vec<ListItem> = tui
        .merge_entries
        .iter()
        .map(|(_, task_id, status)| {
            let (icon, color) = match status {
                MergeStatus::Pending => ("○", Color::Yellow),
                MergeStatus::Approved => ("✓", Color::Green),
                MergeStatus::Rejected => ("✗", Color::Red),
                MergeStatus::Merged => ("●", Color::Green),
                MergeStatus::Conflict => ("!", Color::Red),
            };
            let num = task_id.rsplit('-').next().unwrap_or("");
            ListItem::new(Line::from(vec![
                Span::styled(format!("{icon} "), Style::default().fg(color)),
                Span::styled(format!("#{num}"), Style::default().fg(Color::DarkGray)),
                Span::raw(format!(" {:?}", status)),
            ]))
        })
        .collect();

    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(" Merge Queue "));
    f.render_widget(list, area);
}

fn draw_session_output(f: &mut ratatui::Frame, area: Rect, tui: &TuiState) {
    let selected_id = tui.selected_task_id();
    let title = match selected_id {
        Some(id) => {
            let num = id.rsplit('-').next().unwrap_or(id);
            format!(" Session #{num} ")
        }
        None => " Session ".to_string(),
    };

    let lines: Vec<Line> = selected_id
        .and_then(|id| tui.session_logs.get(id))
        .map(|log| log.iter().map(|l| Line::raw(l.as_str())).collect())
        .unwrap_or_else(|| vec![Line::raw("No session output.")]);

    let paragraph = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title(title))
        .wrap(Wrap { trim: false })
        .scroll((
            // Auto-scroll to bottom.
            selected_id
                .and_then(|id| tui.session_logs.get(id))
                .map(|log| {
                    let visible_height = area.height.saturating_sub(2) as usize;
                    log.len().saturating_sub(visible_height) as u16
                })
                .unwrap_or(0),
            0,
        ));

    f.render_widget(paragraph, area);
}

fn draw_keybindings(f: &mut ratatui::Frame, area: Rect) {
    let line = Line::from(vec![
        Span::styled(" q ", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw("quit  "),
        Span::styled("p ", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw("pause  "),
        Span::styled("s ", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw("stop  "),
        Span::styled("g ", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw("play  "),
        Span::styled("↑↓ ", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw("select"),
    ]);
    f.render_widget(
        Paragraph::new(line).style(Style::default().fg(Color::DarkGray)),
        area,
    );
}

/// RAII guard to restore terminal state on drop.
struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
    }
}

/// Run the TUI event loop. Takes ownership of the terminal.
///
/// This is called from the run loop after all components are started.
pub async fn run_tui(
    server: Arc<Server>,
    event_bus: Arc<EventBus>,
    max_sessions: u32,
) -> io::Result<()> {
    // Setup terminal. The guard ensures cleanup even on early return/panic.
    enable_raw_mode()?;
    let _guard = TerminalGuard;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut tui = TuiState::new(max_sessions);
    let mut event_rx = event_bus.subscribe();
    let mut term_events = EventStream::new();

    // Initial state load.
    refresh_from_server(&mut tui, &server).await;

    let tick_rate = Duration::from_millis(250);

    loop {
        // Draw.
        draw(&mut terminal, &tui)?;

        tokio::select! {
            // Platform events from the event bus.
            result = event_rx.recv() => {
                if let Ok(event) = result {
                    handle_platform_event(&mut tui, &event);
                    // Refresh full state on state-change events.
                    if event.event_type.as_str().starts_with("task:state:")
                        || event.event_type.as_str().starts_with("system:mode:")
                        || event.event_type.as_str().starts_with("merge:")
                    {
                        refresh_from_server(&mut tui, &server).await;
                    }
                }
            }
            // Terminal input events (async, non-blocking).
            maybe_event = term_events.next() => {
                if let Some(Ok(CtEvent::Key(key))) = maybe_event {
                    handle_key(&mut tui, key, &server).await;
                }
            }
            // Periodic redraw tick.
            _ = tokio::time::sleep(tick_rate) => {}
        }

        if tui.should_quit {
            break;
        }
    }

    // Guard handles cleanup via Drop.
    Ok(())
}
