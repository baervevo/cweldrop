use std::net::SocketAddr;
use std::path::PathBuf;

use crate::net::PeerId;

/// Events that drive the App state machine.
#[derive(Debug)]
pub enum AppEvent {
    PeerDiscovered {
        id: PeerId,
        username: String,
        addr: SocketAddr,
        /// Pubkey from the mDNS TXT record. Unverified: only used to match
        /// the announcement to a known peer row; the handshake proves it.
        pubkey: Option<String>,
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
    /// The authenticated peer speaks the signed message chain; text to it
    /// goes through `crate::chain` (and can be queued while it's offline).
    PeerChainReady {
        id: PeerId,
    },
    /// New verified blocks were added to the chain with `pubkey`.
    /// `from_peer` holds the bodies of those the peer wrote (for alerts).
    ChainChanged {
        id: PeerId,
        pubkey: String,
        from_peer: Vec<String>,
    },
    /// A chain block was written to the peer's socket.
    BlockDelivered {
        id: PeerId,
        hash: String,
    },
    /// Remote peer started (`active=true`) or stopped typing.
    PeerTyping {
        id: PeerId,
        active: bool,
    },
    /// Remote wants to send us a file; answer with `NetCmd::AcceptFile` or
    /// `NetCmd::RejectFile`. `name` is already sanitised.
    FileOffered {
        id: PeerId,
        file_id: String,
        name: String,
        size: u64,
    },
    /// Remote accepted our offer; chunks are flowing.
    FileAccepted {
        id: PeerId,
        file_id: String,
    },
    /// Remote declined our offer.
    FileRejected {
        id: PeerId,
        file_id: String,
    },
    /// Bytes moved so far, in either direction.
    FileProgress {
        id: PeerId,
        file_id: String,
        bytes: u64,
    },
    /// Our file was fully sent.
    FileSent {
        id: PeerId,
        file_id: String,
    },
    /// An incoming file was verified and saved at `path`.
    FileReceived {
        id: PeerId,
        file_id: String,
        path: PathBuf,
    },
    FileFailed {
        id: PeerId,
        file_id: String,
        error: String,
    },
    /// An image the user asked to view finished decoding (off the UI thread).
    ImageDecoded {
        path: PathBuf,
        result: Result<ImageProto, String>,
    },
}

/// A decoded image ready for the terminal graphics protocol.
pub struct ImageProto(pub ratatui_image::protocol::StatefulProtocol);

impl std::fmt::Debug for ImageProto {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ImageProto")
    }
}
