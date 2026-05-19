use std::net::SocketAddr;

use crate::net::PeerId;

/// Events that drive the App state machine.
#[derive(Debug)]
pub enum AppEvent {
    PeerDiscovered {
        id: PeerId,
        username: String,
        addr: SocketAddr,
    },
    PeerLost {
        id: PeerId,
    },
    PeerConnecting {
        id: PeerId,
        addr: SocketAddr,
        dialed: bool,
    },
    PeerConnected {
        id: PeerId,
        username: String,
    },
    PeerDisconnected {
        id: PeerId,
    },
    PeerError {
        id: PeerId,
        error: String,
    },
    MessageReceived {
        id: PeerId,
        from: String,
        body: String,
    },
}
