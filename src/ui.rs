use chrono::{DateTime, Local};
use ratatui::{
    buffer::Buffer,
    layout::{Constraint, Direction, Layout, Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap},
    Frame,
};

use crate::app::{App, Focus, LinkRegion, PeerStatus};
use crate::links;

/// Style for links in chat. `link_regions` keys off it, so nothing else in
/// the chat pane may use this exact style.
const LINK_FG: Color = Color::LightCyan;

fn link_style() -> Style {
    Style::default()
        .fg(LINK_FG)
        .add_modifier(Modifier::UNDERLINED)
}

pub fn draw(f: &mut Frame, app: &mut App) {
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

fn char_offset(s: &str, byte_cursor: usize) -> u16 {
    let upto = byte_cursor.min(s.len());
    s[..upto].chars().count() as u16
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

fn draw_right(f: &mut Frame, app: &mut App, area: Rect) {
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
            let mut lines: Vec<Line> = history
                .iter()
                .map(|cl| {
                    let is_me = cl.from == app.self_username;
                    let color = if is_me {
                        Color::LightBlue
                    } else {
                        Color::LightGreen
                    };
                    let mut spans = vec![
                        Span::styled(
                            format!("[{}] ", format_timestamp(cl.at)),
                            Style::default().fg(Color::DarkGray),
                        ),
                        Span::styled(
                            format!("{}: ", cl.from),
                            Style::default().fg(color).add_modifier(Modifier::BOLD),
                        ),
                    ];
                    spans.extend(body_spans(&cl.body));
                    Line::from(spans)
                })
                .collect();
            if p.is_typing() {
                lines.push(typing_line(&p.username));
            }
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

    let chat_area = rows[0];
    let chat_block = Block::default().borders(Borders::ALL).title(title);
    let inner = chat_block.inner(chat_area);
    let chat = Paragraph::new(lines)
        .block(chat_block)
        .wrap(Wrap { trim: false });

    // Clamp scroll to the wrapped content height so PageUp can't go past the
    // first line and PageDown can't pull a blank gap below the newest line.
    let total = chat.line_count(inner.width) as u16;
    let visible = inner.height;
    let max_scroll = total.saturating_sub(visible);
    if app.chat_scroll > max_scroll {
        app.chat_scroll = max_scroll;
    }
    let scroll_from_top = max_scroll - app.chat_scroll;
    let chat = chat.scroll((scroll_from_top, 0));
    f.render_widget(chat, chat_area);
    let urls = app.selected_chat_urls();
    app.link_regions = link_regions(f.buffer_mut(), inner, &urls);

    let input_area = rows[1];
    let (input_title, content, prefix_len, cursor_chars, is_active) = if app.focus == Focus::Command
    {
        (
            "command (Enter=run, Esc=cancel)".to_string(),
            format!(":{}", app.command),
            1u16,
            char_offset(&app.command, app.command_cursor),
            true,
        )
    } else {
        (
            "input (Enter=send, : =command, Tab=switch)".to_string(),
            format!("> {}", app.input),
            2u16,
            char_offset(&app.input, app.input_cursor),
            app.focus == Focus::Input,
        )
    };
    let input_block = Block::default()
        .borders(Borders::ALL)
        .title(input_title)
        .border_style(input_border_style(app));
    let input_inner = input_block.inner(input_area);

    // Horizontal scroll so the cursor stays inside the visible window when the
    // line is longer than the box.
    let cursor_col = prefix_len + cursor_chars;
    let scroll_x = cursor_col.saturating_sub(input_inner.width.saturating_sub(1));
    let input = Paragraph::new(content)
        .block(input_block)
        .scroll((0, scroll_x));
    f.render_widget(input, input_area);

    if is_active && input_inner.width > 0 && input_inner.height > 0 {
        let visible_col = cursor_col - scroll_x;
        let x = input_inner.x + visible_col.min(input_inner.width - 1);
        let y = input_inner.y;
        f.set_cursor_position(Position { x, y });
    }
}

/// Split a message body into plain spans and styled link spans.
fn body_spans(body: &str) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut last = 0;
    for r in links::find_urls(body) {
        if r.start > last {
            spans.push(Span::raw(body[last..r.start].to_string()));
        }
        spans.push(Span::styled(body[r.clone()].to_string(), link_style()));
        last = r.end;
    }
    if last < body.len() {
        spans.push(Span::raw(body[last..].to_string()));
    }
    spans
}

fn is_link_cell(buf: &Buffer, x: u16, y: u16) -> bool {
    let c = &buf[(x, y)];
    c.fg == LINK_FG && c.modifier.contains(Modifier::UNDERLINED)
}

/// Find the on-screen cells of every link rendered inside `area` and resolve
/// each to its full URL. A link the wrapper broke across rows is stitched
/// back together: a run that reaches the right edge continues at the start of
/// the next row. Each fragment is matched against `urls` (all links in the
/// chat) so a link partially scrolled out of view still opens in full.
fn link_regions(buf: &Buffer, area: Rect, urls: &[String]) -> Vec<LinkRegion> {
    /// One stitched link: `(y, x_start, x_end)` per row, plus its visible text.
    type Run = (Vec<(u16, u16, u16)>, String);
    let mut runs: Vec<Run> = Vec::new();
    let mut prev: Option<(u16, u16)> = None; // (y, x_end) of the last link cell
    for y in area.top()..area.bottom() {
        let mut x = area.left();
        while x < area.right() {
            if !is_link_cell(buf, x, y) {
                x += 1;
                continue;
            }
            let sym = buf[(x, y)].symbol().to_string();
            let w = (unicode_width::UnicodeWidthStr::width(sym.as_str()) as u16).max(1);
            let x_end = (x + w).min(area.right());
            let continues = match prev {
                Some((py, pend)) => {
                    (py == y && pend == x)
                        || (py + 1 == y && pend == area.right() && x == area.left())
                }
                None => false,
            };
            if !continues || runs.is_empty() {
                runs.push((Vec::new(), String::new()));
            }
            let (cells, text) = runs.last_mut().expect("just pushed");
            match cells.last_mut() {
                Some(seg) if seg.0 == y && seg.2 == x => seg.2 = x_end,
                _ => cells.push((y, x, x_end)),
            }
            text.push_str(&sym);
            prev = Some((y, x_end));
            x = x_end;
        }
    }

    let mut out = Vec::new();
    for (cells, text) in runs {
        let url = urls
            .iter()
            .find(|u| u.as_str() == text || u.strip_prefix("https://") == Some(text.as_str()))
            .or_else(|| urls.iter().find(|u| u.contains(text.as_str())))
            .cloned()
            .or_else(|| links::normalize(&text));
        let Some(url) = url else { continue };
        out.extend(cells.into_iter().map(|(y, x_start, x_end)| LinkRegion {
            y,
            x_start,
            x_end,
            url: url.clone(),
        }));
    }
    out
}

/// A dim, italic "<name> is typing…" line with dots that animate across
/// redraws (the UI ticks at least twice a second).
fn typing_line(username: &str) -> Line<'static> {
    let phase = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
        / 400
        % 3;
    let dots = ".".repeat(phase as usize + 1);
    Line::from(Span::styled(
        format!("{username} is typing{dots}"),
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::ITALIC),
    ))
}

