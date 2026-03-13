use crate::protocol::{
    Op, decode_sync_response, decode_update, doc_id_from_scoped_user_id, encode_sync_request,
    encode_update, make_scoped_user_id,
};
use crossterm::cursor::{MoveTo, Show};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::style::{Attribute, Color, SetAttribute, SetBackgroundColor, SetForegroundColor};
use crossterm::terminal::{self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::{execute, queue};
use mdcs_sdk::{Awareness, Message, TextDoc};
use std::collections::HashMap;
use std::error::Error;
use std::io::{Write, stdout};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

enum UiEvent {
    Key(KeyEvent),
    Resize,
}

struct TerminalGuard;

impl TerminalGuard {
    fn new() -> Result<Self, Box<dyn Error>> {
        terminal::enable_raw_mode()?;
        execute!(stdout(), EnterAlternateScreen)?;
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = execute!(stdout(), Show, LeaveAlternateScreen);
        let _ = terminal::disable_raw_mode();
    }
}

pub async fn run(addr: &str, user: &str, room: &str, doc: &str) -> Result<(), Box<dyn Error>> {
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
    let mut doc_state = TextDoc::new(doc_id.clone(), scoped_user_id.clone());
    let local_user_id: Option<String> = Some(scoped_user_id.clone());
    let awareness = Awareness::new(scoped_user_id.clone(), user.to_string());

    out_tx
        .send(Message::Hello {
            replica_id: scoped_user_id.clone(),
            user_name: user.to_string(),
        })
        .await?;
    out_tx.send(encode_sync_request(&doc_id, 0)).await?;

    let _term = TerminalGuard::new()?;

    let (ui_tx, mut ui_rx) = mpsc::unbounded_channel::<UiEvent>();
    tokio::task::spawn_blocking(move || {
        loop {
            match event::read() {
                Ok(Event::Key(key)) => {
                    if ui_tx.send(UiEvent::Key(key)).is_err() {
                        break;
                    }
                }
                Ok(Event::Resize(_, _)) => {
                    if ui_tx.send(UiEvent::Resize).is_err() {
                        break;
                    }
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
    });

    let mut server_lines = BufReader::new(reader).lines();

    let mut version = 0u64;
    let mut users_count = 0usize;
    let mut cursor_byte = 0usize;
    let mut scroll = 0usize;
    let mut status_msg = String::new();
    let mut users: HashMap<String, String> = HashMap::new();
    let mut cursors: HashMap<String, usize> = HashMap::new();

    let mut render_ctx = RenderContext {
        addr,
        room,
        doc,
        text: &doc_state.get_text(),
        cursor_byte,
        users_count,
        version,
        status_msg: &status_msg,
        scroll: &mut scroll,
        cursors: &cursors,
        users: &users,
        local_user_id: local_user_id.as_deref(),
    };
    render(&mut render_ctx)?;

    loop {
        let mut dirty = false;
        let mut should_exit = false;
        tokio::select! {
            line = server_lines.next_line() => {
                let line = match line {
                    Ok(Some(line)) => line,
                    Ok(None) => {
                        status_msg = "server closed connection".to_string();
                        dirty = true;
                        should_exit = true;
                        String::new()
                    }
                    Err(err) => {
                        status_msg = format!("read error: {}", err);
                        dirty = true;
                        should_exit = true;
                        String::new()
                    }
                };

                if should_exit {
                    // Skip parsing when connection closed or errored.
                } else {
                    let msg: Message = match serde_json::from_str(&line) {
                        Ok(msg) => msg,
                        Err(_) => continue,
                    };

                    match msg {
                        Message::Hello { replica_id, user_name } => {
                            if doc_id_from_scoped_user_id(&replica_id) == Some(doc_id.as_str()) {
                                users.insert(replica_id, user_name);
                                users_count = users.len();
                                dirty = true;
                            }
                        }
                        Message::Update { .. } => {
                            if let Some((update_doc_id, payload, server_version)) = decode_update(&msg)
                                && update_doc_id == doc_id
                            {
                                if Some(payload.user_id.clone()) != local_user_id {
                                    // Treat `op` as the single source of truth for remote edits.
                                    // Ignore `payload.delta` to avoid double-applying changes.
                                    apply_op_to_doc(&mut doc_state, &payload.op);
                                    adjust_cursor_for_remote(&payload.op, &mut cursor_byte);
                                }
                                version = server_version;
                                cursor_byte = cursor_byte.min(doc_state.get_text().len());
                                dirty = true;
                            }
                        }
                        Message::Presence { user_id, document_id, cursor_pos } => {
                            if document_id == doc_id {
                                match cursor_pos {
                                    Some(pos) => {
                                        cursors.insert(user_id, pos);
                                    }
                                    None => {
                                        users.remove(&user_id);
                                        cursors.remove(&user_id);
                                    }
                                }
                                users_count = users.len();
                                dirty = true;
                            }
                        }
                        Message::SyncResponse { .. } => {
                            if let Some((sync_doc_id, payload, server_version)) = decode_sync_response(&msg)
                                && sync_doc_id == doc_id
                            {
                                doc_state = build_doc(&doc_id, &scoped_user_id, &payload.text);
                                version = server_version;
                                cursor_byte = cursor_byte.min(payload.text.len());
                                users.clear();
                                for user in payload.users {
                                    users.insert(user.id, user.name);
                                }
                                users_count = users.len();
                                status_msg = "sync complete".to_string();
                                dirty = true;
                            }
                        }
                        Message::Ack { .. } | Message::Ping | Message::Pong | Message::SyncRequest { .. } => {}
                    }
                }
            }
            ui_event = ui_rx.recv() => {
                let Some(ui_event) = ui_event else { break; };
                match ui_event {
                    UiEvent::Key(key) => {
                        if key.kind == KeyEventKind::Release {
                            continue;
                        }
                        let mut key_ctx = KeyContext {
                            doc_state: &mut doc_state,
                            cursor_byte: &mut cursor_byte,
                            out_tx: &out_tx,
                            doc_id: &doc_id,
                            local_user_id: local_user_id.as_deref(),
                            version,
                            awareness: &awareness,
                            status_msg: &mut status_msg,
                        };
                        if handle_key(key, &mut key_ctx) {
                            dirty = true;
                            if key.code == KeyCode::Esc || (key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('q')) {
                                should_exit = true;
                            }
                        }
                    }
                    UiEvent::Resize => {
                        dirty = true;
                    }
                }
            }
        }

        if dirty {
            let mut render_ctx = RenderContext {
                addr,
                room,
                doc,
                text: &doc_state.get_text(),
                cursor_byte,
                users_count,
                version,
                status_msg: &status_msg,
                scroll: &mut scroll,
                cursors: &cursors,
                users: &users,
                local_user_id: local_user_id.as_deref(),
            };
            render(&mut render_ctx)?;
        }

        if should_exit {
            break;
        }
    }

    writer_task.abort();
    Ok(())
}

struct KeyContext<'a> {
    doc_state: &'a mut TextDoc,
    cursor_byte: &'a mut usize,
    out_tx: &'a mpsc::Sender<Message>,
    doc_id: &'a str,
    local_user_id: Option<&'a str>,
    version: u64,
    awareness: &'a Awareness,
    status_msg: &'a mut String,
}

fn handle_key(key: KeyEvent, ctx: &mut KeyContext<'_>) -> bool {
    if key.code == KeyCode::Esc {
        return true;
    }
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('q') {
        return true;
    }

    let text = ctx.doc_state.get_text();

    match key.code {
        KeyCode::Left => {
            *ctx.cursor_byte = prev_char_boundary(&text, *ctx.cursor_byte);
            ctx.awareness.set_cursor(ctx.doc_id, *ctx.cursor_byte);
            let _ = ctx.out_tx.try_send(Message::Presence {
                user_id: ctx.local_user_id.unwrap_or("").to_string(),
                document_id: ctx.doc_id.to_string(),
                cursor_pos: Some(*ctx.cursor_byte),
            });
            true
        }
        KeyCode::Right => {
            *ctx.cursor_byte = next_char_boundary(&text, *ctx.cursor_byte);
            ctx.awareness.set_cursor(ctx.doc_id, *ctx.cursor_byte);
            let _ = ctx.out_tx.try_send(Message::Presence {
                user_id: ctx.local_user_id.unwrap_or("").to_string(),
                document_id: ctx.doc_id.to_string(),
                cursor_pos: Some(*ctx.cursor_byte),
            });
            true
        }
        KeyCode::Up => {
            *ctx.cursor_byte = move_cursor_vertical(&text, *ctx.cursor_byte, -1);
            ctx.awareness.set_cursor(ctx.doc_id, *ctx.cursor_byte);
            let _ = ctx.out_tx.try_send(Message::Presence {
                user_id: ctx.local_user_id.unwrap_or("").to_string(),
                document_id: ctx.doc_id.to_string(),
                cursor_pos: Some(*ctx.cursor_byte),
            });
            true
        }
        KeyCode::Down => {
            *ctx.cursor_byte = move_cursor_vertical(&text, *ctx.cursor_byte, 1);
            ctx.awareness.set_cursor(ctx.doc_id, *ctx.cursor_byte);
            let _ = ctx.out_tx.try_send(Message::Presence {
                user_id: ctx.local_user_id.unwrap_or("").to_string(),
                document_id: ctx.doc_id.to_string(),
                cursor_pos: Some(*ctx.cursor_byte),
            });
            true
        }
        KeyCode::Home => {
            *ctx.cursor_byte = line_start(&text, *ctx.cursor_byte);
            ctx.awareness.set_cursor(ctx.doc_id, *ctx.cursor_byte);
            let _ = ctx.out_tx.try_send(Message::Presence {
                user_id: ctx.local_user_id.unwrap_or("").to_string(),
                document_id: ctx.doc_id.to_string(),
                cursor_pos: Some(*ctx.cursor_byte),
            });
            true
        }
        KeyCode::End => {
            *ctx.cursor_byte = line_end(&text, *ctx.cursor_byte);
            ctx.awareness.set_cursor(ctx.doc_id, *ctx.cursor_byte);
            let _ = ctx.out_tx.try_send(Message::Presence {
                user_id: ctx.local_user_id.unwrap_or("").to_string(),
                document_id: ctx.doc_id.to_string(),
                cursor_pos: Some(*ctx.cursor_byte),
            });
            true
        }
        KeyCode::Backspace => {
            if *ctx.cursor_byte > 0 {
                let start = prev_char_boundary(&text, *ctx.cursor_byte);
                let len = *ctx.cursor_byte - start;
                apply_delete(ctx.doc_state, start, len);
                *ctx.cursor_byte = start;
                let delta = Vec::new();
                if let Ok(msg) = encode_update(
                    ctx.doc_id,
                    ctx.local_user_id.unwrap_or(""),
                    Op::Delete { pos: start, len },
                    delta,
                    ctx.version,
                ) {
                    let _ = ctx.out_tx.try_send(msg);
                }
                ctx.awareness.set_cursor(ctx.doc_id, *ctx.cursor_byte);
                let _ = ctx.out_tx.try_send(Message::Presence {
                    user_id: ctx.local_user_id.unwrap_or("").to_string(),
                    document_id: ctx.doc_id.to_string(),
                    cursor_pos: Some(*ctx.cursor_byte),
                });
            }
            true
        }
        KeyCode::Delete => {
            if *ctx.cursor_byte < text.len() {
                let end = next_char_boundary(&text, *ctx.cursor_byte);
                let len = end - *ctx.cursor_byte;
                if len > 0 {
                    apply_delete(ctx.doc_state, *ctx.cursor_byte, len);
                    let delta = Vec::new();
                    if let Ok(msg) = encode_update(
                        ctx.doc_id,
                        ctx.local_user_id.unwrap_or(""),
                        Op::Delete {
                            pos: *ctx.cursor_byte,
                            len,
                        },
                        delta,
                        ctx.version,
                    ) {
                        let _ = ctx.out_tx.try_send(msg);
                    }
                    ctx.awareness.set_cursor(ctx.doc_id, *ctx.cursor_byte);
                    let _ = ctx.out_tx.try_send(Message::Presence {
                        user_id: ctx.local_user_id.unwrap_or("").to_string(),
                        document_id: ctx.doc_id.to_string(),
                        cursor_pos: Some(*ctx.cursor_byte),
                    });
                }
            }
            true
        }
        KeyCode::Enter => {
            let insert = "\n".to_string();
            apply_insert(ctx.doc_state, *ctx.cursor_byte, &insert);
            let delta = Vec::new();
            if let Ok(msg) = encode_update(
                ctx.doc_id,
                ctx.local_user_id.unwrap_or(""),
                Op::Insert {
                    pos: *ctx.cursor_byte,
                    text: insert,
                },
                delta,
                ctx.version,
            ) {
                let _ = ctx.out_tx.try_send(msg);
            }
            *ctx.cursor_byte += 1;
            ctx.awareness.set_cursor(ctx.doc_id, *ctx.cursor_byte);
            let _ = ctx.out_tx.try_send(Message::Presence {
                user_id: ctx.local_user_id.unwrap_or("").to_string(),
                document_id: ctx.doc_id.to_string(),
                cursor_pos: Some(*ctx.cursor_byte),
            });
            true
        }
        KeyCode::Char('r') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            let _ = ctx
                .out_tx
                .try_send(encode_sync_request(ctx.doc_id, ctx.version));
            ctx.status_msg.clear();
            ctx.status_msg.push_str("sync requested");
            true
        }
        KeyCode::Char(ch) => {
            if key.modifiers.contains(KeyModifiers::CONTROL) {
                return false;
            }
            let insert = ch.to_string();
            let insert_len = insert.len();
            apply_insert(ctx.doc_state, *ctx.cursor_byte, &insert);
            let delta = Vec::new();
            if let Ok(msg) = encode_update(
                ctx.doc_id,
                ctx.local_user_id.unwrap_or(""),
                Op::Insert {
                    pos: *ctx.cursor_byte,
                    text: insert,
                },
                delta,
                ctx.version,
            ) {
                let _ = ctx.out_tx.try_send(msg);
            }
            *ctx.cursor_byte += insert_len;
            ctx.awareness.set_cursor(ctx.doc_id, *ctx.cursor_byte);
            let _ = ctx.out_tx.try_send(Message::Presence {
                user_id: ctx.local_user_id.unwrap_or("").to_string(),
                document_id: ctx.doc_id.to_string(),
                cursor_pos: Some(*ctx.cursor_byte),
            });
            true
        }
        _ => false,
    }
}

struct RenderContext<'a> {
    addr: &'a str,
    room: &'a str,
    doc: &'a str,
    text: &'a str,
    cursor_byte: usize,
    users_count: usize,
    version: u64,
    status_msg: &'a str,
    scroll: &'a mut usize,
    cursors: &'a HashMap<String, usize>,
    users: &'a HashMap<String, String>,
    local_user_id: Option<&'a str>,
}

fn render(ctx: &mut RenderContext<'_>) -> Result<(), Box<dyn Error>> {
    let mut out = stdout();
    let (cols, rows) = terminal::size()?;
    let content_height = rows.saturating_sub(1) as usize;

    let (cursor_line, cursor_col) = cursor_line_col(ctx.text, ctx.cursor_byte);
    if cursor_line < *ctx.scroll {
        *ctx.scroll = cursor_line;
    } else if cursor_line >= *ctx.scroll + content_height {
        *ctx.scroll = cursor_line + 1 - content_height;
    }

    queue!(out, MoveTo(0, 0), Clear(ClearType::All))?;

    let lines: Vec<&str> = ctx.text.split('\n').collect();
    let start = (*ctx.scroll).min(lines.len());
    let end = (start + content_height).min(lines.len());

    for (row, line) in lines[start..end].iter().enumerate() {
        let clipped = clip_line(line, cols as usize);
        queue!(out, MoveTo(0, row as u16))?;
        out.write_all(clipped.as_bytes())?;
    }

    render_local_cursor(
        &mut out,
        ctx.text,
        ctx.scroll,
        content_height,
        cols as usize,
        ctx.cursor_byte,
    )?;

    render_remote_cursors(
        &mut out,
        ctx.text,
        ctx.scroll,
        content_height,
        cols as usize,
        ctx.cursors,
        ctx.local_user_id,
    )?;

    let cursor_summary = build_cursor_summary(ctx.cursors, ctx.users, ctx.local_user_id, 3);
    let status = format!(
        "{} | room={} doc={} users={} v={} pos={} | {} | Ctrl+Q quit | Ctrl+R sync {}",
        ctx.addr,
        ctx.room,
        ctx.doc,
        ctx.users_count,
        ctx.version,
        ctx.cursor_byte,
        if cursor_summary.is_empty() {
            "cursors: -"
        } else {
            &cursor_summary
        },
        if ctx.status_msg.is_empty() { "" } else { "|" }
    );
    let status_line = if ctx.status_msg.is_empty() {
        status
    } else {
        format!("{} {}", status, ctx.status_msg)
    };

    queue!(out, MoveTo(0, rows.saturating_sub(1)))?;
    queue!(out, Clear(ClearType::CurrentLine))?;
    let clipped_status = clip_line(&status_line, cols as usize);
    out.write_all(clipped_status.as_bytes())?;

    let cursor_row = cursor_line.saturating_sub(*ctx.scroll);
    if cursor_row < content_height {
        let col = cursor_col.min(cols.saturating_sub(1) as usize);
        queue!(out, MoveTo(col as u16, cursor_row as u16))?;
    }

    out.flush()?;
    Ok(())
}

fn clip_line(line: &str, max_width: usize) -> String {
    if max_width == 0 {
        return String::new();
    }
    let mut out = String::new();
    for ch in line.chars().take(max_width) {
        out.push(ch);
    }
    out
}

fn cursor_line_col(text: &str, cursor_byte: usize) -> (usize, usize) {
    let cursor_byte = clamp_to_boundary(text, cursor_byte);
    let mut line = 0usize;
    let mut col = 0usize;
    for ch in text[..cursor_byte].chars() {
        if ch == '\n' {
            line += 1;
            col = 0;
        } else {
            col += 1;
        }
    }
    (line, col)
}

fn line_start_positions(text: &str) -> Vec<usize> {
    let mut starts = vec![0usize];
    for (idx, ch) in text.char_indices() {
        if ch == '\n' {
            starts.push(idx + ch.len_utf8());
        }
    }
    if starts.is_empty() {
        starts.push(0);
    }
    starts
}

fn line_range(text: &str, starts: &[usize], line_idx: usize) -> (usize, usize) {
    let start = starts.get(line_idx).copied().unwrap_or(0);
    let mut end = if line_idx + 1 < starts.len() {
        starts[line_idx + 1]
    } else {
        text.len()
    };
    if end > start && text.as_bytes()[end - 1] == b'\n' {
        end -= 1;
    }
    (start, end)
}

fn line_start(text: &str, cursor_byte: usize) -> usize {
    let starts = line_start_positions(text);
    let (line_idx, _) = cursor_line_col(text, cursor_byte);
    starts.get(line_idx).copied().unwrap_or(0)
}

fn line_end(text: &str, cursor_byte: usize) -> usize {
    let starts = line_start_positions(text);
    let (line_idx, _) = cursor_line_col(text, cursor_byte);
    let (start, end) = line_range(text, &starts, line_idx);
    if end < start { start } else { end }
}

fn move_cursor_vertical(text: &str, cursor_byte: usize, direction: i32) -> usize {
    let starts = line_start_positions(text);
    let (line_idx, col) = cursor_line_col(text, cursor_byte);
    let target_line = if direction < 0 {
        if line_idx == 0 {
            return cursor_byte;
        }
        line_idx - 1
    } else {
        if line_idx + 1 >= starts.len() {
            return cursor_byte;
        }
        line_idx + 1
    };
    let (start, end) = line_range(text, &starts, target_line);
    let line_text = &text[start..end];
    let mut byte_offset = 0usize;
    for (count, ch) in line_text.chars().enumerate() {
        if count >= col {
            break;
        }
        byte_offset += ch.len_utf8();
    }
    start + byte_offset
}

fn apply_insert(doc: &mut TextDoc, pos: usize, text: &str) {
    let current = doc.get_text();
    let char_pos = byte_to_char_index(&current, pos);
    doc.insert(char_pos, text);
}

fn apply_delete(doc: &mut TextDoc, pos: usize, len: usize) {
    let current = doc.get_text();
    if current.is_empty() {
        return;
    }
    let start = clamp_to_boundary(&current, pos);
    let end = clamp_to_boundary(&current, start.saturating_add(len));
    if start >= end {
        return;
    }
    let char_start = current[..start].chars().count();
    let char_len = current[start..end].chars().count();
    if char_len > 0 {
        doc.delete(char_start, char_len);
    }
}

fn apply_op_to_doc(doc: &mut TextDoc, op: &Op) {
    match op {
        Op::Insert { pos, text } => apply_insert(doc, *pos, text),
        Op::Delete { pos, len } => apply_delete(doc, *pos, *len),
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

fn prev_char_boundary(text: &str, pos: usize) -> usize {
    let mut pos = clamp_to_boundary(text, pos);
    if pos == 0 {
        return 0;
    }
    pos -= 1;
    while pos > 0 && !text.is_char_boundary(pos) {
        pos -= 1;
    }
    pos
}

fn next_char_boundary(text: &str, pos: usize) -> usize {
    let mut pos = clamp_to_boundary(text, pos);
    if pos >= text.len() {
        return text.len();
    }
    pos += 1;
    while pos < text.len() && !text.is_char_boundary(pos) {
        pos += 1;
    }
    pos.min(text.len())
}

fn byte_to_char_index(text: &str, byte_pos: usize) -> usize {
    let byte_pos = clamp_to_boundary(text, byte_pos);
    text[..byte_pos].chars().count()
}

fn build_doc(doc_id: &str, replica_id: &str, text: &str) -> TextDoc {
    let mut doc = TextDoc::new(doc_id, replica_id);
    if !text.is_empty() {
        doc.insert(0, text);
    }
    doc
}

fn unique_suffix() -> u128 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn adjust_cursor_for_remote(op: &Op, cursor_byte: &mut usize) {
    match op {
        Op::Insert { pos, text } => {
            if *pos <= *cursor_byte {
                *cursor_byte = cursor_byte.saturating_add(text.len());
            }
        }
        Op::Delete { pos, len } => {
            if *pos < *cursor_byte {
                let removed = (*cursor_byte - *pos).min(*len);
                *cursor_byte = cursor_byte.saturating_sub(removed);
            }
        }
        Op::Cursor { .. } => {}
    }
}

fn render_remote_cursors(
    out: &mut std::io::Stdout,
    text: &str,
    scroll: &usize,
    content_height: usize,
    cols: usize,
    cursors: &HashMap<String, usize>,
    local_user_id: Option<&str>,
) -> Result<(), Box<dyn Error>> {
    for (user_id, pos) in cursors {
        if Some(user_id.as_str()) == local_user_id {
            continue;
        }
        let (line, col) = cursor_line_col(text, *pos);
        if line < *scroll || line >= *scroll + content_height {
            continue;
        }
        let row = (line - *scroll) as u16;
        let col = col.min(cols.saturating_sub(1)) as u16;
        let cell = cursor_cell_char(text, *pos);
        let color = color_for_user(user_id);
        queue!(
            out,
            MoveTo(col, row),
            SetBackgroundColor(color),
            SetForegroundColor(Color::Black)
        )?;
        out.write_all(cell.to_string().as_bytes())?;
        queue!(out, SetAttribute(Attribute::Reset))?;
    }
    Ok(())
}

fn render_local_cursor(
    out: &mut std::io::Stdout,
    text: &str,
    scroll: &usize,
    content_height: usize,
    cols: usize,
    cursor_byte: usize,
) -> Result<(), Box<dyn Error>> {
    let (line, col) = cursor_line_col(text, cursor_byte);
    if line < *scroll || line >= *scroll + content_height {
        return Ok(());
    }
    let row = (line - *scroll) as u16;
    let col = col.min(cols.saturating_sub(1)) as u16;
    let cell = cursor_cell_char(text, cursor_byte);
    queue!(
        out,
        MoveTo(col, row),
        SetBackgroundColor(Color::White),
        SetForegroundColor(Color::Black)
    )?;
    out.write_all(cell.to_string().as_bytes())?;
    queue!(out, SetAttribute(Attribute::Reset))?;
    Ok(())
}

fn build_cursor_summary(
    cursors: &HashMap<String, usize>,
    users: &HashMap<String, String>,
    local_user_id: Option<&str>,
    limit: usize,
) -> String {
    let mut entries: Vec<(String, usize, String)> = cursors
        .iter()
        .filter(|(id, _)| Some(id.as_str()) != local_user_id)
        .map(|(id, pos)| {
            let name = users.get(id).cloned().unwrap_or_else(|| id.clone());
            (id.clone(), *pos, name)
        })
        .collect();
    entries.sort_by(|(a, _, _), (b, _, _)| a.cmp(b));

    let mut parts = Vec::new();
    for (_, pos, name) in entries.into_iter().take(limit) {
        parts.push(format!("{}@{}", name, pos));
    }

    if parts.is_empty() {
        return String::new();
    }

    format!("cursors: {}", parts.join(", "))
}

fn color_for_user(user_id: &str) -> Color {
    const PALETTE: [Color; 6] = [
        Color::Cyan,
        Color::Magenta,
        Color::Yellow,
        Color::Green,
        Color::Blue,
        Color::Red,
    ];
    let mut hash = 0u64;
    for byte in user_id.as_bytes() {
        hash = hash.wrapping_mul(31).wrapping_add(*byte as u64);
    }
    let idx = (hash as usize) % PALETTE.len();
    PALETTE[idx]
}

fn char_at(text: &str, pos: usize) -> Option<char> {
    let pos = clamp_to_boundary(text, pos);
    if pos >= text.len() {
        return None;
    }
    text[pos..].chars().next()
}

fn cursor_cell_char(text: &str, pos: usize) -> char {
    match char_at(text, pos) {
        Some('\n') | None => ' ',
        Some(ch) => ch,
    }
}
