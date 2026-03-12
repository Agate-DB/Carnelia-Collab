use crate::protocol::{
    Op, WireUser, decode_update, doc_id_from_scoped_user_id, encode_sync_response, encode_update,
};
use crate::storage::Storage;
use mdcs_sdk::{CollaborativeDoc, Message, TextDoc};
use std::collections::HashMap;
use std::error::Error;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex, broadcast, mpsc};

struct DocState {
    doc: TextDoc,
    version: u64,
    cursors: HashMap<String, usize>,
}

struct UserState {
    id: String,
    name: String,
    room: String,
    doc: String,
}

struct SharedState {
    users: HashMap<String, UserState>,
    docs: HashMap<String, DocState>,
    storage: Storage,
}

pub async fn run(addr: &str, data_dir: &str, health_addr: &str) -> Result<(), Box<dyn Error>> {
    let health_listener = TcpListener::bind(health_addr).await?;
    println!("[health] listening on {}", health_addr);
    tokio::spawn(async move {
        if let Err(err) = run_health_loop(health_listener).await {
            println!("[health] error: {}", err);
        }
    });

    let listener = TcpListener::bind(addr).await?;
    println!("[server] listening on {}", addr);

    let state = Arc::new(Mutex::new(SharedState {
        users: HashMap::new(),
        docs: HashMap::new(),
        storage: Storage::new(data_dir),
    }));

    let (broadcast_tx, _) = broadcast::channel::<Message>(256);

    loop {
        let (stream, peer) = listener.accept().await?;
        println!("[server] connection from {}", peer);
        let state = Arc::clone(&state);
        let broadcast_tx = broadcast_tx.clone();
        let broadcast_rx = broadcast_tx.subscribe();
        tokio::spawn(async move {
            if let Err(err) = handle_connection(stream, state, broadcast_tx, broadcast_rx).await {
                println!("[server] connection error: {}", err);
            }
        });
    }
}

async fn run_health_loop(listener: TcpListener) -> Result<(), Box<dyn Error>> {
    loop {
        let (stream, _) = listener.accept().await?;
        tokio::spawn(async move {
            if let Err(err) = handle_health_conn(stream).await {
                println!("[health] request error: {}", err);
            }
        });
    }
}

async fn handle_health_conn(stream: TcpStream) -> Result<(), Box<dyn Error>> {
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();

    let request_line = match lines.next_line().await? {
        Some(line) => line,
        None => return Ok(()),
    };

    let ok = request_line.starts_with("GET /health");
    if ok {
        writer
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 2\r\n\r\nOK",
            )
            .await?;
    } else {
        writer
            .write_all(
                b"HTTP/1.1 404 Not Found\r\nContent-Type: text/plain\r\nContent-Length: 9\r\n\r\nNot Found",
            )
            .await?;
    }

    Ok(())
}

