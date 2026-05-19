use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::app::{App, Focus, PeerStatus};
use crate::net::NetCmd;

/// Handle a keypress; return any NetCmds to emit.
pub fn handle_key(app: &mut App, k: KeyEvent) -> Vec<NetCmd> {
    // Ctrl-C: always quit.
    if k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char('c') {
        app.should_quit = true;
        return Vec::new();
    }

    // Scrollback (works in any non-Command focus).
    if app.focus != Focus::Command {
        match k.code {
            KeyCode::PageUp => {
                app.chat_scroll = app.chat_scroll.saturating_add(5);
                return Vec::new();
            }
            KeyCode::PageDown => {
                app.chat_scroll = app.chat_scroll.saturating_sub(5);
                return Vec::new();
            }
            _ => {}
        }
    }

    match app.focus {
        Focus::Command => handle_command(app, k),
        Focus::Peers => handle_peers(app, k),
        Focus::Input => handle_input(app, k),
    }
}

fn handle_command(app: &mut App, k: KeyEvent) -> Vec<NetCmd> {
    match k.code {
        KeyCode::Esc => {
            app.command.clear();
            app.command_cursor = 0;
            app.focus = Focus::Input;
            Vec::new()
        }
        KeyCode::Enter => {
            let line = std::mem::take(&mut app.command);
            app.command_cursor = 0;
            app.focus = Focus::Input;
            app.run_command(&line)
        }
        KeyCode::Left => {
            move_cursor_left(&app.command, &mut app.command_cursor);
            Vec::new()
        }
        KeyCode::Right => {
            move_cursor_right(&app.command, &mut app.command_cursor);
            Vec::new()
        }
        KeyCode::Home => {
            app.command_cursor = 0;
            Vec::new()
        }
        KeyCode::End => {
            app.command_cursor = app.command.len();
            Vec::new()
        }
        KeyCode::Backspace => {
            delete_before_cursor(&mut app.command, &mut app.command_cursor);
            Vec::new()
        }
        KeyCode::Delete => {
            delete_at_cursor(&mut app.command, app.command_cursor);
            Vec::new()
        }
        KeyCode::Char(c) => {
            insert_at_cursor(&mut app.command, &mut app.command_cursor, c);
            Vec::new()
        }
        _ => Vec::new(),
    }
}

fn handle_peers(app: &mut App, k: KeyEvent) -> Vec<NetCmd> {
    match k.code {
        KeyCode::Tab => {
            app.focus = Focus::Input;
            Vec::new()
        }
        KeyCode::Up | KeyCode::Char('k') => {
            app.select_prev();
            app.chat_scroll = 0;
            Vec::new()
        }
        KeyCode::Down | KeyCode::Char('j') => {
            app.select_next();
            app.chat_scroll = 0;
            Vec::new()
        }
        KeyCode::Enter => {
            // Connect to selected peer if not already connected.
            if let Some(p) = app.selected_peer() {
                match p.status {
                    PeerStatus::Available | PeerStatus::Offline | PeerStatus::Error => {
                        if let Some(addr) = p.addr {
                            let id = p.id.clone();
                            app.status_msg = format!("dialing {addr}");
                            return vec![NetCmd::Connect { id, addr }];
                        }
                    }
                    _ => {}
                }
                app.focus = Focus::Input;
            }
            Vec::new()
        }
        KeyCode::Char(':') => {
            app.focus = Focus::Command;
            Vec::new()
        }
        _ => Vec::new(),
    }
}

fn handle_input(app: &mut App, k: KeyEvent) -> Vec<NetCmd> {
    match k.code {
        KeyCode::Tab => {
            app.focus = Focus::Peers;
            Vec::new()
        }
        KeyCode::Esc => {
            app.input.clear();
            app.input_cursor = 0;
            Vec::new()
        }
        KeyCode::Char(':') if app.input.is_empty() => {
            app.focus = Focus::Command;
            Vec::new()
        }
        KeyCode::Left => {
            move_cursor_left(&app.input, &mut app.input_cursor);
            Vec::new()
        }
        KeyCode::Right => {
            move_cursor_right(&app.input, &mut app.input_cursor);
            Vec::new()
        }
        KeyCode::Home => {
            app.input_cursor = 0;
            Vec::new()
        }
        KeyCode::End => {
            app.input_cursor = app.input.len();
            Vec::new()
        }
        KeyCode::Backspace => {
            delete_before_cursor(&mut app.input, &mut app.input_cursor);
            Vec::new()
        }
        KeyCode::Delete => {
            delete_at_cursor(&mut app.input, app.input_cursor);
            Vec::new()
        }
        KeyCode::Enter => {
            let body = std::mem::take(&mut app.input);
            app.input_cursor = 0;
            if body.is_empty() {
                return Vec::new();
            }
            // Sending jumps the view back to newest.
            app.chat_scroll = 0;
            if let Some(p) = app.selected_peer() {
                if matches!(p.status, PeerStatus::Online) {
                    let id = p.id.clone();
                    app.push_self_message(&id, body.clone());
                    return vec![NetCmd::SendText { id, body }];
                } else {
                    app.status_msg = format!("peer not connected ({:?})", p.status);
                }
            } else {
                app.status_msg = "no peer selected".to_string();
            }
            Vec::new()
        }
        KeyCode::Char(c) => {
            insert_at_cursor(&mut app.input, &mut app.input_cursor, c);
            Vec::new()
        }
        _ => Vec::new(),
    }
}

fn insert_at_cursor(s: &mut String, cursor: &mut usize, c: char) {
    let idx = (*cursor).min(s.len());
    s.insert(idx, c);
    *cursor = idx + c.len_utf8();
}

fn delete_before_cursor(s: &mut String, cursor: &mut usize) {
    if *cursor == 0 || s.is_empty() {
        return;
    }
    let mut new_cursor = *cursor - 1;
    while new_cursor > 0 && !s.is_char_boundary(new_cursor) {
        new_cursor -= 1;
    }
    s.replace_range(new_cursor..*cursor, "");
    *cursor = new_cursor;
}

fn delete_at_cursor(s: &mut String, cursor: usize) {
    if cursor >= s.len() {
        return;
    }
    let mut next = cursor + 1;
    while next < s.len() && !s.is_char_boundary(next) {
        next += 1;
    }
    s.replace_range(cursor..next, "");
}

fn move_cursor_left(s: &str, cursor: &mut usize) {
    if *cursor == 0 {
        return;
    }
    let mut nc = *cursor - 1;
    while nc > 0 && !s.is_char_boundary(nc) {
        nc -= 1;
    }
    *cursor = nc;
}

fn move_cursor_right(s: &str, cursor: &mut usize) {
    if *cursor >= s.len() {
        *cursor = s.len();
        return;
    }
    let mut nc = *cursor + 1;
    while nc < s.len() && !s.is_char_boundary(nc) {
        nc += 1;
    }
    *cursor = nc;
}
