use crossterm::ExecutableCommand;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use pj::tools::{self, ToolCall};
use pj::*;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use std::io::{self, Write, stdout};
use tokio::sync::mpsc;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

fn strip_code_blocks(text: &str) -> String {
    let mut result = String::new();
    let mut pos = 0;
    while pos < text.len() {
        let remaining = &text[pos..];
        if let Some(start) = remaining.find("```") {
            result.push_str(&text[pos..pos + start]);
            let content_start = pos + start + 3;
            let rest = &text[content_start..];
            let line_end = rest.find('\n').unwrap_or(rest.len());
            let code_start = content_start + line_end + 1;
            if let Some(end) = text[code_start..].find("```") {
                result.push_str(&text[code_start..code_start + end]);
                pos = code_start + end + 3;
            } else {
                result.push_str(&text[pos..]);
                break;
            }
        } else {
            result.push_str(&text[pos..]);
            break;
        }
    }
    result
}

fn truncate_display_width(text: &str, max_width: usize) -> String {
    let mut result = String::new();
    let mut width = 0;
    for ch in text.chars() {
        let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if width + ch_width > max_width {
            break;
        }
        result.push(ch);
        width += ch_width;
    }
    result
}

#[derive(PartialEq)]
enum Focus {
    Sidebar,
    Input,
}

enum CliEvent {
    TextReady {
        chat_id: i64,
    },
    ToolCallsPending {
        chat_id: i64,
    },
    Error {
        chat_id: i64,
        message: String,
    },
}

struct App {
    chats: Vec<ChatSummary>,
    messages: Vec<MessageOut>,
    active_chat_id: Option<i64>,
    input: String,
    loading: bool,
    loading_chat_id: Option<i64>,
    sidebar_index: usize,
    focus: Focus,
    scroll: u16,
    confirmation_scroll: u16,
    exit: bool,
    pool: DbPool,
    ollama_url: String,
    model: String,
    tx: mpsc::UnboundedSender<CliEvent>,
    rx: mpsc::UnboundedReceiver<CliEvent>,
    pending_tool_calls: Option<(i64, Vec<ToolCall>)>,
    waiting_confirmation: bool,
    event_client: Option<EventClient>,
}

