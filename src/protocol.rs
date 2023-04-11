use std::net::SocketAddr;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum ServerMessage {
    Allocation {
        /// Id of this allocation
        id: uuid::Uuid,
        /// The endpoint that the players should use to connect to the server via proxy
        allocated_endpoint: String,
        /// HTTP root for assets downloading
        assets_root: String,
        /// The endpoint that the players could try to connect to the server directly
        external_endpoint: SocketAddr,
    },
    PlayerConnected {
        player_id: String,
    },
    RequestAsset {
        key: String,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum ServerStreamHeader {
    /// Player opened a stream to the server
    PlayerStreamOpened { player_id: String },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum ClientMessage {
    AllocateEndpoint { project_id: String },
}

#[derive(Clone, Deserialize, Serialize)]
pub enum ClientStreamHeader {
    /// Open a stream to selected player
    OpenPlayerStream { player_id: String },

    /// Store asset
    StoreAsset { key: String, data: Vec<u8> },
}

impl std::fmt::Debug for ClientStreamHeader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClientStreamHeader::OpenPlayerStream { player_id } => {
                write!(f, "OpenPlayerStream {{ player_id: {} }}", player_id)
            }
            ClientStreamHeader::StoreAsset { key, data } => {
                write!(f, "StoreAsset {{ key: {}, data: {}b }}", key, data.len())
            }
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct DatagramInfo {
    pub player_id: String,
}
