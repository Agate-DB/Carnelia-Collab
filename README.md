# Carnelia Collab

Barebones Rust CLI client and server for collaborative plain-text peer to peer editing over TCP.

Uses [Carnelia](https://github.com/Agate-DB/Carnelia) as the backend CRDT engine.

## What It Does
- TCP server that maintains room/doc text state.
- CLI clients connect, join a room/doc, and send insert/delete/cursor ops.
- Server broadcasts updates to all clients in the same room/doc.
- Plain-text snapshots are persisted to `data/<room>/<doc>`.
- MDCS `TextDoc` is used internally for edits; `/sync` requests a snapshot.

## Quick Start

### 1) Start the server

```powershell
cargo run -- server --addr 0.0.0.0:4000 --data-dir data
```

### 2) Connect clients

```powershell
cargo run -- client --addr 127.0.0.1:4000 --user Alice --room demo --doc shared.txt
cargo run -- client --addr 127.0.0.1:4000 --user Bob --room demo --doc shared.txt
```

### 3) Client commands
```
/insert <pos> <text>   (or: i <pos> <text>)
/delete <pos> <len>    (or: d <pos> <len>)
/cursor <pos>          (or: c <pos>)
/sync
/show
/users
/cursors
/quit
```

Positions are byte offsets (UTF-8 boundary clamped). Keep text ASCII for predictable indexes.

## Deployment (Real Users)
1. Build a release binary locally:

```powershell
cargo build --release
```

2. Copy the binary to your server (VPS) and run it:

```powershell
# example path after build
./target/release/testing_carnelia server --addr 127.0.0.1:4000 --data-dir data
```

3. Open the port in your firewall (e.g. 4000/tcp) if you are not using a proxy.
4. Clients connect using your public IP or domain:

```powershell
./target/release/testing_carnelia client --addr <public-ip>:4000 --user Alice --room demo --doc shared.txt
```

### TLS with Nginx Stream
Use Nginx stream to terminate TLS on port 443 and proxy to the TCP backend.

1. Copy `deploy/nginx-stream.conf` to `/etc/nginx/conf.d/collab-stream.conf`.
2. Replace the certificate paths with your domain cert (e.g. Let’s Encrypt).
3. Reload Nginx:

```bash
sudo nginx -t
sudo systemctl reload nginx
```

4. Run the server on localhost and connect clients through 443:

```bash
./target/release/testing_carnelia server --addr 127.0.0.1:4000 --data-dir data
```

```bash
./target/release/testing_carnelia client --addr <your-domain>:443 --user Alice --room demo --doc shared.txt
```

### Optional: Run under systemd (Linux)
- Create a service that runs the binary with your preferred `--addr` and `--data-dir`.
- Ensure the working directory is writable for `data/` snapshots.

## Share via ngrok (Quick Demo)
ngrok can forward raw TCP so others can connect without a VPS. This is best for short demos.

1. Install and authenticate ngrok (see ngrok docs):
2. Start the server locally:

```powershell
cargo run -- server --addr 127.0.0.1:4000 --data-dir data
```

3. Start ngrok TCP tunnel to port 4000:

```powershell
ngrok tcp 4000
```

4. ngrok will print a public address like `tcp://0.tcp.ngrok.io:12345`.
   Share the host and port with others:

```powershell
cargo run -- client --addr 0.tcp.ngrok.io:12345 --user Alice --room demo --doc shared.txt
```

Notes:
- ngrok TCP does not add TLS. For encrypted connections, use a VPS with Nginx stream + TLS.
- If you want a stable address, use an ngrok reserved TCP address.

## Minimal TUI Frontend
The TUI joins/leaves automatically and manages cursor movement and edits.

Remote cursors are shown as colored highlights, and a short cursor list is visible in the status line.

Start the server:

```powershell
cargo run -- server --addr 127.0.0.1:4000 --data-dir data
```

Run the TUI client:

```powershell
cargo run -- tui --addr 127.0.0.1:4000 --user Alice --room demo --doc shared.txt
```

Controls:
- Arrow keys: move cursor
- Home/End: line start/end
- Enter: newline
- Backspace/Delete: remove characters
- Ctrl+R: request sync
- Ctrl+Q or Esc: quit

For ngrok, use the public host:port as the `--addr` value.

## Docker Deployment
Build and run the server in a container and publish port 4000.
Via docker-compose:

```powershell
docker compose up --build
```

You can still connect from clients using the host IP or from ngrok by forwarding port 4000 on the host:

```powershell
ngrok tcp 4000
```

## Protocol
Line-delimited JSON over TCP.
- Client → Server: `Join`, `Insert`, `Delete`, `Cursor`, `SyncRequest`, `Ping`
- Server → Client: `Welcome`, `Applied`, `Presence`, `SyncResponse`, `Error`

See `src/protocol.rs` for full message schemas.

## Notes
- The current `/sync` command provides snapshot sync (full text) for now.