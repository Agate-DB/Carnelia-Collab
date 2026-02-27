use crate::protocol::{ClientMessage, Op, ServerMessage};
use mdcs_sdk::TextDoc;
use std::collections::HashMap;
use std::error::Error;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

pub async fn run(addr: &str, user: &str, room: &str, doc: &str) -> Result<(), Box<dyn Error>> {
    println!("[client] connecting to {}", addr);
    let stream = TcpStream::connect(addr).await?;
    let (reader, writer) = stream.into_split();

    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<ClientMessage>();

    let writer_task = tokio::spawn(async move {
        let mut writer = writer;
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

    out_tx.send(ClientMessage::Join {
        user: user.to_string(),
        room: room.to_string(),
        doc: doc.to_string(),
    })?;

    println!("[client] joined room '{}' doc '{}'", room, doc);
    println!("[client] type /help for commands");

    let mut server_lines = BufReader::new(reader).lines();
    let mut stdin_lines = BufReader::new(tokio::io::stdin()).lines();

    let doc_id = format!("{}/{}", room, doc);
    let replica_id = format!("{}-{}", user, unique_suffix());
    let mut doc_state = TextDoc::new(doc_id.clone(), replica_id.clone());
    let mut local_user_id: Option<usize> = None;

    let mut version = 0u64;
    let mut users: HashMap<usize, String> = HashMap::new();
    let mut cursors: HashMap<usize, usize> = HashMap::new();

    loop {
        tokio::select! {
            line = server_lines.next_line() => {
                let line = match line {
                    Ok(Some(line)) => line,
                    Ok(None) => {
                        println!("[client] server closed connection");
                        break;
                    }
                    Err(err) => {
                        println!("[client] read error: {}", err);
                        break;
                    }
                };

                let msg: ServerMessage = match serde_json::from_str(&line) {
                    Ok(msg) => msg,
                    Err(_) => continue,
                };

                apply_server_message(
                    &msg,
                    &doc_id,
                    &replica_id,
                    &mut doc_state,
                    &mut version,
                    &mut local_user_id,
                    &mut users,
                    &mut cursors,
                );
            }
            input = stdin_lines.next_line() => {
                let input = match input {
                    Ok(Some(line)) => line,
                    Ok(None) => break,
                    Err(err) => {
                        println!("[client] stdin error: {}", err);
                        break;
                    }
                };

                let current_text = doc_state.get_text();
                if handle_local_command(&input, &current_text, &users, &cursors) {
                    continue;
                }

                if input.trim().eq_ignore_ascii_case("/quit") {
                    break;
                }

                if input.trim().eq_ignore_ascii_case("/sync") {
                    let _ = out_tx.send(ClientMessage::SyncRequest);
                    continue;
                }

                if let Some(msg) = parse_command(&input) {
                    apply_local_op(&mut doc_state, &msg);
                    if out_tx.send(msg).is_err() {
                        println!("[client] failed to send message");
                        break;
                    }
                } else if !input.trim().is_empty() {
                    println!("[client] unknown command, try /help");
                }
            }
        }
    }

    writer_task.abort();
    Ok(())
}

fn apply_server_message(
    msg: &ServerMessage,
    doc_id: &str,
    replica_id: &str,
    doc_state: &mut TextDoc,
    version: &mut u64,
    local_user_id: &mut Option<usize>,
    users: &mut HashMap<usize, String>,
    cursors: &mut HashMap<usize, usize>,
) {
    match msg {
        ServerMessage::Welcome {
            user_id,
            text: server_text,
            version: server_version,
            users: server_users,
            ..
        } => {
            *doc_state = build_doc(doc_id, replica_id, server_text);
            *version = *server_version;
            *local_user_id = Some(*user_id);
            users.clear();
            cursors.clear();
            for user in server_users {
                users.insert(user.id, user.name.clone());
            }
            println!("[client] welcome user_id={} version={}", user_id, version);
            print_document(&doc_state.get_text());
        }
        ServerMessage::Applied {
            user_id,
            op,
            version: server_version,
            ..
        } => {
            if Some(*user_id) != *local_user_id {
                apply_op_to_doc(doc_state, op);
            }
            *version = *server_version;
            println!("[client] applied op from user {} (v{})", user_id, version);
        }
        ServerMessage::Presence { users: server_users, .. } => {
            users.clear();
            cursors.clear();
            for user in server_users {
                users.insert(user.id, user.name.clone());
            }
            println!("[client] users online: {}", users.len());
        }
        ServerMessage::SyncResponse { text, version: server_version, .. } => {
            *doc_state = build_doc(doc_id, replica_id, text);
            *version = *server_version;
            cursors.clear();
            println!("[client] sync complete (v{})", version);
        }
        ServerMessage::Error { message } => {
            println!("[client] error: {}", message);
        }
    }
}

fn parse_command(input: &str) -> Option<ClientMessage> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }

    if let Some(rest) = trimmed.strip_prefix("/insert ") {
        return parse_insert(rest);
    }
    if let Some(rest) = trimmed.strip_prefix("/delete ") {
        return parse_delete(rest);
    }
    if let Some(rest) = trimmed.strip_prefix("/cursor ") {
        return parse_cursor(rest);
    }
    if let Some(rest) = trimmed.strip_prefix("i ") {
        return parse_insert(rest);
    }
    if let Some(rest) = trimmed.strip_prefix("d ") {
        return parse_delete(rest);
    }
    if let Some(rest) = trimmed.strip_prefix("c ") {
        return parse_cursor(rest);
    }

    None
}

