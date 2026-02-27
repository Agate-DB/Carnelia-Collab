use crate::protocol::{ClientMessage, Op, ServerMessage, UserInfo};
use crate::storage::Storage;
use mdcs_sdk::TextDoc;
use std::collections::HashMap;
use std::error::Error;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, mpsc, Mutex};

struct DocState {
    doc: TextDoc,
    version: u64,
    cursors: HashMap<usize, usize>,
}

struct UserState {
    id: usize,
    name: String,
    room: String,
    doc: String,
}

struct SharedState {
    next_user_id: usize,
    users: HashMap<usize, UserState>,
    docs: HashMap<String, DocState>,
    storage: Storage,
}

pub async fn run(addr: &str, data_dir: &str) -> Result<(), Box<dyn Error>> {
    let listener = TcpListener::bind(addr).await?;
    println!("[server] listening on {}", addr);

    let state = Arc::new(Mutex::new(SharedState {
        next_user_id: 1,
        users: HashMap::new(),
        docs: HashMap::new(),
        storage: Storage::new(data_dir),
    }));

    let (broadcast_tx, _) = broadcast::channel::<ServerMessage>(256);

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

async fn handle_connection(
    stream: TcpStream,
    state: Arc<Mutex<SharedState>>,
    broadcast_tx: broadcast::Sender<ServerMessage>,
    mut broadcast_rx: broadcast::Receiver<ServerMessage>,
) -> Result<(), Box<dyn Error>> {
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();

    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<ServerMessage>();

    let mut current_user_id: Option<usize> = None;
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

                let msg: ClientMessage = match serde_json::from_str(&line) {
                    Ok(msg) => msg,
                    Err(_) => continue,
                };

                match msg {
                    ClientMessage::Join { user, room, doc } => {
                        let mut guard = state.lock().await;
                        let user_id = guard.next_user_id;
                        guard.next_user_id += 1;

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

                        let user_state = UserState {
                            id: user_id,
                            name: user.clone(),
                            room: room.clone(),
                            doc: doc.clone(),
                        };
                        guard.users.insert(user_id, user_state);

                        let users = users_in_doc(&guard.users, &room, &doc);
                        let welcome = ServerMessage::Welcome {
                            user_id,
                            room: room.clone(),
                            doc: doc.clone(),
                            text: doc_text,
                            version: doc_version,
                            users: users.clone(),
                        };

                        current_user_id = Some(user_id);
                        current_room = Some(room.clone());
                        current_doc = Some(doc.clone());

                        let _ = out_tx.send(welcome);
                        drop(guard);

                        let _ = broadcast_tx.send(ServerMessage::Presence {
                            room,
                            doc,
                            users,
                        });
                    }
                    ClientMessage::Insert { pos, text } => {
                        handle_op(
                            &state,
                            &broadcast_tx,
                            current_user_id,
                            current_room.as_deref(),
                            current_doc.as_deref(),
                            Op::Insert { pos, text },
                        ).await;
                    }
                    ClientMessage::Delete { pos, len } => {
                        handle_op(
                            &state,
                            &broadcast_tx,
                            current_user_id,
                            current_room.as_deref(),
                            current_doc.as_deref(),
                            Op::Delete { pos, len },
                        ).await;
                    }
                    ClientMessage::Cursor { pos } => {
                        handle_op(
                            &state,
                            &broadcast_tx,
                            current_user_id,
                            current_room.as_deref(),
                            current_doc.as_deref(),
                            Op::Cursor { pos },
                        ).await;
                    }
                    ClientMessage::SyncRequest => {
                        if let (Some(room), Some(doc)) = (current_room.as_deref(), current_doc.as_deref()) {
                            let guard = state.lock().await;
                            let doc_key = doc_key(room, doc);
                            if let Some(doc_state) = guard.docs.get(&doc_key) {
                                let _ = out_tx.send(ServerMessage::SyncResponse {
                                    room: room.to_string(),
                                    doc: doc.to_string(),
                                    text: doc_state.doc.get_text(),
                                    version: doc_state.version,
                                });
                            }
                        }
                    }
                    ClientMessage::Ping => {}
                }
            }
            event = broadcast_rx.recv() => {
                if let Ok(event) = event {
                    if should_forward(&event, current_room.as_deref(), current_doc.as_deref()) {
                        let _ = out_tx.send(event);
                    }
                }
            }
        }
    }

    if let Some(user_id) = current_user_id {
        let mut guard = state.lock().await;
        guard.users.remove(&user_id);
        if let (Some(room), Some(doc)) = (current_room.take(), current_doc.take()) {
            let users = users_in_doc(&guard.users, &room, &doc);
            let _ = broadcast_tx.send(ServerMessage::Presence { room, doc, users });
        }
    }

    writer_task.abort();
    Ok(())
}

async fn handle_op(
    state: &Arc<Mutex<SharedState>>,
    broadcast_tx: &broadcast::Sender<ServerMessage>,
    current_user_id: Option<usize>,
    room: Option<&str>,
    doc: Option<&str>,
    op: Op,
) {
    let Some(user_id) = current_user_id else { return; };
    let Some(room) = room else { return; };
    let Some(doc) = doc else { return; };

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

    let (updated_text, version) = {
        let doc_state = guard.docs.get_mut(&doc_key).expect("doc exists");
        apply_op_to_doc(doc_state, user_id, &op);
        doc_state.version += 1;
        (doc_state.doc.get_text(), doc_state.version)
    };

    let _ = guard.storage.save_text(room, doc, &updated_text);
    drop(guard);

    let _ = broadcast_tx.send(ServerMessage::Applied {
        user_id,
        room: room.to_string(),
        doc: doc.to_string(),
        op,
        version,
    });
}

fn should_forward(msg: &ServerMessage, room: Option<&str>, doc: Option<&str>) -> bool {
    let Some(room) = room else { return false; };
    let Some(doc) = doc else { return false; };
    match msg {
        ServerMessage::Welcome { .. } | ServerMessage::Error { .. } => true,
        ServerMessage::Applied { room: r, doc: d, .. } => r == room && d == doc,
        ServerMessage::Presence { room: r, doc: d, .. } => r == room && d == doc,
        ServerMessage::SyncResponse { room: r, doc: d, .. } => r == room && d == doc,
    }
}

fn users_in_doc(users: &HashMap<usize, UserState>, room: &str, doc: &str) -> Vec<UserInfo> {
    users
        .values()
        .filter(|u| u.room == room && u.doc == doc)
        .map(|u| UserInfo {
            id: u.id,
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

fn apply_op_to_doc(doc_state: &mut DocState, user_id: usize, op: &Op) {
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
            doc_state.cursors.insert(user_id, clamped);
        }
    }
}
