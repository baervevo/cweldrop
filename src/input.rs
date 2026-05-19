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
            app.focus = Focus::Input;
            Vec::new()
        }
        KeyCode::Enter => {
            let line = std::mem::take(&mut app.command);
            app.focus = Focus::Input;
            app.run_command(&line)
        }
        KeyCode::Backspace => {
            app.command.pop();
            Vec::new()
        }
        KeyCode::Char(c) => {
            app.command.push(c);
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
            Vec::new()
        }
        KeyCode::Down | KeyCode::Char('j') => {
            app.select_next();
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
            Vec::new()
        }
        KeyCode::Char(':') if app.input.is_empty() => {
            app.focus = Focus::Command;
            Vec::new()
        }
        KeyCode::Backspace => {
            app.input.pop();
            Vec::new()
        }
        KeyCode::Enter => {
            let body = std::mem::take(&mut app.input);
            if body.is_empty() {
                return Vec::new();
            }
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
            app.input.push(c);
            Vec::new()
        }
        _ => Vec::new(),
    }
}