impl App {
    fn new(pool: DbPool) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let ollama_url = ollama_url();
        let model = model_name();
        let mut app = Self {
            chats: vec![],
            messages: vec![],
            active_chat_id: None,
            input: String::new(),
            loading: false,
            loading_chat_id: None,
            sidebar_index: 0,
            focus: Focus::Input,
            scroll: 0,
            confirmation_scroll: 0,
            exit: false,
            pool,
            ollama_url,
            model,
            tx,
            rx,
            pending_tool_calls: None,
            waiting_confirmation: false,
            event_client: EventClient::connect(&socket_path()),
        };
        app.load_chats();
        app
    }

    fn load_chats(&mut self) {
        self.chats = list_chats(&self.pool).unwrap_or_default();
        if !self.chats.is_empty() && self.sidebar_index >= self.chats.len() {
            self.sidebar_index = self.chats.len() - 1;
        }
    }

    fn load_messages(&mut self) {
        if let Some(id) = self.active_chat_id {
            self.messages = get_messages(&self.pool, id).unwrap_or_default();
        } else {
            self.messages = vec![];
        }
        self.scroll = 0;
    }

    fn refresh_pending_for_active_chat(&mut self) {
        let already_shown = matches!(
            self.pending_tool_calls,
            Some((chat_id, _)) if Some(chat_id) == self.active_chat_id
        );
        if !already_shown
            && let Some(chat_id) = self.active_chat_id
            && let Ok(Some(calls)) = load_pending_tools(&self.pool, chat_id)
        {
            self.pending_tool_calls = Some((chat_id, calls));
            self.confirmation_scroll = 0;
            self.waiting_confirmation = true;
        }
    }

    fn is_active_chat_loading(&self) -> bool {
        self.loading && self.loading_chat_id == self.active_chat_id
    }

    fn has_pending_confirmation_for_active_chat(&self) -> bool {
        matches!(
            self.pending_tool_calls,
            Some((chat_id, _)) if Some(chat_id) == self.active_chat_id
        )
    }

    fn confirmation_line_count(&self) -> usize {
        match &self.pending_tool_calls {
            Some((_, tool_calls)) => tool_calls
                .iter()
                .map(|tc| tools::tool_call_description(tc).lines().count() + 1)
                .sum(),
            None => 0,
        }
    }

    fn select_chat(&mut self, index: usize) {
        if index < self.chats.len() {
            self.active_chat_id = Some(self.chats[index].id);
            self.load_messages();
            self.refresh_pending_for_active_chat();
            self.focus = Focus::Input;
        }
    }

    fn new_chat(&mut self) {
        if let Ok(chat) = create_chat(&self.pool) {
            let _ = publish_chat_change(&ChatChange::Upsert { id: chat.id });
            self.load_chats();
            self.sidebar_index = 0;
            self.active_chat_id = Some(chat.id);
            self.messages = vec![];
            self.input.clear();
            self.focus = Focus::Input;
            self.scroll = 0;
        }
    }

    fn send_message(&mut self) {
        let message = self.input.trim().to_string();
        if message.is_empty() || self.loading {
            return;
        }
        self.input.clear();

        let chat_id = match self.active_chat_id {
            Some(id) => id,
            None => {
                self.new_chat();
                match self.active_chat_id {
                    Some(id) => id,
                    None => return,
                }
            }
        };

        let _ = update_title_from_message(&self.pool, chat_id, &message);
        let _ = add_message(&self.pool, chat_id, "user", &message, None);
        let _ = publish_chat_change(&ChatChange::Upsert { id: chat_id });
        self.load_chats();
        self.load_messages();

        self.start_turn();
    }

    fn start_turn(&mut self) {
        self.loading = true;
        self.loading_chat_id = self.active_chat_id;
        let chat_id = match self.loading_chat_id {
            Some(id) => id,
            None => return,
        };
        let _ = publish_chat_change(&ChatChange::Activity {
            id: chat_id,
            state: ChatActivityState::Thinking,
        });
        let pool = self.pool.clone();
        let ollama_url = self.ollama_url.clone();
        let model = self.model.clone();
        let tx = self.tx.clone();

        tokio::spawn(async move {
            let outcome = run_chat_turn(&pool, &ollama_url, &model, chat_id).await;
            report_outcome(outcome, chat_id, &tx, &pool).await;
        });
    }

    fn confirm_tool(&mut self) {
        let (chat_id, _) = match self.pending_tool_calls.take() {
            Some(v) => v,
            None => return,
        };
        self.waiting_confirmation = false;
        self.resolve(chat_id, true);
    }

    fn deny_tool(&mut self) {
        let (chat_id, _) = match self.pending_tool_calls.take() {
            Some(v) => v,
            None => return,
        };
        self.waiting_confirmation = false;
        self.resolve(chat_id, false);
    }

    fn resolve(&mut self, chat_id: i64, approved: bool) {
        self.loading = true;
        self.loading_chat_id = Some(chat_id);
        let _ = publish_chat_change(&ChatChange::Activity {
            id: chat_id,
            state: ChatActivityState::Thinking,
        });

        let pool = self.pool.clone();
        let ollama_url = self.ollama_url.clone();
        let model = self.model.clone();
        let tx = self.tx.clone();

        tokio::spawn(async move {
            let outcome =
                resolve_pending_tools(&pool, &ollama_url, &model, chat_id, approved).await;
            report_outcome(outcome, chat_id, &tx, &pool).await;
        });
    }
}

