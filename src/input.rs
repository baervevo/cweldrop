use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::app::{App, FilesAction, Focus, PeerStatus, SETTINGS_ROWS};
use crate::net::NetCmd;

/// Handle a keypress; return any NetCmds to emit.
pub fn handle_key(app: &mut App, k: KeyEvent) -> Vec<NetCmd> {
    // Ctrl-C: always quit.
    if k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char('c') {
        app.should_quit = true;
        return Vec::new();
    }

    // The viewer is modal: close or scroll, other keys are ignored.
    if app.viewer.is_some() {
        match k.code {
            KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') => app.close_viewer(),
            KeyCode::Up | KeyCode::Char('k') => app.scroll_viewer(-1),
            KeyCode::Down | KeyCode::Char('j') => app.scroll_viewer(1),
            KeyCode::PageUp | KeyCode::Left | KeyCode::Char('p') => {
                if !app.viewer_page(-1) {
                    app.scroll_viewer(-20);
                }
            }
            KeyCode::PageDown | KeyCode::Right | KeyCode::Char(' ' | 'n') => {
                if !app.viewer_page(1) {
                    app.scroll_viewer(20);
                }
            }
            KeyCode::Home | KeyCode::Char('g') => app.scroll_viewer(i32::MIN / 2),
            KeyCode::End | KeyCode::Char('G') => app.scroll_viewer(i32::MAX / 2),
            _ => {}
        }
        return Vec::new();
    }

    // Scrollback (works in any non-Command focus).
    if !matches!(app.focus, Focus::Command | Focus::Settings | Focus::Files) {
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
        Focus::Settings => {
            handle_settings(app, k);
            Vec::new()
        }
        Focus::Files => {
            let action = match k.code {
                KeyCode::Up | KeyCode::Char('k') => FilesAction::Up,
                KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => FilesAction::Down,
                KeyCode::Enter => FilesAction::Open,
                KeyCode::Char('a') => FilesAction::Accept,
                KeyCode::Char('r') | KeyCode::Char('d') => FilesAction::Reject,
                KeyCode::Char('A') => FilesAction::AcceptAll,
                KeyCode::Char('R') => FilesAction::RejectAll,
                KeyCode::Esc | KeyCode::Char('q') => FilesAction::Close,
                _ => return Vec::new(),
            };
            app.files_action(action)
        }
    }
}

fn handle_settings(app: &mut App, k: KeyEvent) {
    if let Some(buf) = app.settings_edit.as_mut() {
        match k.code {
            KeyCode::Esc => app.settings_edit = None,
            KeyCode::Enter => app.settings_commit_edit(),
            KeyCode::Backspace => {
                buf.pop();
            }
            KeyCode::Char(c) => buf.push(c),
            _ => {}
        }
        return;
    }
    match k.code {
        KeyCode::Esc | KeyCode::Char('q') => app.focus = Focus::Input,
        KeyCode::Up | KeyCode::Char('k') => {
            app.settings_sel = (app.settings_sel + SETTINGS_ROWS - 1) % SETTINGS_ROWS;
        }
        KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => {
            app.settings_sel = (app.settings_sel + 1) % SETTINGS_ROWS;
        }
        KeyCode::Right | KeyCode::Enter | KeyCode::Char(' ' | 'l') => app.settings_adjust(true),
        KeyCode::Left | KeyCode::Char('h') => app.settings_adjust(false),
        _ => {}
    }
}

fn handle_command(app: &mut App, k: KeyEvent) -> Vec<NetCmd> {
    match k.code {
        KeyCode::Esc => {
            app.command.clear();
            app.command_cursor = 0;
            app.focus = Focus::Input;
            app.reset_file_search();
            Vec::new()
        }
        KeyCode::Enter => app.command_enter(),
        KeyCode::Up => {
            app.command_move(-1);
            Vec::new()
        }
        KeyCode::Down => {
            app.command_move(1);
            Vec::new()
        }
        KeyCode::Char('p') if k.modifiers.contains(KeyModifiers::CONTROL) => {
            app.command_move(-1);
            Vec::new()
        }
        KeyCode::Char('n') if k.modifiers.contains(KeyModifiers::CONTROL) => {
            app.command_move(1);
            Vec::new()
        }
        KeyCode::Tab => {
            app.command_complete();
            app.refresh_file_search();
            Vec::new()
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
            app.cmd_sel = 0;
            app.refresh_file_search();
            Vec::new()
        }
        KeyCode::Delete => {
            delete_at_cursor(&mut app.command, app.command_cursor);
            app.cmd_sel = 0;
            app.refresh_file_search();
            Vec::new()
        }
        KeyCode::Char(c) => {
            insert_at_cursor(&mut app.command, &mut app.command_cursor, c);
            app.cmd_sel = 0;
            app.refresh_file_search();
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
            app.typing_stop()
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
            typing_feedback(app)
        }
        KeyCode::Delete => {
            delete_at_cursor(&mut app.input, app.input_cursor);
            typing_feedback(app)
        }
        KeyCode::Enter => {
            let body = std::mem::take(&mut app.input);
            app.input_cursor = 0;
            if body.is_empty() {
                return Vec::new();
            }
            // Sending jumps the view back to newest.
            app.chat_scroll = 0;
            let mut cmds = app.typing_stop();
            cmds.extend(app.send_text(body));
            cmds
        }
        KeyCode::Char(c) => {
            insert_at_cursor(&mut app.input, &mut app.input_cursor, c);
            app.typing_ping()
        }
        _ => Vec::new(),
    }
}

/// After an edit, ping typing if there's still text, otherwise signal stop.
fn typing_feedback(app: &mut App) -> Vec<NetCmd> {
    if app.input.is_empty() {
        app.typing_stop()
    } else {
        app.typing_ping()
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
