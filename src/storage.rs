use std::fs;
use std::io;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Storage {
    data_dir: PathBuf,
}

impl Storage {
    pub fn new<P: AsRef<Path>>(data_dir: P) -> Self {
        Self {
            data_dir: data_dir.as_ref().to_path_buf(),
        }
    }

    pub fn load_text(&self, room: &str, doc: &str) -> io::Result<String> {
        let path = self.doc_path(room, doc);
        match fs::read_to_string(&path) {
            Ok(text) => Ok(text),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(String::new()),
            Err(err) => Err(err),
        }
    }

    pub fn save_text(&self, room: &str, doc: &str, text: &str) -> io::Result<()> {
        let path = self.doc_path(room, doc);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, text)
    }

    fn doc_path(&self, room: &str, doc: &str) -> PathBuf {
        let safe_room = sanitize_component(room);
        let safe_doc = sanitize_component(doc);
        self.data_dir.join(safe_room).join(safe_doc)
    }
}

fn sanitize_component(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' || ch == '.' {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        "untitled".to_string()
    } else {
        out
    }
}

