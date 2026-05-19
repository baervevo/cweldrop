use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap},
    Frame,
};

use crate::app::{App, Focus, PeerStatus};

pub fn draw(f: &mut Frame, app: &App) {
    let area = f.area();
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(1)])
        .split(area);

    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(28), Constraint::Min(20)])
        .split(outer[0]);

    draw_peers(f, app, cols[0]);
    draw_right(f, app, cols[1]);
    draw_status(f, app, outer[1]);
}

fn draw_peers(f: &mut Frame, app: &App, area: Rect) {
    let items: Vec<ListItem> = app
        .peers
        .iter()
        .map(|p| {
            let (sym, color) = match p.status {
                PeerStatus::Available => ("○", Color::Cyan),
                PeerStatus::Connecting => ("◔", Color::Yellow),
                PeerStatus::Online => ("●", Color::Green),
                PeerStatus::Offline => ("∅", Color::DarkGray),
                PeerStatus::Error => ("✗", Color::Red),
            };
            let line = Line::from(vec![
                Span::styled(format!("{sym} "), Style::default().fg(color)),
                Span::raw(p.username.clone()),
            ]);
            ListItem::new(line)
        })
        .collect();

    let border = peer_border_style(app);
    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("peers")
                .border_style(border),
        )
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));

    let mut state = ListState::default();
    if !app.peers.is_empty() {
        state.select(Some(app.selected.min(app.peers.len() - 1)));
    }
    f.render_stateful_widget(list, area, &mut state);
}

fn peer_border_style(app: &App) -> Style {
    if app.focus == Focus::Peers {
        Style::default().fg(Color::Magenta)
    } else {
        Style::default().fg(Color::DarkGray)
    }
}

fn input_border_style(app: &App) -> Style {
    if app.focus == Focus::Input {
        Style::default().fg(Color::Magenta)
    } else {
        Style::default().fg(Color::DarkGray)
    }
}

fn draw_right(f: &mut Frame, app: &App, area: Rect) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(3)])
        .split(area);

    let (title, lines) = match app.selected_peer() {
        Some(p) => {
            let history = app
                .history
                .get(p.history_key())
                .map(|h| h.as_slice())
                .unwrap_or(&[]);
            let lines: Vec<Line> = history
                .iter()
                .map(|cl| {
                    let is_me = cl.from == app.self_username;
                    let color = if is_me {
                        Color::LightBlue
                    } else {
                        Color::LightGreen
                    };
                    Line::from(vec![
                        Span::styled(
                            format!("[{}] ", format_hms(cl.at)),
                            Style::default().fg(Color::DarkGray),
                        ),
                        Span::styled(
                            format!("{}: ", cl.from),
                            Style::default().fg(color).add_modifier(Modifier::BOLD),
                        ),
                        Span::raw(cl.body.clone()),
                    ])
                })
                .collect();
            let title = match p.status {
                PeerStatus::Online => format!("chat with {} [online]", p.username),
                PeerStatus::Connecting => format!("chat with {} [connecting…]", p.username),
                PeerStatus::Available => {
                    format!("chat with {} [press Enter to connect]", p.username)
                }
                PeerStatus::Offline => format!("chat with {} [offline]", p.username),
                PeerStatus::Error => format!(
                    "chat with {} [error: {}]",
                    p.username,
                    p.last_error.as_deref().unwrap_or("?")
                ),
            };
            (title, lines)
        }
        None => (
            "no peer selected".to_string(),
            vec![Line::from(Span::styled(
                "Tip: wait for mDNS discovery or use :c <ip>:<port>",
                Style::default().fg(Color::DarkGray),
            ))],
        ),
    };

    let chat = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title(title))
        .wrap(Wrap { trim: false });
    f.render_widget(chat, rows[0]);

    let (input_title, content) = if app.focus == Focus::Command {
        (
            "command (Enter=run, Esc=cancel)".to_string(),
            format!(":{}", app.command),
        )
    } else {
        (
            "input (Enter=send, : =command, Tab=switch)".to_string(),
            format!("> {}", app.input),
        )
    };
    let input = Paragraph::new(content).block(
        Block::default()
            .borders(Borders::ALL)
            .title(input_title)
            .border_style(input_border_style(app)),
    );
    f.render_widget(input, rows[1]);
}

fn format_hms(t: std::time::SystemTime) -> String {
    let secs = t
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let h = (secs / 3600) % 24;
    let m = (secs / 60) % 60;
    let s = secs % 60;
    format!("{h:02}:{m:02}:{s:02}")
}

fn draw_status(f: &mut Frame, app: &App, area: Rect) {
    let online = app
        .peers
        .iter()
        .filter(|p| matches!(p.status, PeerStatus::Online))
        .count();
    let total = app.peers.len();
    let base = format!(
        "{} | port {} | peers: {total} ({online} online) | Tab switch, : command, Ctrl-C quit",
        app.self_username, app.self_port
    );
    let line = if app.status_msg.is_empty() {
        base
    } else {
        format!("{base} — {}", app.status_msg)
    };
    let para = Paragraph::new(line).style(Style::default().fg(Color::DarkGray));
    f.render_widget(para, area);
}