async fn report_outcome(
    outcome: Result<TurnOutcome, String>,
    chat_id: i64,
    tx: &mpsc::UnboundedSender<CliEvent>,
    pool: &DbPool,
) {
    match outcome {
        Ok(TurnOutcome::Reply(_)) => {
            let _ = publish_chat_change(&ChatChange::Activity {
                id: chat_id,
                state: ChatActivityState::Idle,
            });
            let _ = tx.send(CliEvent::TextReady { chat_id });
        }
        Ok(TurnOutcome::PendingTools(_)) => {
            let _ = tx.send(CliEvent::ToolCallsPending { chat_id });
        }
        Err(e) => {
            let _ = add_message(pool, chat_id, "assistant", &format!("Error: {e}"), None);
            let _ = publish_chat_change(&ChatChange::Upsert { id: chat_id });
            let _ = publish_chat_change(&ChatChange::Activity {
                id: chat_id,
                state: ChatActivityState::Idle,
            });
            let _ = tx.send(CliEvent::Error { chat_id, message: e });
        }
    }
}

fn init_terminal() -> io::Result<Terminal<impl ratatui::backend::Backend>> {
    enable_raw_mode()?;
    stdout().execute(EnterAlternateScreen)?;
    let backend = ratatui::backend::CrosstermBackend::new(stdout());
    Terminal::new(backend)
}

fn restore_terminal() -> io::Result<()> {
    disable_raw_mode()?;
    stdout().execute(LeaveAlternateScreen)?;
    Ok(())
}

fn draw_sidebar(frame: &mut Frame, area: Rect, app: &App) {
    let constraints = vec![Constraint::Min(1), Constraint::Length(1)];
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area);

    let items: Vec<ListItem> = app
        .chats
        .iter()
        .enumerate()
        .map(|(i, chat)| {
            let prefix = if Some(chat.id) == app.active_chat_id {
                " \u{25b6} "
            } else {
                "   "
            };
            let truncated_title = truncate_display_width(&chat.title, 22);
            let label = if UnicodeWidthStr::width(chat.title.as_str()) > 22 {
                format!("{}{}", prefix, truncated_title)
            } else {
                format!("{}{}", prefix, chat.title)
            };
            let style = if i == app.sidebar_index && matches!(app.focus, Focus::Sidebar) {
                Style::default().bg(Color::DarkGray)
            } else if Some(chat.id) == app.active_chat_id {
                Style::default().fg(Color::Cyan)
            } else {
                Style::default()
            };
            ListItem::new(label).style(style)
        })
        .collect();

    let list = List::new(items)
        .block(Block::default().title(" Chats ").borders(Borders::ALL))
        .highlight_style(Style::default().bg(Color::DarkGray));

    let mut state = ratatui::widgets::ListState::default().with_selected(Some(app.sidebar_index));
    frame.render_stateful_widget(list, chunks[0], &mut state);

    let help_text = if matches!(app.focus, Focus::Sidebar) {
        " [n] new  [d] del  [Tab] focus  [q] quit "
    } else {
        " [Tab] focus  [q] quit "
    };
    let help = Paragraph::new(Line::from(Span::styled(
        help_text,
        Style::default().fg(Color::DarkGray),
    )));
    frame.render_widget(help, chunks[1]);
}