/// Format a message timestamp in the viewer's local timezone. Messages from
/// today show only the time; older ones are prefixed with the date.
fn format_timestamp(t: std::time::SystemTime) -> String {
    let dt: DateTime<Local> = t.into();
    if dt.date_naive() == Local::now().date_naive() {
        dt.format("%H:%M:%S").to_string()
    } else {
        dt.format("%Y-%m-%d %H:%M:%S").to_string()
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{ChatLine, Peer};
    use ratatui::{backend::TestBackend, Terminal};

    fn app_with_messages(bodies: &[&str]) -> App {
        let mut app = App::new("me".into(), 7421);
        app.peers.push(Peer {
            id: "p".into(),
            username: "them".into(),
            addr: None,
            status: PeerStatus::Online,
            last_error: None,
            pubkey: None,
            typing_at: None,
        });
        app.history.insert(
            "p".into(),
            bodies
                .iter()
                .map(|b| ChatLine {
                    from: "them".into(),
                    body: (*b).into(),
                    at: std::time::SystemTime::now(),
                })
                .collect(),
        );
        app
    }

    fn render(app: &mut App, w: u16, h: u16) {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| draw(f, app)).unwrap();
    }

    #[test]
    fn body_spans_style_only_links() {
        let spans = body_spans("a https://x.io b");
        assert_eq!(spans.len(), 3);
        assert_eq!(spans[1].content, "https://x.io");
        assert_eq!(spans[1].style, link_style());
        assert_ne!(spans[0].style, link_style());
    }

    #[test]
    fn link_on_one_row_is_clickable() {
        let mut app = app_with_messages(&["see https://example.com now"]);
        render(&mut app, 100, 10);
        assert!(!app.link_regions.is_empty());
        let r = app.link_regions[0].clone();
        assert_eq!(r.url, "https://example.com");
        assert_eq!(r.x_end - r.x_start, "https://example.com".len() as u16);
        app.click(r.x_start, r.y);
        assert_eq!(app.pending_open.as_deref(), Some("https://example.com"));
    }

    #[test]
    fn wrapped_link_resolves_to_full_url_on_every_row() {
        let url = "https://example.com/a/very/long/path/that/must/wrap/across/several/rows";
        let mut app = app_with_messages(&[&format!("x {url}")]);
        // Chat pane is 30 wide minus borders: the URL spans 3+ rows.
        render(&mut app, 58, 12);
        let rows: std::collections::BTreeSet<u16> = app.link_regions.iter().map(|r| r.y).collect();
        assert!(rows.len() >= 3, "regions: {:?}", app.link_regions);
        assert!(app.link_regions.iter().all(|r| r.url == url));
    }

    #[test]
    fn separate_links_stay_separate() {
        let mut app = app_with_messages(&["https://a.com https://b.com", "www.c.com"]);
        render(&mut app, 100, 10);
        let urls: Vec<&str> = app.link_regions.iter().map(|r| r.url.as_str()).collect();
        assert_eq!(
            urls,
            vec!["https://a.com", "https://b.com", "https://www.c.com"]
        );
    }
}
