use std::net::SocketAddr;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum ServerMessage {
    Allocation {
        id: uuid::Uuid,
        endpoint: SocketAddr,
    },
    PlayerConnected {
        player_id: String,
    },
    RequestAsset {
        key: String,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum ClientMessage {
    StoreAsset { key: String, data: Vec<u8> },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct StreamInfo {
    pub player_id: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct DatagramInfo {
    pub player_id: String,
}
