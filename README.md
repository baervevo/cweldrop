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
| `:`            | Command line                                   |
| `Ctrl-C`       | Quit                                           |

### Commands

| Command       | Effect                              |
| ------------- | ----------------------------------- |
| `:c <ip:port>` | Manually dial a peer               |
| `:nick <name>` | Change your username                |
| `:d`           | Disconnect from selected peer       |
| `:q`           | Quit                                |

## Wire protocol

Newline-delimited JSON frames over a single TCP connection:

```
{"kind":"hello","username":"alice","version":"0.1.0"}
{"kind":"text","body":"hello there"}
{"kind":"bye"}
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