async fn handle_connection(
    stream: TcpStream,
    state: Arc<Mutex<SharedState>>,
    broadcast_tx: broadcast::Sender<Message>,
    mut broadcast_rx: broadcast::Receiver<Message>,
) -> Result<(), Box<dyn Error>> {
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();

    let (out_tx, mut out_rx) = mpsc::channel::<Message>(64);

    let mut current_user_id: Option<String> = None;
    let mut current_user_name: Option<String> = None;
    let mut current_room: Option<String> = None;
    let mut current_doc: Option<String> = None;

    let writer_task = tokio::spawn(async move {
        while let Some(msg) = out_rx.recv().await {
            let json = match serde_json::to_string(&msg) {
                Ok(json) => json,
                Err(_) => continue,
            };
            if writer.write_all(json.as_bytes()).await.is_err() {
                break;
            }
            if writer.write_all(b"\n").await.is_err() {
                break;
            }
        }
    });

    loop {
        tokio::select! {
            line = lines.next_line() => {
                let line = match line {
                    Ok(Some(line)) => line,
                    Ok(None) => break,
                    Err(err) => {
                        println!("[server] read error: {}", err);
                        break;
                    }
                };

                let msg: Message = match serde_json::from_str(&line) {
                    Ok(msg) => msg,
                    Err(_) => continue,
                };

                match msg {
                    Message::Hello {
                        replica_id,
                        user_name,
                    } => {
                        current_user_id = Some(replica_id);
                        current_user_name = Some(user_name);
                    }
                    Message::SyncRequest { document_id, .. } => {
                        if current_user_id.is_none() || current_user_name.is_none() {
                            continue;
                        }

                        if doc_id_from_scoped_user_id(current_user_id.as_deref().unwrap())
                            != Some(document_id.as_str())
                        {
                            println!(
                                "[server] replica_id not scoped to document: {}",
                                document_id
                            );
                            continue;
                        }

                        let (room, doc) = split_doc_id(&document_id);
                        current_room = Some(room.clone());
                        current_doc = Some(doc.clone());

                        let mut guard = state.lock().await;
                        let doc_key = doc_key(&room, &doc);
                        let (doc_text, doc_version) = if let Some(doc_state) = guard.docs.get(&doc_key) {
                            (doc_state.doc.get_text(), doc_state.version)
                        } else {
                            let text = guard.storage.load_text(&room, &doc).unwrap_or_default();
                            let mut new_doc = TextDoc::new(doc_key.clone(), "server");
                            if !text.is_empty() {
                                new_doc.insert(0, &text);
                            }
                            guard.docs.insert(
                                doc_key.clone(),
                                DocState {
                                    doc: new_doc,
                                    version: 0,
                                    cursors: HashMap::new(),
                                },
                            );
                            (text, 0)
                        };

                        let user_id = current_user_id.clone().unwrap();
                        let user_name = current_user_name.clone().unwrap();
                        let user_state = UserState {
                            id: user_id.clone(),
                            name: user_name.clone(),
                            room: room.clone(),
                            doc: doc.clone(),
                        };
                        guard.users.insert(user_id.clone(), user_state);

                        let users = users_in_doc(&guard.users, &room, &doc);
                        match encode_sync_response(&document_id, &doc_text, users, doc_version) {
                            Ok(sync) => {
                                let _ = out_tx.send(sync).await;
                            }
                            Err(err) => {
                                println!("[server] failed to encode sync response: {}", err);
                            }
                        }
                        drop(guard);

                        let _ = broadcast_tx.send(Message::Hello {
                            replica_id: user_id,
                            user_name,
                        });
                    }
                    Message::Update { .. } => {
                        handle_update(
                            &state,
                            &broadcast_tx,
                            current_user_id.as_deref(),
                            current_room.as_deref(),
                            current_doc.as_deref(),
                            &msg,
                        )
                        .await;
                    }
                    Message::Presence {
                        user_id,
                        document_id,
                        cursor_pos,
                    } => {
                        if let (Some(current_id), Some(room), Some(doc)) = (
                            current_user_id.as_deref(),
                            current_room.as_deref(),
                            current_doc.as_deref(),
                        ) {
                            if user_id != current_id {
                                println!("[server] ignoring spoofed presence for {}", user_id);
                                continue;
                            }
                            if document_id != doc_key(room, doc) {
                                continue;
                            }
                            let mut guard = state.lock().await;
                            if let Some(doc_state) = guard.docs.get_mut(&document_id) {
                                match cursor_pos {
                                    Some(pos) => {
                                        doc_state.cursors.insert(user_id.clone(), pos);
                                    }
                                    None => {
                                        doc_state.cursors.remove(&user_id);
                                    }
                                }
                            }
                            drop(guard);
                            let _ = broadcast_tx.send(Message::Presence {
                                user_id,
                                document_id,
                                cursor_pos,
                            });
                        }
                    }
                    Message::SyncResponse { .. } => {}
                    Message::Ack { .. } | Message::Ping | Message::Pong => {}
                }
            }
            event = broadcast_rx.recv() => {
                if let Ok(event) = event
                    && should_forward(&event, current_room.as_deref(), current_doc.as_deref())
                {
                    let _ = out_tx.send(event).await;
                }
            }
        }
    }

    if let Some(user_id) = current_user_id {
        let mut guard = state.lock().await;
        guard.users.remove(&user_id);
        if let (Some(room), Some(doc)) = (current_room.take(), current_doc.take()) {
            let document_id = doc_key(&room, &doc);
            let _ = broadcast_tx.send(Message::Presence {
                user_id,
                document_id,
                cursor_pos: None,
            });
        }
    }

    writer_task.abort();
    Ok(())
}

