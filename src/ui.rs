use chrono::{DateTime, Local};
use ratatui::{
    buffer::Buffer,
    layout::{Constraint, Direction, Layout, Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
    Frame,
};

use crate::app::{
    App, ChatLine, Delivery, FileInfo, FileState, Focus, LinkRegion, LinkTarget, Peer, PeerStatus,
};
use crate::files;
use crate::links;
use crate::preview::Content;

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
    if app.focus == Focus::Settings {
        draw_settings(f, app, area);
    }
    if app.viewer.is_some() {
        draw_viewer(f, app, outer[0]);
    }
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
            ListItem::new(Line::from(peer_label(p, sym, color)))
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
                    match &cl.file {
                        Some(file) => spans.extend(file_spans(file, is_me)),
                        None => spans.extend(body_spans(&cl.body)),
                    }
                    if let Some(mark) = &cl.chain {
                        spans.extend(delivery_span(mark.delivery));
                    }
                    Line::from(spans)
                })
                .collect();
            if p.is_typing() {
                lines.push(typing_line(&p.username));
            }
            let ident = identity_note(p);
            let title = match p.status {
                PeerStatus::Online => format!("chat with {} [online]{ident}", p.username),
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
    let targets = app
        .selected_peer()
        .and_then(|p| app.history.get(p.history_key()))
        .map(|h| link_targets(h))
        .unwrap_or_default();
    app.link_regions = link_regions(f.buffer_mut(), inner, &targets);

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
fn link_regions(buf: &Buffer, area: Rect, targets: &[(String, LinkTarget)]) -> Vec<LinkRegion> {
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
        let target = targets
            .iter()
            .find(|(shown, _)| *shown == text)
            .or_else(|| {
                targets
                    .iter()
                    .find(|(shown, _)| shown.contains(text.as_str()))
            })
            .map(|(_, t)| t.clone())
            .or_else(|| links::normalize(&text).map(LinkTarget::Url));
        let Some(target) = target else { continue };
        out.extend(cells.into_iter().map(|(y, x_start, x_end)| LinkRegion {
            y,
            x_start,
            x_end,
            target: target.clone(),
        }));
    }
    out
}

/// Trailing marker for our own chain messages.
fn delivery_span(d: Delivery) -> Option<Span<'static>> {
    let dim = Style::default().fg(Color::DarkGray);
    match d {
        Delivery::Received => None,
        Delivery::Queued => Some(Span::styled(" ⏳", dim)),
        Delivery::Sent => Some(Span::styled(" ✓", dim)),
        Delivery::Acked => Some(Span::styled(" ✓✓", Style::default().fg(Color::Green))),
    }
}

/// Every clickable thing in a chat as (text exactly as rendered, target).
fn link_targets(history: &[ChatLine]) -> Vec<(String, LinkTarget)> {
    let mut out = Vec::new();
    for cl in history {
        match &cl.file {
            Some(f) => {
                if let Some(path) = viewable(f) {
                    out.push((f.name.clone(), LinkTarget::File(path.clone())));
                }
            }
            None => {
                for r in links::find_urls(&cl.body) {
                    if let Some(url) = links::normalize(&cl.body[r.clone()]) {
                        out.push((cl.body[r].to_string(), LinkTarget::Url(url)));
                    }
                }
            }
        }
    }
    out
}

/// Path to open in the viewer, if this transfer finished.
fn viewable(f: &FileInfo) -> Option<&std::path::PathBuf> {
    f.path.as_ref().filter(|_| f.state == FileState::Done)
}

