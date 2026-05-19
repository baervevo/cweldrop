use std::io;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use crossterm::event::{Event as CtEvent, EventStream};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use futures_util::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use tokio::sync::mpsc;
use tracing::{error, info};

use cweldrop::app::App;
use cweldrop::event::AppEvent;
use cweldrop::identity::Identity;
use cweldrop::net::NetCmd;
use cweldrop::{discovery, input, net, ui};

#[derive(Parser, Debug)]
#[command(version, about = "AirDrop-inspired terminal chat over LAN")]
struct Cli {
    /// TCP port to listen on (0 = pick free port)
    #[arg(long, default_value_t = 7421)]
    port: u16,

    /// Username advertised to peers (defaults to $USER@$HOSTNAME)
    #[arg(long)]
    nick: Option<String>,

    /// Disable mDNS advertise + browse
    #[arg(long, default_value_t = false)]
    no_mdns: bool,

    /// Log file path
    #[arg(long, default_value = "cweldrop.log")]
    log_file: String,
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let file_appender = tracing_appender::rolling::never(".", &cli.log_file);
    let (nb, _guard) = tracing_appender::non_blocking(file_appender);
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(nb)
        .with_ansi(false)
        .init();

    let username = cli.nick.unwrap_or_else(default_nick);
    let identity = Arc::new(Identity::load_or_create().context("load identity")?);
    info!(
        %username,
        port = cli.port,
        pubkey = %identity.pubkey_hex(),
        "starting cweldrop"
    );

    let (evt_tx, mut evt_rx) = mpsc::channel::<AppEvent>(256);
    let (cmd_tx, cmd_rx) = mpsc::channel::<NetCmd>(256);

    let actual_port = net::run_net(
        cli.port,
        username.clone(),
        Arc::clone(&identity),
        cmd_rx,
        evt_tx.clone(),
    )
    .await?;

    let discovery = if cli.no_mdns {
        None
    } else {
        match discovery::Discovery::start(&username, actual_port) {
            Ok(d) => {
                if let Err(e) = d.spawn_browser(evt_tx.clone()) {
                    error!(error=%e, "mdns browse failed to start");
                }
                Some(d)
            }
            Err(e) => {
                error!(error=%e, "mdns disabled (start failed)");
                None
            }
        }
    };

    enable_raw_mode().context("enable raw mode")?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen).context("enter alt screen")?;
    let backend = CrosstermBackend::new(stdout);
    let mut term = Terminal::new(backend).context("terminal")?;

    let result = run_ui(
        &mut term,
        App::new(username, actual_port).with_pubkey(identity.pubkey_hex()),
        cmd_tx.clone(),
        &mut evt_rx,
    )
    .await;

    let _ = disable_raw_mode();
    let _ = execute!(term.backend_mut(), LeaveAlternateScreen);
    let _ = term.show_cursor();
    let _ = cmd_tx.send(NetCmd::Shutdown).await;
    if let Some(d) = discovery {
        d.shutdown();
    }

    result
}

async fn run_ui<B: ratatui::backend::Backend>(
    term: &mut Terminal<B>,
    mut app: App,
    cmd_tx: mpsc::Sender<NetCmd>,
    evt_rx: &mut mpsc::Receiver<AppEvent>,
) -> Result<()> {
    let mut keys = EventStream::new();
    term.draw(|f| ui::draw(f, &mut app))?;

    loop {
        if app.should_quit {
            return Ok(());
        }
        tokio::select! {
            ev = keys.next() => {
                match ev {
                    Some(Ok(CtEvent::Key(k))) => {
                        if k.kind == crossterm::event::KeyEventKind::Press {
                            let cmds = input::handle_key(&mut app, k);
                            for c in cmds {
                                if cmd_tx.send(c).await.is_err() {
                                    error!("net cmd channel closed");
                                    return Ok(());
                                }
                            }
                        }
                    }
                    Some(Ok(CtEvent::Resize(_, _))) => {}
                    Some(Ok(_)) => {}
                    Some(Err(e)) => {
                        error!(error=%e, "terminal event error");
                    }
                    None => return Ok(()),
                }
            }
            net_ev = evt_rx.recv() => {
                match net_ev {
                    Some(e) => app.on_event(e),
                    None => return Ok(()),
                }
            }
            _ = tokio::time::sleep(Duration::from_millis(500)) => {}
        }
        term.draw(|f| ui::draw(f, &mut app))?;
    }
}

fn default_nick() -> String {
    let host = hostname::get()
        .ok()
        .and_then(|h| h.into_string().ok())
        .unwrap_or_else(|| "host".to_string());
    let user = whoami::username();
    format!("{user}@{host}")
}