async fn handle_update(
    state: &Arc<Mutex<SharedState>>,
    broadcast_tx: &broadcast::Sender<Message>,
    current_user_id: Option<&str>,
    room: Option<&str>,
    doc: Option<&str>,
    msg: &Message,
) {
    if current_user_id.is_none() {
        return;
    }
    let Some(room) = room else {
        return;
    };
    let Some(doc) = doc else {
        return;
    };

    let Some((document_id, payload, _)) = decode_update(msg) else {
        return;
    };
    if let Some(current_id) = current_user_id {
        if payload.user_id != current_id {
            println!("[server] ignoring spoofed update for {}", payload.user_id);
            return;
        }
    }
    if document_id != doc_key(room, doc) {
        return;
    }

    let mut guard = state.lock().await;
    let doc_key = doc_key(room, doc);
    if !guard.docs.contains_key(&doc_key) {
        let text = guard.storage.load_text(room, doc).unwrap_or_default();
        let mut new_doc = TextDoc::new(doc_key.clone(), "server");
        if !text.is_empty() {
            new_doc.insert(0, &text);
        }
        guard.docs.insert(
            doc_key.clone(),
            DocState {
                doc: new_doc,
                version: 0,
                cursors: HashMap::new(),
            },
        );
    }

    let (updated_text, version, op, delta) = {
        let doc_state = guard.docs.get_mut(&doc_key).expect("doc exists");
        apply_op_to_doc(doc_state, &payload.user_id, &payload.op);
        let delta = Vec::new();
        doc_state.version += 1;
        (
            doc_state.doc.get_text(),
            doc_state.version,
            payload.op,
            delta,
        )
    };

    let _ = guard.storage.save_text(room, doc, &updated_text);
    drop(guard);

    match op {
        Op::Cursor { pos } => {
            let _ = broadcast_tx.send(Message::Presence {
                user_id: payload.user_id,
                document_id: doc_key,
                cursor_pos: Some(pos),
            });
        }
        _ => {
            match encode_update(&doc_key, &payload.user_id, op, delta, version) {
                Ok(update) => {
                    let _ = broadcast_tx.send(update);
                }
                Err(err) => {
                    println!("[server] failed to encode update: {}", err);
                }
            }
        }
    }
}

fn should_forward(msg: &Message, room: Option<&str>, doc: Option<&str>) -> bool {
    let Some(room) = room else {
        return false;
    };
    let Some(doc) = doc else {
        return false;
    };
    let doc_id = doc_key(room, doc);
    match msg {
        Message::Hello { replica_id, .. } => {
            doc_id_from_scoped_user_id(replica_id) == Some(doc_id.as_str())
        }
        Message::Update { document_id, .. } => document_id == &doc_id,
        Message::Presence { document_id, .. } => document_id == &doc_id,
        Message::SyncResponse { document_id, .. } => document_id == &doc_id,
        Message::Ack { .. } | Message::Ping | Message::Pong | Message::SyncRequest { .. } => false,
    }
}

fn users_in_doc(users: &HashMap<String, UserState>, room: &str, doc: &str) -> Vec<WireUser> {
    users
        .values()
        .filter(|u| u.room == room && u.doc == doc)
        .map(|u| WireUser {
            id: u.id.clone(),
            name: u.name.clone(),
        })
        .collect()
}

fn doc_key(room: &str, doc: &str) -> String {
    format!("{}/{}", room, doc)
}

fn clamp_to_boundary(text: &str, pos: usize) -> usize {
    let mut pos = pos.min(text.len());
    while pos > 0 && !text.is_char_boundary(pos) {
        pos -= 1;
    }
    pos
}

fn byte_to_char_index(text: &str, byte_pos: usize) -> usize {
    let byte_pos = clamp_to_boundary(text, byte_pos);
    text[..byte_pos].chars().count()
}

fn apply_op_to_doc(doc_state: &mut DocState, user_id: &str, op: &Op) {
    match op {
        Op::Insert { pos, text } => {
            let current = doc_state.doc.get_text();
            let char_pos = byte_to_char_index(&current, *pos);
            doc_state.doc.insert(char_pos, text);
        }
        Op::Delete { pos, len } => {
            let current = doc_state.doc.get_text();
            if current.is_empty() {
                return;
            }
            let start = clamp_to_boundary(&current, *pos);
            let end = clamp_to_boundary(&current, start.saturating_add(*len));
            if start >= end {
                return;
            }
            let char_start = current[..start].chars().count();
            let char_len = current[start..end].chars().count();
            if char_len > 0 {
                doc_state.doc.delete(char_start, char_len);
            }
        }
        Op::Cursor { pos } => {
            let current = doc_state.doc.get_text();
            let clamped = clamp_to_boundary(&current, *pos);
            doc_state.cursors.insert(user_id.to_string(), clamped);
        }
    }
}

fn split_doc_id(document_id: &str) -> (String, String) {
    match document_id.split_once('/') {
        Some((room, doc)) => (room.to_string(), doc.to_string()),
        None => ("default".to_string(), document_id.to_string()),
    }
}
