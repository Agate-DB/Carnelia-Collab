use crate::protocol::{ClientMessage, Op, ServerMessage};
use crossterm::cursor::{Hide, MoveTo, Show};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::terminal::{self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::{execute, queue};
use crossterm::style::{Attribute, Color, SetAttribute, SetBackgroundColor, SetForegroundColor};
use mdcs_sdk::TextDoc;
use std::collections::HashMap;
use std::error::Error;
use std::io::{stdout, Write};
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
        execute!(stdout(), EnterAlternateScreen, Hide)?;
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

    let doc_id = format!("{}/{}", room, doc);
    let replica_id = format!("{}-{}", user, unique_suffix());
    let mut doc_state = TextDoc::new(doc_id.clone(), replica_id.clone());
    let mut local_user_id: Option<usize> = None;
    let mut version = 0u64;
    let mut users_count = 0usize;
    let mut cursor_byte = 0usize;
    let mut scroll = 0usize;
    let mut status_msg = String::new();
    let mut users: HashMap<usize, String> = HashMap::new();
    let mut cursors: HashMap<usize, usize> = HashMap::new();

    render(
        addr,
        room,
        doc,
        &doc_state.get_text(),
        cursor_byte,
        users_count,
        version,
        &status_msg,
        &mut scroll,
        &cursors,
        &users,
        local_user_id,
    )?;

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
                    let msg: ServerMessage = match serde_json::from_str(&line) {
                        Ok(msg) => msg,
                        Err(_) => continue,
                    };

                    match msg {
                        ServerMessage::Welcome { user_id, text, version: server_version, users: server_users, .. } => {
                            doc_state = build_doc(&doc_id, &replica_id, &text);
                            version = server_version;
                            local_user_id = Some(user_id);
                            users.clear();
                            cursors.clear();
                            for user in server_users {
                                users.insert(user.id, user.name.clone());
                            }
                            users_count = users.len();
                            cursor_byte = cursor_byte.min(text.len());
                            status_msg = format!("connected as {}", user_id);
                            dirty = true;
                        }
                        ServerMessage::Applied { user_id, op, version: server_version, .. } => {
                            if Some(user_id) != local_user_id {
                                match op {
                                    Op::Cursor { pos } => {
                                        cursors.insert(user_id, pos);
                                    }
                                    _ => {
                                        adjust_cursor_for_remote(&op, &mut cursor_byte);
                                        apply_op_to_doc(&mut doc_state, &op);
                                    }
                                }
                            }
                            version = server_version;
                            cursor_byte = cursor_byte.min(doc_state.get_text().len());
                            dirty = true;
                        }
                        ServerMessage::Presence { users: server_users, .. } => {
                            users.clear();
                            for user in server_users {
                                users.insert(user.id, user.name.clone());
                            }
                            users_count = users.len();
                            cursors.retain(|id, _| users.contains_key(id));
                            dirty = true;
                        }
                        ServerMessage::SyncResponse { text, version: server_version, .. } => {
                            doc_state = build_doc(&doc_id, &replica_id, &text);
                            version = server_version;
                            cursor_byte = cursor_byte.min(text.len());
                            status_msg = "sync complete".to_string();
                            dirty = true;
                        }
                        ServerMessage::Error { message } => {
                            status_msg = message;
                            dirty = true;
                        }
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
                        if handle_key(
                            key,
                            &mut doc_state,
                            &mut cursor_byte,
                            &out_tx,
                            &mut status_msg,
                        ) {
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
            render(
                addr,
                room,
                doc,
                &doc_state.get_text(),
                cursor_byte,
                users_count,
                version,
                &status_msg,
                &mut scroll,
                &cursors,
                &users,
                local_user_id,
            )?;
        }

        if should_exit {
            break;
        }
    }

    writer_task.abort();
    Ok(())
}

