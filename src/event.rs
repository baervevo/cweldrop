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
        resolved_id: Option<PeerId>,
    },
    /// Fired after the cryptographic handshake completes. `pubkey` is the
    /// verified hex-encoded ed25519 pubkey of the remote and becomes the
    /// canonical id under which history is keyed.
    PeerAuthenticated {
        id: PeerId,
        pubkey: PeerId,
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
    /// Remote peer started (`active=true`) or stopped typing.
    PeerTyping {
        id: PeerId,
        active: bool,
    },
}