fn draw_confirmation_panel(frame: &mut Frame, area: Rect, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(2)])
        .split(area);

    let mut lines = vec![Line::from(Span::styled(
        "AI wants to use tools",
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    ))];

    if let Some((_, tool_calls)) = &app.pending_tool_calls {
        for tc in tool_calls {
            lines.push(Line::from(""));
            for line in tools::tool_call_description(tc).lines() {
                lines.push(Line::from(Span::styled(
                    format!("  {line}"),
                    Style::default().fg(Color::Cyan),
                )));
            }
        }
    }

    let details = Paragraph::new(Text::from(lines))
        .block(
            Block::default()
                .title(" Tool Approval ")
                .borders(Borders::ALL),
        )
        .wrap(Wrap { trim: false })
        .scroll((app.confirmation_scroll, 0));
    frame.render_widget(details, chunks[0]);

    let actions = Paragraph::new(Line::from(vec![
        Span::styled(
            "[y] accept",
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(
            "[n] deny",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled("[↑/↓] scroll", Style::default().fg(Color::Yellow)),
    ]))
    .block(Block::default().borders(Borders::LEFT | Borders::RIGHT | Borders::BOTTOM));
    frame.render_widget(actions, chunks[1]);
}

fn draw_main(frame: &mut Frame, area: Rect, app: &App) {
    let waiting_confirmation = app.has_pending_confirmation_for_active_chat();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(if waiting_confirmation {
            [Constraint::Min(1), Constraint::Length(10)]
        } else {
            [Constraint::Min(1), Constraint::Length(7)]
        })
        .split(area);

    let title = match app.active_chat_id {
        Some(id) => {
            let t = app
                .chats
                .iter()
                .find(|c| c.id == id)
                .map(|c| c.title.as_str())
                .unwrap_or("Chat");
            format!(" {t} ")
        }
        None => "".to_string(),
    };

    let mut lines: Vec<Line> = Vec::new();

    if app.messages.is_empty()
        && app.active_chat_id.is_some()
        && !app.is_active_chat_loading()
        && !waiting_confirmation
    {
        let empty = Paragraph::new("No messages yet. Type below to start chatting.")
            .style(Style::default().fg(Color::DarkGray))
            .block(Block::default().title(title).borders(Borders::ALL));
        frame.render_widget(empty, chunks[0]);
    } else if app.active_chat_id.is_some() {
        for msg in &app.messages {
            match msg.role.as_str() {
                "tool" => {
                    let name = msg.name.as_deref().unwrap_or("tool");
                    let first_line: String = msg
                        .content
                        .lines()
                        .next()
                        .unwrap_or("")
                        .chars()
                        .take(200)
                        .collect();
                    lines.push(Line::from(Span::styled(
                        format!("  [{name}] {first_line}"),
                        Style::default().fg(Color::DarkGray),
                    )));
                    continue;
                }
                role => {
                    let label = match role {
                        "user" => "You",
                        "assistant" => "AI",
                        other => other,
                    };
                    let color = match role {
                        "user" => Color::Green,
                        "assistant" => Color::Cyan,
                        _ => Color::White,
                    };
                    lines.push(Line::from(Span::styled(
                        format!("[{label}]"),
                        Style::default().fg(color).add_modifier(Modifier::BOLD),
                    )));
                    if let Some(calls) = &msg.tool_calls {
                        for tc in calls {
                            for line in tools::tool_call_description(tc).lines() {
                                lines.push(Line::from(Span::styled(
                                    format!("  {line}"),
                                    Style::default().fg(Color::DarkGray),
                                )));
                            }
                        }
                    }
                    if !msg.content.is_empty() {
                        lines.push(Line::from(strip_code_blocks(&msg.content)));
                    }
                    lines.push(Line::from(""));
                }
            }
        }

        if app.is_active_chat_loading() {
            lines.push(Line::from(Span::styled(
                "[AI] Thinking...",
                Style::default().fg(Color::Yellow),
            )));
        }

        let messages = Paragraph::new(Text::from(lines))
            .block(Block::default().title(title).borders(Borders::ALL))
            .wrap(Wrap { trim: false })
            .scroll((app.scroll, 0));
        frame.render_widget(messages, chunks[0]);
    } else {
        let empty = Paragraph::new("Select a chat from the sidebar or press n to create one.")
            .style(Style::default().fg(Color::DarkGray))
            .block(Block::default().title(title).borders(Borders::ALL));
        frame.render_widget(empty, chunks[0]);
    }

    let input_style = if matches!(app.focus, Focus::Input) {
        Style::default().fg(Color::White)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    if waiting_confirmation {
        draw_confirmation_panel(frame, chunks[1], app);
    } else {
        let input_text: &str = if app.input.is_empty() {
            " Type a message..."
        } else {
            app.input.as_str()
        };
        let input = Paragraph::new(input_text)
            .style(input_style)
            .block(Block::default().title(" Input ").borders(Borders::ALL));
        frame.render_widget(input, chunks[1]);
    }

    if !waiting_confirmation
        && matches!(app.focus, Focus::Input)
        && chunks[1].width > 2
        && chunks[1].height > 2
    {
        let max_cursor_col = chunks[1].width.saturating_sub(2);
        let input_width = UnicodeWidthStr::width(app.input.as_str()) as u16;
        let cursor_col = (input_width + 1).min(max_cursor_col);
        frame.set_cursor_position((chunks[1].x + cursor_col, chunks[1].y + 1));
    }
}

fn draw_help(frame: &mut Frame, area: Rect) {
    let help_lines = vec![
        Line::from(""),
        Line::from(" pj — Key Bindings").bold(),
        Line::from(""),
        Line::from("  Tab          Switch focus (sidebar / input)"),
        Line::from("  Up / Down    Navigate chat list"),
        Line::from("  Enter        Select chat / Send message"),
        Line::from("  n            New chat"),
        Line::from("  d            Delete active chat"),
        Line::from("  q / Ctrl+c   Quit"),
        Line::from("  ?            Toggle this help"),
        Line::from(""),
        Line::from(" Press any key to close"),
    ];
    let help = Paragraph::new(Text::from(help_lines))
        .style(Style::default().fg(Color::White))
        .block(
            Block::default()
                .title(" Help ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan)),
        );
    frame.render_widget(help, area);
}

fn ui(frame: &mut Frame, app: &App, show_help: bool) {
    if show_help {
        let area = frame.area();
        let help_area = Rect {
            x: area.width / 6,
            y: area.height / 6,
            width: area.width * 2 / 3,
            height: area.height * 2 / 3,
        };
        draw_help(frame, help_area);
        return;
    }

    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(1)])
        .split(frame.area());

    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(30), Constraint::Min(1)])
        .split(outer[1]);

    draw_sidebar(frame, chunks[0], app);
    draw_main(frame, chunks[1], app);

    let top_bar = if app.loading {
        Paragraph::new(Line::from(vec![
            Span::raw(" "),
            Span::styled(
                " pj ",
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled(" Thinking... ", Style::default().fg(Color::Yellow)),
        ]))
    } else if app.waiting_confirmation {
        Paragraph::new(Line::from(vec![
            Span::raw(" "),
            Span::styled(
                " pj ",
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled(" Awaiting confirmation ", Style::default().fg(Color::Green)),
            Span::raw(" "),
            Span::styled(
                "[y] accept [n] deny [↑/↓] scroll",
                Style::default().fg(Color::Yellow),
            ),
        ]))
    } else {
        Paragraph::new(Line::from(vec![
            Span::raw(" "),
            Span::styled(
                " pj ",
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled(
                format!(" {} chats ", app.chats.len()),
                Style::default().fg(Color::DarkGray),
            ),
        ]))
    };
    frame.render_widget(top_bar, outer[0]);
}

fn run_tui(pool: DbPool) -> io::Result<()> {
    let mut terminal = init_terminal()?;
    let mut app = App::new(pool);
    let mut show_help = false;

    terminal.clear()?;

    while !app.exit {
        terminal.draw(|f| ui(f, &app, show_help))?;

        if let Ok(event) = app.rx.try_recv() {
            match event {
                CliEvent::TextReady { chat_id, .. } => {
                    app.loading = false;
                    app.loading_chat_id = None;
                    if app.active_chat_id == Some(chat_id) {
                        app.load_messages();
                    }
                }
                CliEvent::ToolCallsPending { chat_id } => {
                    app.loading = false;
                    app.loading_chat_id = None;
                    if app.active_chat_id == Some(chat_id) {
                        app.load_messages();
                        if let Ok(Some(calls)) = load_pending_tools(&app.pool, chat_id) {
                            app.pending_tool_calls = Some((chat_id, calls));
                            app.confirmation_scroll = 0;
                            app.waiting_confirmation = true;
                        }
                    }
                }
                CliEvent::Error { chat_id, message } => {
                    app.loading = false;
                    app.loading_chat_id = None;
                    let _ = add_message(
                        &app.pool,
                        chat_id,
                        "assistant",
                        &format!("Error: {message}"),
                        None,
                    );
                    let _ = publish_chat_change(&ChatChange::Upsert { id: chat_id });
                    let _ = publish_chat_change(&ChatChange::Activity {
                        id: chat_id,
                        state: ChatActivityState::Idle,
                    });
                    if app.active_chat_id == Some(chat_id) {
                        app.load_messages();
                    }
                }
            }
            app.waiting_confirmation = app.pending_tool_calls.is_some();
        }

        let mut socket_events = Vec::new();
        let sp = socket_path();
        if let Some(client) = app.event_client.as_mut() {
            if socket_inode(&sp) != Some(client.ino) {
                app.event_client = None;
            } else {
                loop {
                    match client.try_recv() {
                        Some(Some(change)) => socket_events.push(change),
                        Some(None) => break,
                        None => {
                            app.event_client = None;
                            break;
                        }
                    }
                }
            }
        }
        if app.event_client.is_none() {
            app.event_client = EventClient::connect(&sp);
            if app.event_client.is_some() {
                app.load_chats();
            }
        }
        if !socket_events.is_empty() {
            for ev in &socket_events {
                match ev {
                    ChatChange::Deleted { id } => {
                        if app.active_chat_id == Some(*id) {
                            app.active_chat_id = None;
                            app.messages = vec![];
                        }
                    }
                    ChatChange::Upsert { id } => {
                        if app.active_chat_id == Some(*id) && !app.loading {
                            app.load_messages();
                            app.refresh_pending_for_active_chat();
                        }
                    }
                    ChatChange::Activity { id, state } => {
                        if let ChatActivityState::Idle = state
                            && app.active_chat_id == Some(*id)
                        {
                            let still_pending = matches!(
                                app.pending_tool_calls,
                                Some((cid, _)) if cid == *id
                            );
                            if !still_pending {
                                app.waiting_confirmation = false;
                            }
                        }
                    }
                }
            }
            app.load_chats();
        }

        if event::poll(std::time::Duration::from_millis(100))?
            && let Event::Key(key) = event::read()?
        {
            if key.kind != KeyEventKind::Press {
                continue;
            }

            if show_help {
                show_help = false;
                continue;
            }

            match key.code {
                KeyCode::Esc => {
                    app.exit = true;
                }
                KeyCode::Char('q') if matches!(app.focus, Focus::Sidebar) => {
                    app.exit = true;
                }
                KeyCode::Char('c') if key.modifiers == KeyModifiers::CONTROL => {
                    app.exit = true;
                }
                KeyCode::Char('?') if app.focus == Focus::Sidebar => {
                    show_help = !show_help;
                }
                _ => {}
            }

            if show_help {
                continue;
            }

            if app.has_pending_confirmation_for_active_chat() {
                match key.code {
                    KeyCode::Char('y') | KeyCode::Char('Y') => {
                        app.confirm_tool();
                    }
                    KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                        app.deny_tool();
                    }
                    KeyCode::Up => {
                        app.confirmation_scroll = app.confirmation_scroll.saturating_sub(1);
                    }
                    KeyCode::Down => {
                        let max_scroll = app.confirmation_line_count().saturating_sub(1) as u16;
                        app.confirmation_scroll =
                            app.confirmation_scroll.saturating_add(1).min(max_scroll);
                    }
                    _ => {}
                }
                continue;
            }

            match app.focus {
                Focus::Sidebar => handle_sidebar_key(&mut app, key),
                Focus::Input => handle_input_key(&mut app, key),
            }
        }
    }

    restore_terminal()?;
    Ok(())
}

fn handle_sidebar_key(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Up | KeyCode::Char('k') if app.sidebar_index > 0 => {
            app.sidebar_index -= 1;
        }
        KeyCode::Down | KeyCode::Char('j') if app.sidebar_index + 1 < app.chats.len() => {
            app.sidebar_index += 1;
        }
        KeyCode::Enter => app.select_chat(app.sidebar_index),
        KeyCode::Char('n') => app.new_chat(),
        KeyCode::Char('d') if !app.chats.is_empty() && app.chats.len() > app.sidebar_index => {
            let id = app.chats[app.sidebar_index].id;
            let _ = delete_chat(&app.pool, id);
            let _ = publish_chat_change(&ChatChange::Deleted { id });
            if app.active_chat_id == Some(id) {
                app.active_chat_id = None;
                app.messages = vec![];
            }
            app.load_chats();
        }
        KeyCode::Tab => app.focus = Focus::Input,
        _ => {}
    }
}

