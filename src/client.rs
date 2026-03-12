use crate::protocol::{
    decode_sync_response,
    decode_update,
    doc_id_from_scoped_user_id,
    encode_sync_request,
    encode_update,
    make_scoped_user_id,
    Op,
};
use mdcs_sdk::Message;
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

    let (out_tx, mut out_rx) = mpsc::channel::<Message>(64);

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

    let doc_id = format!("{}/{}", room, doc);
    let raw_user_id = format!("{}-{}", user, unique_suffix());
    let scoped_user_id = make_scoped_user_id(&doc_id, &raw_user_id);
    let replica_id = scoped_user_id.clone();
    let mut doc_state = TextDoc::new(doc_id.clone(), replica_id.clone());
    let mut local_user_id: Option<String> = Some(replica_id.clone());

    out_tx
        .send(Message::Hello {
            replica_id: scoped_user_id.clone(),
            user_name: user.to_string(),
        })
        .await?;
    out_tx
        .send(encode_sync_request(&doc_id, 0))
        .await?;

    println!("[client] joined room '{}' doc '{}'", room, doc);
    println!("[client] type /help for commands");

    let mut server_lines = BufReader::new(reader).lines();
    let mut stdin_lines = BufReader::new(tokio::io::stdin()).lines();

    let mut version = 0u64;
    let mut users: HashMap<String, String> = HashMap::new();
    let mut cursors: HashMap<String, usize> = HashMap::new();

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

                let msg: Message = match serde_json::from_str(&line) {
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
                    if out_tx.send(encode_sync_request(&doc_id, version)).await.is_err() {
                        println!("[client] failed to send sync request");
                        break;
                    }
                    continue;
                }

                if let Some(op) = parse_command(&input) {
                    apply_local_op(&mut doc_state, &op);
                    let msg = encode_update(&doc_id, local_user_id.as_deref().unwrap_or(""), op, version);
                    if out_tx.send(msg).await.is_err() {
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
    msg: &Message,
    doc_id: &str,
    replica_id: &str,
    doc_state: &mut TextDoc,
    version: &mut u64,
    local_user_id: &mut Option<String>,
    users: &mut HashMap<String, String>,
    cursors: &mut HashMap<String, usize>,
) {
    match msg {
        Message::Hello {
            replica_id,
            user_name,
        } => {
            if doc_id_from_scoped_user_id(replica_id) != Some(doc_id) {
                return;
            }
            users.insert(replica_id.clone(), user_name.clone());
            println!("[client] user online: {}", user_name);
        }
        Message::Update { .. } => {
            if let Some((update_doc_id, payload, server_version)) = decode_update(msg) {
                if update_doc_id != doc_id {
                    return;
                }
                if Some(payload.user_id.clone()) != *local_user_id {
                    apply_op_to_doc(doc_state, &payload.op);
                }
                *version = server_version;
            }
        }
        Message::Presence {
            user_id,
            document_id,
            cursor_pos,
        } => {
            if document_id != doc_id {
                return;
            }
            match cursor_pos {
                Some(pos) => {
                    cursors.insert(user_id.clone(), *pos);
                }
                None => {
                    cursors.remove(user_id);
                    users.remove(user_id);
                }
            }
        }
        Message::SyncResponse { .. } => {
            if let Some((sync_doc_id, payload, server_version)) = decode_sync_response(msg) {
                if sync_doc_id != doc_id {
                    return;
                }
                *doc_state = build_doc(doc_id, replica_id, &payload.text);
                *version = server_version;
                cursors.clear();
                users.clear();
                for user in payload.users {
                    users.insert(user.id, user.name);
                }
                *local_user_id = Some(replica_id.to_string());
                println!("[client] sync complete (v{})", version);
                print_document(&doc_state.get_text());
            }
        }
        Message::Ack { .. }
        | Message::Ping
        | Message::Pong
        | Message::SyncRequest { .. } => {}
    }
}

fn parse_command(input: &str) -> Option<Op> {
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

fn parse_insert(rest: &str) -> Option<Op> {
    let mut parts = rest.splitn(2, ' ');
    let pos = parts.next()?.parse::<usize>().ok()?;
    let text = parts.next().unwrap_or("").to_string();
    Some(Op::Insert { pos, text })
}

fn parse_delete(rest: &str) -> Option<Op> {
    let mut parts = rest.split_whitespace();
    let pos = parts.next()?.parse::<usize>().ok()?;
    let len = parts.next()?.parse::<usize>().ok()?;
    Some(Op::Delete { pos, len })
}

fn parse_cursor(rest: &str) -> Option<Op> {
    let pos = rest.trim().parse::<usize>().ok()?;
    Some(Op::Cursor { pos })
}

fn handle_local_command(
    input: &str,
    text: &str,
    users: &HashMap<String, String>,
    cursors: &HashMap<String, usize>,
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

fn apply_local_op(doc: &mut TextDoc, op: &Op) {
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