fn handle_key(
    key: KeyEvent,
    doc_state: &mut TextDoc,
    cursor_byte: &mut usize,
    out_tx: &mpsc::UnboundedSender<ClientMessage>,
    status_msg: &mut String,
) -> bool {
    if key.code == KeyCode::Esc {
        return true;
    }
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('q') {
        return true;
    }

    let text = doc_state.get_text();

    match key.code {
        KeyCode::Left => {
            *cursor_byte = prev_char_boundary(&text, *cursor_byte);
            let _ = out_tx.send(ClientMessage::Cursor { pos: *cursor_byte });
            true
        }
        KeyCode::Right => {
            *cursor_byte = next_char_boundary(&text, *cursor_byte);
            let _ = out_tx.send(ClientMessage::Cursor { pos: *cursor_byte });
            true
        }
        KeyCode::Up => {
            *cursor_byte = move_cursor_vertical(&text, *cursor_byte, -1);
            let _ = out_tx.send(ClientMessage::Cursor { pos: *cursor_byte });
            true
        }
        KeyCode::Down => {
            *cursor_byte = move_cursor_vertical(&text, *cursor_byte, 1);
            let _ = out_tx.send(ClientMessage::Cursor { pos: *cursor_byte });
            true
        }
        KeyCode::Home => {
            *cursor_byte = line_start(&text, *cursor_byte);
            let _ = out_tx.send(ClientMessage::Cursor { pos: *cursor_byte });
            true
        }
        KeyCode::End => {
            *cursor_byte = line_end(&text, *cursor_byte);
            let _ = out_tx.send(ClientMessage::Cursor { pos: *cursor_byte });
            true
        }
        KeyCode::Backspace => {
            if *cursor_byte > 0 {
                let start = prev_char_boundary(&text, *cursor_byte);
                let len = *cursor_byte - start;
                apply_delete(doc_state, start, len);
                *cursor_byte = start;
                let _ = out_tx.send(ClientMessage::Delete { pos: start, len });
                let _ = out_tx.send(ClientMessage::Cursor { pos: *cursor_byte });
            }
            true
        }
        KeyCode::Delete => {
            if *cursor_byte < text.len() {
                let end = next_char_boundary(&text, *cursor_byte);
                let len = end - *cursor_byte;
                if len > 0 {
                    apply_delete(doc_state, *cursor_byte, len);
                    let _ = out_tx.send(ClientMessage::Delete {
                        pos: *cursor_byte,
                        len,
                    });
                    let _ = out_tx.send(ClientMessage::Cursor { pos: *cursor_byte });
                }
            }
            true
        }
        KeyCode::Enter => {
            let insert = "\n".to_string();
            apply_insert(doc_state, *cursor_byte, &insert);
            let _ = out_tx.send(ClientMessage::Insert {
                pos: *cursor_byte,
                text: insert,
            });
            *cursor_byte += 1;
            let _ = out_tx.send(ClientMessage::Cursor { pos: *cursor_byte });
            true
        }
        KeyCode::Char('r') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            let _ = out_tx.send(ClientMessage::SyncRequest);
            status_msg.clear();
            status_msg.push_str("sync requested");
            true
        }
        KeyCode::Char(ch) => {
            if key.modifiers.contains(KeyModifiers::CONTROL) {
                return false;
            }
            let insert = ch.to_string();
            let insert_len = insert.len();
            apply_insert(doc_state, *cursor_byte, &insert);
            let _ = out_tx.send(ClientMessage::Insert {
                pos: *cursor_byte,
                text: insert,
            });
            *cursor_byte += insert_len;
            let _ = out_tx.send(ClientMessage::Cursor { pos: *cursor_byte });
            true
        }
        _ => false,
    }
}

fn render(
    addr: &str,
    room: &str,
    doc: &str,
    text: &str,
    cursor_byte: usize,
    users_count: usize,
    version: u64,
    status_msg: &str,
    scroll: &mut usize,
    cursors: &HashMap<usize, usize>,
    users: &HashMap<usize, String>,
    local_user_id: Option<usize>,
) -> Result<(), Box<dyn Error>> {
    let mut out = stdout();
    let (cols, rows) = terminal::size()?;
    let content_height = rows.saturating_sub(1) as usize;

    let (cursor_line, cursor_col) = cursor_line_col(text, cursor_byte);
    if cursor_line < *scroll {
        *scroll = cursor_line;
    } else if cursor_line >= *scroll + content_height {
        *scroll = cursor_line + 1 - content_height;
    }

    queue!(out, MoveTo(0, 0), Clear(ClearType::All))?;

    let lines: Vec<&str> = text.split('\n').collect();
    let start = (*scroll).min(lines.len());
    let end = (start + content_height).min(lines.len());

    for (row, line) in lines[start..end].iter().enumerate() {
        let clipped = clip_line(line, cols as usize);
        queue!(out, MoveTo(0, row as u16))?;
        out.write_all(clipped.as_bytes())?;
    }

    render_local_cursor(
        &mut out,
        text,
        scroll,
        content_height,
        cols as usize,
        cursor_byte,
    )?;

    render_remote_cursors(
        &mut out,
        text,
        scroll,
        content_height,
        cols as usize,
        cursors,
        local_user_id,
    )?;

    let cursor_summary = build_cursor_summary(cursors, users, local_user_id, 3);
    let status = format!(
        "{} | room={} doc={} users={} v={} pos={} | {} | Ctrl+Q quit | Ctrl+R sync {}",
        addr,
        room,
        doc,
        users_count,
        version,
        cursor_byte,
        if cursor_summary.is_empty() { "cursors: -" } else { &cursor_summary },
        if status_msg.is_empty() { "" } else { "|" }
    );
    let status_line = if status_msg.is_empty() {
        status
    } else {
        format!("{} {}", status, status_msg)
    };

    queue!(out, MoveTo(0, rows.saturating_sub(1)))?;
    queue!(out, Clear(ClearType::CurrentLine))?;
    let clipped_status = clip_line(&status_line, cols as usize);
    out.write_all(clipped_status.as_bytes())?;

    let cursor_row = cursor_line.saturating_sub(*scroll);
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
    if end < start {
        start
    } else {
        end
    }
}