fn handle_input_key(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Enter => {
            if app.input.trim() == "/exit" {
                app.input.clear();
                app.exit = true;
            } else {
                app.send_message();
            }
        }
        KeyCode::Tab => app.focus = Focus::Sidebar,
        KeyCode::Char(c) => app.input.push(c),
        KeyCode::Backspace => {
            app.input.pop();
        }
        _ => {}
    }
}

async fn run_one_shot(pool: &DbPool, question: &str) {
    let ollama_url = ollama_url();
    let model = model_name();

    let chat = match create_chat(pool) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Error: {e}");
            return;
        }
    };
    let _ = publish_chat_change(&ChatChange::Upsert { id: chat.id });

    let _ = update_title_from_message(pool, chat.id, question);
    let _ = add_message(pool, chat.id, "user", question, None);
    let _ = publish_chat_change(&ChatChange::Upsert { id: chat.id });

    println!("You: {question}");
    print!("AI: ");
    io::stdout().flush().ok();
    run_cli_turn(pool, chat.id, &ollama_url, &model).await;
}

async fn run_cli_turn(pool: &DbPool, chat_id: i64, ollama_url: &str, model: &str) {
    let mut turn = run_chat_turn(pool, ollama_url, model, chat_id).await;
    loop {
        match turn {
            Ok(TurnOutcome::Reply(text)) => {
                if !text.trim().is_empty() {
                    println!("{}", strip_code_blocks(&text));
                }
                return;
            }
            Ok(TurnOutcome::PendingTools(calls)) => {
                println!();
                for tc in &calls {
                    println!("  {}", tools::tool_call_description(tc));
                }
                print!("\nExecute? [y/N] ");
                io::stdout().flush().ok();
                let mut input = String::new();
                io::stdin().read_line(&mut input).ok();
                let approved = input.trim().eq_ignore_ascii_case("y");
                turn = resolve_pending_tools(pool, ollama_url, model, chat_id, approved).await;
            }
            Err(e) => {
                eprintln!("\nError: {e}");
                return;
            }
        }
    }
}

