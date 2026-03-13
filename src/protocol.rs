use mdcs_sdk::Message;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Op {
    Insert { pos: usize, text: String },
    Delete { pos: usize, len: usize },
    Cursor { pos: usize },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireUpdate {
    pub user_id: String,
    pub op: Op,
    pub delta: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireSync {
    pub text: String,
    pub users: Vec<WireUser>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireUser {
    pub id: String,
    pub name: String,
}

pub fn encode_update(
    document_id: &str,
    user_id: &str,
    op: Op,
    delta: Vec<u8>,
    version: u64,
) -> Result<Message, serde_json::Error> {
    let payload = WireUpdate {
        user_id: user_id.to_string(),
        op,
        delta,
    };
    let delta = serde_json::to_vec(&payload)?;
    Ok(Message::Update {
        document_id: document_id.to_string(),
        delta,
        version,
    })
}

pub fn decode_update(msg: &Message) -> Option<(String, WireUpdate, u64)> {
    match msg {
        Message::Update {
            document_id,
            delta,
            version,
        } => {
            let payload: WireUpdate = serde_json::from_slice(delta).ok()?;
            Some((document_id.clone(), payload, *version))
        }
        _ => None,
    }
}

pub fn encode_sync_request(document_id: &str, version: u64) -> Message {
    Message::SyncRequest {
        document_id: document_id.to_string(),
        version,
    }
}

pub fn encode_sync_response(
    document_id: &str,
    text: &str,
    users: Vec<WireUser>,
    version: u64,
) -> Result<Message, serde_json::Error> {
    let payload = WireSync {
        text: text.to_string(),
        users,
    };
    let delta = serde_json::to_vec(&payload)?;
    Ok(Message::SyncResponse {
        document_id: document_id.to_string(),
        deltas: vec![delta],
        version,
    })
}

pub fn decode_sync_response(msg: &Message) -> Option<(String, WireSync, u64)> {
    match msg {
        Message::SyncResponse {
            document_id,
            deltas,
            version,
        } => {
            let delta = deltas.first()?;
            let payload: WireSync = serde_json::from_slice(delta).ok()?;
            Some((document_id.clone(), payload, *version))
        }
        _ => None,
    }
}

pub fn make_scoped_user_id(document_id: &str, user_id: &str) -> String {
    format!("{}|{}", document_id, user_id)
}

pub fn doc_id_from_scoped_user_id(scoped_id: &str) -> Option<&str> {
    scoped_id.split_once('|').map(|(doc_id, _)| doc_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_update() {
        let msg = encode_update(
            "room/doc.txt",
            "room/doc.txt|user-1",
            Op::Insert {
                pos: 1,
                text: "hi".to_string(),
            },
            vec![1, 2, 3],
            5,
        )
        .expect("encode");
        let (doc_id, payload, version) = decode_update(&msg).expect("decode");
        assert_eq!(doc_id, "room/doc.txt");
        assert_eq!(version, 5);
        assert_eq!(payload.user_id, "room/doc.txt|user-1");
        assert_eq!(payload.delta, vec![1, 2, 3]);
        match payload.op {
            Op::Insert { pos, text } => {
                assert_eq!(pos, 1);
                assert_eq!(text, "hi");
            }
            _ => panic!("wrong op"),
        }
    }

    #[test]
    fn roundtrip_sync_response() {
        let users = vec![WireUser {
            id: "room/doc.txt|user-1".to_string(),
            name: "Alice".to_string(),
        }];
        let msg = encode_sync_response("room/doc.txt", "hello", users, 2).expect("encode");
        let (doc_id, payload, version) = decode_sync_response(&msg).expect("decode");
        assert_eq!(doc_id, "room/doc.txt");
        assert_eq!(version, 2);
        assert_eq!(payload.text, "hello");
        assert_eq!(payload.users.len(), 1);
        assert_eq!(payload.users[0].name, "Alice");
    }
}
