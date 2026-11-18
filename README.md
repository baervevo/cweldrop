# cweldrop

AirDrop-inspired terminal chat for the local network. Single Rust binary that
acts as both server and client: opens a TCP port, advertises itself over
mDNS/zeroconf, browses the LAN for peers, and chats with them in a TUI.

Built on plain TCP + newline-delimited JSON — netcat with a face.

## Build

```sh
cargo build --release
./target/release/cweldrop --help
```

## Run

```sh
# Auto-discovery + chat on default port 7421
cweldrop

# Custom nick + port
cweldrop --nick alice --port 7421

# Disable mDNS, dial peers manually with `:c <ip:port>`
cweldrop --no-mdns --port 7421
```

Two instances on the same host (for testing):

```sh
# terminal 1
cweldrop --port 7421 --nick a

# terminal 2
cweldrop --port 7422 --nick b
```

## Keys

| Key            | Action                                         |
| -------------- | ---------------------------------------------- |
| `Tab`          | Switch focus between peer list and input       |
| `↑`/`↓`, `j/k` | Move peer selection                            |
| `Enter` (list) | Connect to selected peer (if not already)      |
| `Enter` (input)| Send message to selected peer                  |
| `:`            | Command line with a fuzzy-search hint panel: ↑/↓ or Ctrl-P/N pick, Tab completes, Enter runs |
| Click link     | Open it in the browser (Shift+drag to select text) |
| `Ctrl-C`       | Quit                                           |

### Commands

| Command       | Effect                              |
| ------------- | ----------------------------------- |
| `:c <ip:port>` | Manually dial a peer               |
| `:nick <name>` | Change your username                |
| `:d`           | Disconnect from selected peer       |
| `:open [n]`    | Open the newest (or n-th newest) link in this chat |
| `:send <path>` | Offer a file to the selected peer   |
| `:accept [n\|name\|all]` / `:reject …` | Answer a file offer; with several pending and no argument, opens `:files` |
| `:files`       | Pick from all files in this chat: Enter accept/open, `a`/`r` accept/reject, `A`/`R` all pending |
| `:view [n\|name]` | Open a file in this chat (also: click its name): images in the graphics viewer, text with [bat](https://github.com/sharkdp/bat) highlighting (plain text without bat) |
| `:settings`    | Settings panel: incoming file policy (ask / auto-accept / reject), size cap, download folder |
| `:verify`      | Show the safety code for this chat; if your peer sees the same code, you hold each other's real keys |
| `:verify confirm` / `:verify forget` | Mark this contact's key as verified (✔), or undo it |
| `:q`           | Quit                                |
| `:help`        | List all commands in the hint panel |

## Identity

A peer is its ed25519 key, not its nickname. Each install generates a key
once (`identity.key`); the handshake proves possession of it. The peer list
shows a short key fingerprint next to every name (`alice 2fa6e4c8`; a `?`
means announced over mDNS but not yet proven). Keys are pinned on first
use: if someone answers for a known peer with a different key, the
connection is refused (`key mismatch`), and a new key that uses the name of
a contact you already know is flagged in red (`⚠ not the alice you know`).
Compare safety codes with `:verify`, then `:verify confirm` to get a ✔.
Old clients without keys are shown as `unverified`.

## Wire protocol

Newline-delimited JSON frames over a single TCP connection:

```
{"kind":"hello","username":"alice","version":"0.1.0"}
{"kind":"text","body":"hello there"}
{"kind":"bye"}
```

Signed message chain (peers whose `hello` carries `"caps":["chain1"]`):
every text message is a block signed by its author and hash-linked to its
parents, rooted at `sha256("cweldrop-genesis-v1" ‖ sorted pubkeys)`. Nobody
else can inject a message, and none can be edited or reordered without
breaking the hashes. Messages to an offline contact are queued in the chain;
on connect each side sends its heads and gets back every block it's missing.
Writes made on both sides while apart fork the chain; the next message
merges the branches, and both sides sort the result identically. Stored in
`~/.local/share/cweldrop/chains/<pubkey>.jsonl`.

```
{"kind":"chain_heads","heads":["5e1f…","a90c…"]}
{"kind":"chain_block","block":{"prev":["5e1f…"],"author":"<pubkey>","at":1763489000123,"body":"hi","sig":"…","hash":"…"}}
```

Marks after your messages: ⏳ queued (peer offline), ✓ delivered, ✓✓ the
peer has signed a later message on top of it.

File transfer (receiver must answer the offer before any data flows; chunks
are 32 KiB base64 and interleave with chat; the receiver checks size and
SHA-256 before moving the file out of a hidden `.part` file):

```
{"kind":"file_offer","file_id":"9f2c…","name":"cat.png","size":48213}
{"kind":"file_accept","file_id":"9f2c…"}        // or file_reject
{"kind":"file_chunk","file_id":"9f2c…","data":"iVBORw0K…"}
{"kind":"file_done","file_id":"9f2c…","sha256":"…"}
{"kind":"file_cancel","file_id":"9f2c…"}        // either side, any time
```

Both sides send `hello` immediately on connect; only then is the peer marked
`Online` in the UI.

## Logs

Written to `cweldrop.log` in the cwd (stdout/stderr would corrupt the TUI).
Override with `--log-file path` or filter with `RUST_LOG=cweldrop=debug`.

## Status

MVP. No encryption (LAN-trust model). 1:1 chat per peer. File transfer is
deliberately out of scope but the `Message` enum is `#[serde(tag = "kind")]`
so adding `file_offer` / `file_chunk` later is straightforward.
