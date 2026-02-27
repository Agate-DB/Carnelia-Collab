use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserInfo {
    pub id: usize,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ClientMessage {
    Join { user: String, room: String, doc: String },
    Insert { pos: usize, text: String },
    Delete { pos: usize, len: usize },
    Cursor { pos: usize },
    SyncRequest,
    Ping,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ServerMessage {
    Welcome {
        user_id: usize,
        room: String,
        doc: String,
        text: String,
        version: u64,
        users: Vec<UserInfo>,
    },
    Applied {
        user_id: usize,
        room: String,
        doc: String,
        op: Op,
        version: u64,
    },
    Presence {
        room: String,
        doc: String,
        users: Vec<UserInfo>,
    },
    SyncResponse {
        room: String,
        doc: String,
        text: String,
        version: u64,
    },
    Error { message: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Op {
    Insert { pos: usize, text: String },
    Delete { pos: usize, len: usize },
    Cursor { pos: usize },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_client_message() {
        let msg = ClientMessage::Insert {
            pos: 2,
            text: "hi".to_string(),
        };
        let json = serde_json::to_string(&msg).expect("serialize");
        let parsed: ClientMessage = serde_json::from_str(&json).expect("deserialize");
        match parsed {
            ClientMessage::Insert { pos, text } => {
                assert_eq!(pos, 2);
                assert_eq!(text, "hi");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn roundtrip_server_message() {
        let msg = ServerMessage::Presence {
            room: "room".to_string(),
            doc: "doc.txt".to_string(),
            users: vec![UserInfo {
                id: 1,
                name: "Alice".to_string(),
            }],
        };
        let json = serde_json::to_string(&msg).expect("serialize");
        let parsed: ServerMessage = serde_json::from_str(&json).expect("deserialize");
        match parsed {
            ServerMessage::Presence { room, doc, users } => {
                assert_eq!(room, "room");
                assert_eq!(doc, "doc.txt");
                assert_eq!(users.len(), 1);
                assert_eq!(users[0].name, "Alice");
            }
            _ => panic!("wrong variant"),
        }
    }
}