fn move_cursor_vertical(text: &str, cursor_byte: usize, direction: i32) -> usize {
    let starts = line_start_positions(text);
    let (line_idx, col) = cursor_line_col(text, cursor_byte);
    let target_line = if direction < 0 {
        if line_idx == 0 { return cursor_byte; }
        line_idx - 1
    } else {
        if line_idx + 1 >= starts.len() { return cursor_byte; }
        line_idx + 1
    };
    let (start, end) = line_range(text, &starts, target_line);
    let line_text = &text[start..end];
    let mut byte_offset = 0usize;
    let mut count = 0usize;
    for ch in line_text.chars() {
        if count >= col {
            break;
        }
        byte_offset += ch.len_utf8();
        count += 1;
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
    cursors: &HashMap<usize, usize>,
    local_user_id: Option<usize>,
) -> Result<(), Box<dyn Error>> {
    for (user_id, pos) in cursors {
        if Some(*user_id) == local_user_id {
            continue;
        }
        let (line, col) = cursor_line_col(text, *pos);
        if line < *scroll || line >= *scroll + content_height {
            continue;
        }
        let row = (line - *scroll) as u16;
        let col = col.min(cols.saturating_sub(1)) as u16;
        let cell = char_at(text, *pos).unwrap_or(' ');
        let color = color_for_user(*user_id);
        queue!(out, MoveTo(col, row), SetBackgroundColor(color), SetForegroundColor(Color::Black))?;
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
    let cell = char_at(text, cursor_byte).unwrap_or(' ');
    queue!(out, MoveTo(col, row), SetBackgroundColor(Color::White), SetForegroundColor(Color::Black))?;
    out.write_all(cell.to_string().as_bytes())?;
    queue!(out, SetAttribute(Attribute::Reset))?;
    Ok(())
}

fn build_cursor_summary(
    cursors: &HashMap<usize, usize>,
    users: &HashMap<usize, String>,
    local_user_id: Option<usize>,
    limit: usize,
) -> String {
    let mut entries: Vec<(usize, usize, String)> = cursors
        .iter()
        .filter(|(id, _)| Some(**id) != local_user_id)
        .map(|(id, pos)| {
            let name = users.get(id).cloned().unwrap_or_else(|| format!("user{}", id));
            (*id, *pos, name)
        })
        .collect();
    entries.sort_by_key(|(id, _, _)| *id);

    let mut parts = Vec::new();
    for (_, pos, name) in entries.into_iter().take(limit) {
        parts.push(format!("{}@{}", name, pos));
    }

    if parts.is_empty() {
        return String::new();
    }

    format!("cursors: {}", parts.join(", "))
}

fn color_for_user(user_id: usize) -> Color {
    const PALETTE: [Color; 6] = [
        Color::Cyan,
        Color::Magenta,
        Color::Yellow,
        Color::Green,
        Color::Blue,
        Color::Red,
    ];
    PALETTE[user_id % PALETTE.len()]
}

fn char_at(text: &str, pos: usize) -> Option<char> {
    let pos = clamp_to_boundary(text, pos);
    if pos >= text.len() {
        return None;
    }
    text[pos..].chars().next()
}