/// "📎 name (size) — state" for a file transfer line.
fn file_spans(f: &FileInfo, is_me: bool) -> Vec<Span<'static>> {
    let dim = Style::default().fg(Color::DarkGray);
    let icon = if files::is_image(&f.name) {
        "🖼 "
    } else {
        "📎 "
    };
    let name_style = if viewable(f).is_some() {
        link_style()
    } else {
        Style::default().add_modifier(Modifier::BOLD)
    };
    let pct = |b: u64| (b * 100).checked_div(f.size).unwrap_or(100);
    let (state, style) = match &f.state {
        FileState::Offered => ("waiting for them to accept…".to_string(), dim),
        FileState::AwaitingDecision => (
            ":accept or :reject".to_string(),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        FileState::Transferring(b) => (format!("{}%", pct(*b)), Style::default().fg(Color::Yellow)),
        FileState::Done => match &f.path {
            Some(_) if is_me => ("sent · click or :view to open".to_string(), dim),
            Some(p) if files::is_image(&f.name) => (
                format!("click or :view to open · saved to {}", p.display()),
                dim,
            ),
            Some(p) => (
                format!("click or :view to preview · saved to {}", p.display()),
                dim,
            ),
            None => ("received".to_string(), dim),
        },
        FileState::Rejected => ("declined".to_string(), dim),
        FileState::Failed(e) => (format!("failed: {e}"), Style::default().fg(Color::Red)),
    };
    vec![
        Span::raw(icon),
        Span::styled(f.name.clone(), name_style),
        Span::styled(format!(" ({}) — ", files::human_size(f.size)), dim),
        Span::styled(state, style),
    ]
}

/// Peer list entry: status dot, badges, name and a short key fingerprint so
/// two people with the same nickname can be told apart.
fn peer_label(p: &Peer, sym: &str, color: Color) -> Vec<Span<'static>> {
    let dim = Style::default().fg(Color::DarkGray);
    let mut spans = vec![Span::styled(format!("{sym} "), Style::default().fg(color))];
    if p.warning.is_some() {
        spans.push(Span::styled(
            "⚠ ",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ));
    } else if p.verified {
        spans.push(Span::styled("✔ ", Style::default().fg(Color::Green)));
    }
    let name_style = if p.warning.is_some() {
        Style::default().fg(Color::Red)
    } else {
        Style::default()
    };
    spans.push(Span::styled(p.username.clone(), name_style));
    match (p.pubkey.as_deref(), p.fingerprint()) {
        (Some(_), Some(fp)) => spans.push(Span::styled(format!(" {fp}"), dim)),
        (None, Some(fp)) => spans.push(Span::styled(format!(" {fp}?"), dim)),
        (None, None) if p.status == PeerStatus::Online => spans.push(Span::styled(
            " unverified",
            Style::default().fg(Color::Yellow),
        )),
        _ => {}
    }
    spans
}

/// Identity part of the chat title.
fn identity_note(p: &Peer) -> String {
    match (&p.warning, &p.pubkey) {
        (Some(w), _) => format!(" ⚠ {w}"),
        (None, Some(k)) if p.verified => format!(" · key {} ✔ verified", &k[..8.min(k.len())]),
        (None, Some(k)) => format!(" · key {} (:verify)", &k[..8.min(k.len())]),
        (None, None) if p.status == PeerStatus::Online => {
            " · ⚠ unverified: old client, identity not proven".to_string()
        }
        _ => String::new(),
    }
}

/// A `w`×`h` rect centred in `area` (clamped to fit).
fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    }
}

fn draw_settings(f: &mut Frame, app: &App, area: Rect) {
    let popup = centered(area, 72, 9);
    f.render_widget(Clear, popup);
    let s = &app.settings;
    let dir = match &app.settings_edit {
        Some(buf) => format!("{buf}▏"),
        None => s.download_dir.display().to_string(),
    };
    let rows = [
        (
            "Incoming files",
            format!("‹ {} ›", s.incoming_files.label()),
        ),
        ("Max file size", format!("‹ {} MB ›", s.max_file_mb)),
        ("Download folder", dir),
    ];
    let mut lines: Vec<Line> = rows
        .iter()
        .enumerate()
        .map(|(i, (label, value))| {
            let sel = i == app.settings_sel;
            let style = if sel {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                Style::default()
            };
            Line::from(vec![
                Span::styled(format!(" {label:<16} "), style.add_modifier(Modifier::BOLD)),
                Span::styled(format!(" {value} "), style),
            ])
        })
        .collect();
    lines.push(Line::raw(""));
    let hint = if app.settings_edit.is_some() {
        "type a path · Enter save · Esc cancel"
    } else {
        "↑/↓ select · ←/→/Enter change · Esc close (saved automatically)"
    };
    lines.push(Line::from(Span::styled(
        hint,
        Style::default().fg(Color::DarkGray),
    )));
    let block = Block::default()
        .borders(Borders::ALL)
        .title("settings")
        .border_style(Style::default().fg(Color::Magenta));
    f.render_widget(Paragraph::new(lines).block(block), popup);
}