fn parse_insert(rest: &str) -> Option<ClientMessage> {
    let mut parts = rest.splitn(2, ' ');
    let pos = parts.next()?.parse::<usize>().ok()?;
    let text = parts.next().unwrap_or("").to_string();
    Some(ClientMessage::Insert { pos, text })
}

fn parse_delete(rest: &str) -> Option<ClientMessage> {
    let mut parts = rest.split_whitespace();
    let pos = parts.next()?.parse::<usize>().ok()?;
    let len = parts.next()?.parse::<usize>().ok()?;
    Some(ClientMessage::Delete { pos, len })
}

fn parse_cursor(rest: &str) -> Option<ClientMessage> {
    let pos = rest.trim().parse::<usize>().ok()?;
    Some(ClientMessage::Cursor { pos })
}

fn handle_local_command(
    input: &str,
    text: &str,
    users: &HashMap<usize, String>,
    cursors: &HashMap<usize, usize>,
) -> bool {
    let trimmed = input.trim();
    if trimmed.eq_ignore_ascii_case("/help") {
        print_help();
        return true;
    }
    if trimmed.eq_ignore_ascii_case("/show") {
        print_document(text);
        return true;
    }
    if trimmed.eq_ignore_ascii_case("/users") {
        println!("[client] users:");
        for (id, name) in users {
            println!("  {}: {}", id, name);
        }
        return true;
    }
    if trimmed.eq_ignore_ascii_case("/cursors") {
        println!("[client] cursors:");
        for (id, pos) in cursors {
            let name = users.get(id).map(String::as_str).unwrap_or("unknown");
            println!("  {} ({}): {}", id, name, pos);
        }
        return true;
    }
    false
}

fn print_help() {
    println!("Commands:");
    println!("  /insert <pos> <text>   (or: i <pos> <text>)");
    println!("  /delete <pos> <len>    (or: d <pos> <len>)");
    println!("  /cursor <pos>          (or: c <pos>)");
    println!("  /sync");
    println!("  /show");
    println!("  /users");
    println!("  /cursors");
    println!("  /quit");
}

fn print_document(text: &str) {
    println!("[doc] {} bytes", text.len());
    for (idx, line) in text.lines().enumerate() {
        println!("{:>4} | {}", idx + 1, line);
    }
}

fn build_doc(doc_id: &str, replica_id: &str, text: &str) -> TextDoc {
    let mut doc = TextDoc::new(doc_id, replica_id);
    if !text.is_empty() {
        doc.insert(0, text);
    }
    doc
}

fn apply_local_op(doc: &mut TextDoc, msg: &ClientMessage) {
    match msg {
        ClientMessage::Insert { pos, text } => {
            let current = doc.get_text();
            let char_pos = byte_to_char_index(&current, *pos);
            doc.insert(char_pos, text);
        }
        ClientMessage::Delete { pos, len } => {
            let current = doc.get_text();
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
                doc.delete(char_start, char_len);
            }
        }
        ClientMessage::Cursor { .. } | ClientMessage::Join { .. } | ClientMessage::SyncRequest | ClientMessage::Ping => {}
    }
}

fn apply_op_to_doc(doc: &mut TextDoc, op: &Op) {
    match op {
        Op::Insert { pos, text } => {
            let current = doc.get_text();
            let char_pos = byte_to_char_index(&current, *pos);
            doc.insert(char_pos, text);
        }
        Op::Delete { pos, len } => {
            let current = doc.get_text();
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
                doc.delete(char_start, char_len);
            }
        }
        Op::Cursor { .. } => {}
    }
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

fn unique_suffix() -> u128 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}
