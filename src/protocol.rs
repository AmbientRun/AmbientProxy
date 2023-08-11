use std::net::SocketAddr;

use serde::{Deserialize, Serialize};

pub const NO_COMPRESSION: &str = "";
pub const GZIP_COMPRESSION: &str = "gzip";

/// Messages sent from the proxy to the Ambient server
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

/// Headers for streams sent from proxy to the Ambient server
#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum ServerStreamHeader {
    /// Player opened a stream to the server
    PlayerStreamOpened { player_id: String },
}

/// Messages sent from the Ambient server to the proxy
#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum ClientMessage {
    AllocateEndpoint { project_id: String },
}

/// Headers for streams sent from the Ambient server to the proxy
#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum ClientStreamHeader {
    /// Open a stream to selected player
    OpenPlayerStream { player_id: String },

    /// Store asset
    StoreAsset {
        key: String,
        length: u32,
        compression: String,
    },
}

/// Header for datagrams sent between the Ambient server and the proxy (both ways)
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct DatagramInfo {
    /// Recipient/origin of the datagram
    pub player_id: String,
}

/// Stats returned from /stats/ALLOCATION_ID endpoint
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ProxyStats {
    /// The endpoint that the players should use to connect to the server via proxy
    pub allocated_endpoint: String,
    /// Total size of all assets
    pub total_assets_size: usize,
    /// Total number of assets
    pub total_assets_count: usize,
    /// Number of players currently connected via this proxy
    pub players_count: usize,
}