async fn run_plain_loop(pool: &DbPool) {
    let ollama_url = ollama_url();
    let model = model_name();

    let chat = match create_chat(pool) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Error: {e}");
            return;
        }
    };
    let _ = publish_chat_change(&ChatChange::Upsert { id: chat.id });

    println!("pj plain mode. Type /exit to quit.\n");

    loop {
        print!("You: ");
        io::stdout().flush().ok();

        let mut input = String::new();
        if io::stdin().read_line(&mut input).is_err() {
            eprintln!("\nError: failed to read input");
            break;
        }

        let question = input.trim().to_string();
        if question.is_empty() {
            continue;
        }
        if question == "/exit" {
            break;
        }

        let _ = update_title_from_message(pool, chat.id, &question);
        let _ = add_message(pool, chat.id, "user", &question, None);
        let _ = publish_chat_change(&ChatChange::Upsert { id: chat.id });

        print!("AI: ");
        io::stdout().flush().ok();
        run_cli_turn(pool, chat.id, &ollama_url, &model).await;
        println!();
    }
}

#[tokio::main]
async fn main() {
    let database_url = database_url();
    let pool = create_pool(&database_url);
    init_db(&pool);

    let args: Vec<String> = std::env::args().collect();
    if args.len() > 1 && args[1] == "--plain" {
        run_plain_loop(&pool).await;
    } else if args.len() > 1 && args[1] == "--tui" {
        if let Err(e) = run_tui(pool) {
            eprintln!("TUI error: {e}");
            let _ = restore_terminal();
        }
    } else if args.len() > 1 {
        let question = args[1..].join(" ");
        run_one_shot(&pool, &question).await;
    } else {
        if let Err(e) = run_tui(pool) {
            eprintln!("TUI error: {e}");
            let _ = restore_terminal();
        }
    }
}