fn draw_viewer(f: &mut Frame, app: &mut App, area: Rect) {
    let Some(v) = app.viewer.as_mut() else { return };
    f.render_widget(Clear, area);
    let hint = match v.content {
        Some(Content::Text(_)) => "↑/↓ PgUp/PgDn scroll · Esc to close",
        _ => "Esc to close",
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!("{} — {hint}", v.name))
        .border_style(Style::default().fg(Color::Magenta));
    let inner = block.inner(area);
    f.render_widget(block, area);
    match (&mut v.content, &v.error) {
        (_, Some(e)) => f.render_widget(
            Paragraph::new(format!("cannot show {}: {e}", v.name))
                .style(Style::default().fg(Color::Red))
                .wrap(Wrap { trim: false }),
            inner,
        ),
        (Some(Content::Image(img)), None) => {
            f.render_stateful_widget(ratatui_image::StatefulImage::default(), inner, &mut img.0)
        }
        (Some(Content::Text(text)), None) => {
            let para = Paragraph::new(text.clone()).wrap(Wrap { trim: false });
            let max = (para.line_count(inner.width) as u16).saturating_sub(inner.height);
            v.scroll = v.scroll.min(max);
            f.render_widget(para.scroll((v.scroll, 0)), inner);
        }
        (None, None) => f.render_widget(
            Paragraph::new("loading…").style(Style::default().fg(Color::DarkGray)),
            inner,
        ),
    }
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
        "{} | port {} | peers: {total} ({online} online) | Tab switch, : command, :settings, Ctrl-C quit",
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
    use crate::app::ChatLine;
    use ratatui::text::Text;
    use ratatui::{backend::TestBackend, Terminal};

    fn app_with_messages(bodies: &[&str]) -> App {
        let mut app = App::new("me".into(), 7421);
        app.peers.push(Peer::new(
            "p".into(),
            "them".into(),
            None,
            PeerStatus::Online,
        ));
        app.history.insert(
            "p".into(),
            bodies
                .iter()
                .map(|b| ChatLine::text("them".into(), (*b).into()))
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
        assert_eq!(r.target, LinkTarget::Url("https://example.com".into()));
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
        assert!(app
            .link_regions
            .iter()
            .all(|r| r.target == LinkTarget::Url(url.into())));
    }

    #[test]
    fn separate_links_stay_separate() {
        let mut app = app_with_messages(&["https://a.com https://b.com", "www.c.com"]);
        render(&mut app, 100, 10);
        let urls: Vec<LinkTarget> = app.link_regions.iter().map(|r| r.target.clone()).collect();
        let want: Vec<LinkTarget> = ["https://a.com", "https://b.com", "https://www.c.com"]
            .into_iter()
            .map(|u| LinkTarget::Url(u.into()))
            .collect();
        assert_eq!(urls, want);
    }

    fn file_line(name: &str, state: FileState) -> ChatLine {
        ChatLine {
            file: Some(FileInfo {
                file_id: "f1".into(),
                name: name.into(),
                size: 2048,
                path: Some(format!("/tmp/{name}").into()),
                state,
            }),
            ..ChatLine::text("them".into(), name.into())
        }
    }

    fn screen_text(app: &mut App, w: u16, h: u16) -> String {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| draw(f, app)).unwrap();
        let buf = term.backend().buffer();
        (0..h)
            .map(|y| (0..w).map(|x| buf[(x, y)].symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn finished_image_name_opens_viewer() {
        let mut app = app_with_messages(&[]);
        app.history
            .get_mut("p")
            .unwrap()
            .push(file_line("cat.png", FileState::Done));
        let text = screen_text(&mut app, 100, 10);
        assert!(
            text.contains("cat.png (2.0 KB) — click or :view to open"),
            "{text}"
        );
        let r = app
            .link_regions
            .first()
            .expect("image name is clickable")
            .clone();
        assert_eq!(r.target, LinkTarget::File("/tmp/cat.png".into()));
        app.click(r.x_start, r.y);
        assert_eq!(app.pending_view, Some("/tmp/cat.png".into()));
        assert!(app.viewer.is_some());
        let text = screen_text(&mut app, 100, 10);
        assert!(text.contains("cat.png — Esc to close"), "{text}");
    }

    #[test]
    fn only_finished_files_are_clickable() {
        let mut app = app_with_messages(&[]);
        let h = app.history.get_mut("p").unwrap();
        h.push(file_line("cat.png", FileState::AwaitingDecision));
        h.push(file_line("notes.txt", FileState::Done));
        let text = screen_text(&mut app, 160, 10);
        assert!(text.contains(":accept or :reject"), "{text}");
        assert!(
            text.contains("click or :view to preview · saved to /tmp/notes.txt"),
            "{text}"
        );
        let targets: Vec<_> = app.link_regions.iter().map(|r| r.target.clone()).collect();
        assert_eq!(targets, vec![LinkTarget::File("/tmp/notes.txt".into())]);
    }

    #[test]
    fn text_viewer_renders_and_scrolls() {
        let mut app = app_with_messages(&[]);
        app.open_viewer("/tmp/notes.txt".into());
        let body: Vec<Line> = (1..=100).map(|i| Line::raw(format!("line {i}"))).collect();
        app.viewer.as_mut().unwrap().content = Some(Content::Text(Text::from(body)));
        let text = screen_text(&mut app, 80, 20);
        assert!(text.contains("notes.txt — ↑/↓ PgUp/PgDn scroll"), "{text}");
        assert!(text.contains("line 1 "), "{text}");
        app.scroll_viewer(50);
        let text = screen_text(&mut app, 80, 20);
        assert!(text.contains("line 51"), "{text}");
        assert!(!text.contains("line 1 "), "{text}");
        // Scrolling past the end clamps to the last page.
        app.scroll_viewer(10_000);
        let text = screen_text(&mut app, 80, 20);
        assert!(text.contains("line 100"), "{text}");
        assert!(app.viewer.as_ref().unwrap().scroll <= 100);
    }

    #[test]
    fn peer_labels_show_identity() {
        let mut app = App::new("me".into(), 7421);
        let key = "2fa6e4c84f12469fc1e46382b46c41c0859be1565aa55611c0254d0693e93399";
        app.peers.push(Peer {
            pubkey: Some(key.into()),
            verified: true,
            ..Peer::new("a".into(), "alice".into(), None, PeerStatus::Online)
        });
        app.peers.push(Peer {
            pubkey: Some("9c1d".repeat(16)),
            warning: Some("not the alice you know".into()),
            ..Peer::new("b".into(), "alice".into(), None, PeerStatus::Online)
        });
        app.peers.push(Peer::new(
            "c".into(),
            "oldie".into(),
            None,
            PeerStatus::Online,
        ));
        app.peers.push(Peer {
            claimed_pubkey: Some("77".repeat(32)),
            ..Peer::new("d".into(), "dora".into(), None, PeerStatus::Available)
        });
        let text = screen_text(&mut app, 120, 12);
        assert!(text.contains("✔ alice 2fa6e4c8"), "{text}");
        assert!(text.contains("⚠ alice 9c1d9c1d"), "{text}");
        assert!(text.contains("oldie unverified"), "{text}");
        assert!(text.contains("dora 77777777?"), "{text}");
        assert!(text.contains("key 2fa6e4c8 ✔ verified"), "title: {text}");
        app.selected = 1;
        let text = screen_text(&mut app, 120, 12);
        assert!(text.contains("⚠ not the alice you know"), "{text}");
    }

    #[test]
    fn settings_panel_renders_values() {
        let mut app = app_with_messages(&[]);
        app.run_command("settings");
        let text = screen_text(&mut app, 100, 20);
        assert!(text.contains("Incoming files"), "{text}");
        assert!(text.contains("‹ ask ›"), "{text}");
        assert!(text.contains("‹ 100 MB ›"), "{text}");
    }
}
