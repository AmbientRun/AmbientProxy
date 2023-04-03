use std::{
    collections::{hash_map::DefaultHasher, HashMap},
    fmt::Debug,
    hash::{Hash, Hasher},
    net::{IpAddr, SocketAddr, TcpListener},
    ops::RangeInclusive,
    sync::Arc,
    time::Duration,
};

use anyhow::anyhow;
use axum::{
    extract::{Path, State},
    http::{Method, StatusCode},
    routing::get,
    Router,
};
use bytes::Bytes;
use flume::{Receiver, Sender};
use futures::StreamExt;
use parking_lot::RwLock;
use quinn::{
    Connecting, Connection, Endpoint, RecvStream, SendStream, ServerConfig, TransportConfig,
};
use rustls::{Certificate, PrivateKey};
use tokio::sync::Notify;
use tower_http::cors::CorsLayer;

use crate::{
    bytes::{drop_prefix, prefix},
    protocol::{ClientMessage, DatagramInfo, ServerMessage, StreamInfo},
    streams::{read_framed, spawn_stream_copy, write_framed, IncomingStream, OutgoingStream},
};

const ASSET_FETCH_TIMEOUT: Duration = Duration::from_secs(10);
const IDLE_TIMEOUT: Duration = Duration::from_secs(5);

const CERT: &[u8] = include_bytes!("./cert.der");
const CERT_KEY: &[u8] = include_bytes!("./cert.key.der");

#[derive(Default)]
struct ProxyStore {
    proxies: RwLock<HashMap<uuid::Uuid, Arc<ProxyServer>>>,
}

impl ProxyStore {
    fn insert(&self, id: uuid::Uuid, proxy: Arc<ProxyServer>) {
        self.proxies.write().insert(id, proxy);
    }

    fn get(&self, id: &uuid::Uuid) -> Option<Arc<ProxyServer>> {
        self.proxies.read().get(id).cloned()
    }
}

pub struct ManagementServer {
    /// Management server endpoint
    endpoint: Endpoint,

    /// HTTP interface listener (ping, assets, etc.)
    http_listener: TcpListener,

    /// Currently allocated proxies
    proxies: Arc<ProxyStore>,

    /// Address to bind to
    bind_addr: IpAddr,

    /// Range of ports to allocate for proxies
    proxy_allocation_ports: RangeInclusive<u16>,

    /// Public address of the server (to advertise to clients)
    public_host_name: String,

    /// Public address of the HTTP interface, hostname:port (to advertise to clients)
    http_public_addr: String,
}

impl ManagementServer {
    pub fn new(
        bind_addr: IpAddr,
        management_port: u16,
        http_port: u16,
        proxy_allocation_ports: RangeInclusive<u16>,
        public_host_name: String,
        http_public_host_name: String,
    ) -> crate::Result<Self> {
        let server_addr = SocketAddr::from((bind_addr, management_port));
        let http_addr = SocketAddr::from((bind_addr, http_port));

        let cert = Certificate(CERT.to_vec());
        let cert_key = PrivateKey(CERT_KEY.to_vec());
        let mut server_conf =
            ServerConfig::with_single_cert(vec![cert], cert_key).map_err(anyhow::Error::from)?;
        let mut transport = TransportConfig::default();
        transport.max_idle_timeout(Some(IDLE_TIMEOUT.try_into().expect("Should fit in VarInt")));
        server_conf.transport = Arc::new(transport);

        let endpoint = Endpoint::server(server_conf, server_addr)?;
        Ok(Self {
            endpoint,
            http_listener: TcpListener::bind(http_addr)?,
            proxies: Default::default(),
            bind_addr,
            proxy_allocation_ports,
            public_host_name,
            http_public_addr: format!("{http_public_host_name}:{http_port}"),
        })
    }

    pub async fn start(self) {
        let Self {
            endpoint,
            http_listener,
            proxies,
            bind_addr,
            proxy_allocation_ports,
            public_host_name,
            http_public_addr,
        } = self;

        Self::start_http_interface(http_listener, proxies.clone());

        while let Some(conn) = endpoint.accept().await {
            match Self::handle_connection(
                conn,
                bind_addr,
                proxy_allocation_ports.clone(),
                public_host_name.clone(),
                http_public_addr.clone(),
            )
            .await
            {
                Ok((id, proxy_server)) => {
                    proxies.insert(id, proxy_server);
                }
                Err(err) => {
                    tracing::error!("Failed handling the connection: {:?}", err);
                }
            }
        }
    }

    fn start_http_interface(listener: TcpListener, proxies: Arc<ProxyStore>) {
        tracing::debug!("Starting HTTP interface on: {:?}", listener.local_addr());
        let router = Router::new()
            .route("/ping", get(|| async move { "ok" }))
            .route("/content/:id/*path", get(get_asset))
            .with_state(proxies)
            .layer(
                CorsLayer::new()
                    .allow_origin(tower_http::cors::Any)
                    .allow_methods(vec![Method::GET])
                    .allow_headers(tower_http::cors::Any),
            );

        tokio::spawn(async move {
            axum::Server::from_tcp(listener)
                .unwrap()
                .serve(router.into_make_service())
                .await
                .unwrap();
        });
    }

    async fn handle_connection(
        conn: Connecting,
        bind_addr: IpAddr,
        ports: RangeInclusive<u16>,
        public_host_name: String,
        http_public_addr: String,
    ) -> crate::Result<(uuid::Uuid, Arc<ProxyServer>)> {
        tracing::info!("Got a new connection from: {:?}", conn.remote_address());
        let conn = conn.await?;
        let allocation_id = uuid::Uuid::new_v4();
        let proxy_server = Arc::new(
            ProxyServer::new(
                allocation_id,
                conn,
                public_host_name,
                http_public_addr,
                bind_addr,
                ports,
            )
            .await?,
        );

        // start the proxy server
        {
            let proxy_server = proxy_server.clone();
            tokio::spawn(async move {
                proxy_server.start().await;
            });
        }

        Ok((allocation_id, proxy_server))
    }
}

struct ProxyServer {
    id: uuid::Uuid,
    proxy_endpoint: RwLock<Option<Endpoint>>,
    public_host_name: String,
    http_public_addr: String,
    bind_addr: IpAddr,
    ports: RangeInclusive<u16>,
    ambient_server_conn: Connection,
    server_message_sender: Sender<ServerMessage>,
    client_message_receiver: Receiver<ClientMessage>,
    player_conns: RwLock<HashMap<String, Arc<PlayerConnection>>>,
    asset_store: RwLock<HashMap<String, Asset>>,
}

impl ProxyServer {
    const BIND_ATTEMPTS: i32 = 10;

    fn create_proxy_endpoint(
        addr: IpAddr,
        ports: RangeInclusive<u16>,
        mut allocation_seed: impl Hasher,
    ) -> anyhow::Result<Endpoint> {
        let cert = Certificate(CERT.to_vec());
        let cert_key = PrivateKey(CERT_KEY.to_vec());
        let mut server_conf = ServerConfig::with_single_cert(vec![cert], cert_key)?;
        let mut transport = TransportConfig::default();
        transport.max_idle_timeout(Some(IDLE_TIMEOUT.try_into().expect("Should fit in VarInt")));
        server_conf.transport = Arc::new(transport);

        // pick a port
        let ports_len = *ports.end() - *ports.start() + 1;
        for attempt in 0..Self::BIND_ATTEMPTS {
            attempt.hash(&mut allocation_seed);
            let port = *ports.start() + (allocation_seed.finish() as u16) % ports_len;
            debug_assert!(ports.contains(&port));
            let server_addr = SocketAddr::from((addr, port));
            if let Ok(endpoint) = Endpoint::server(server_conf.clone(), server_addr) {
                return Ok(endpoint);
            }
        }

        Err(anyhow!("Failed to bind"))
    }

    async fn new(
        id: uuid::Uuid,
        ambient_server_conn: Connection,
        public_host_name: String,
        http_public_addr: String,
        bind_addr: IpAddr,
        ports: RangeInclusive<u16>,
    ) -> anyhow::Result<Self> {
        // create communication channels
        let (client_message_sender, client_message_receiver) = flume::unbounded::<ClientMessage>();
        let (server_message_sender, server_message_receiver) = flume::unbounded::<ServerMessage>();

        {
            let ambient_server_conn = ambient_server_conn.clone();
            tokio::spawn(async move {
                // accept a bi stream from the newly connected server
                tracing::debug!("Accepting a bi stream from the game server");
                let Ok((send_stream, recv_stream)) = ambient_server_conn.accept_bi().await else {
                    tracing::error!("Failed to accept a bi stream from the game server");
                    return;
                };
                let mut tx = OutgoingStream::new(send_stream);
                let mut rx = IncomingStream::new(recv_stream);

                tracing::debug!("Starting to message processing");
                let mut receiver_stream = server_message_receiver.stream();
                loop {
                    tokio::select! {
                        Ok(message) = rx.next::<ClientMessage>() => {
                            tracing::debug!("Got message from server: {:?}", message);
                            if let Err(err) = client_message_sender.send_async(message).await {
                                tracing::error!("Error processing message from the server: {:?}", err);
                            }
                        }
                        Some(message) = receiver_stream.next() => {
                            tracing::debug!("Got message for server: {:?}", message);
                            if let Err(err) = tx.send(&message).await {
                                tracing::error!("Error sending message to the server: {:?}", err);
                            }
                        }
                    }
                }
            });
        }

        Ok(Self {
            id,
            proxy_endpoint: Default::default(),
            public_host_name,
            http_public_addr,
            bind_addr,
            ports,
            ambient_server_conn,
            server_message_sender,
            client_message_receiver,
            player_conns: Default::default(),
            asset_store: Default::default(),
        })
    }

    async fn start(&self) {
        let mut client_message_receiver = self.client_message_receiver.stream();
        loop {
            tokio::select! {
                // new player connection
                Some(conn) = self.accept_connection() => {
                    match self.handle_connection(conn).await {
                        Ok((player_id, connection)) => {
                            self.player_conns.write().insert(player_id, connection);
                        }
                        Err(err) => {
                            tracing::error!("Failed to handle incoming client connection: {:?}", err);
                        }
                    }
                }

                // server send a message to the proxy
                Some(message) = client_message_receiver.next() => {
                    match message {
                        ClientMessage::AllocateEndpoint { ref project_id }=> {
                            tracing::debug!("Got allocate endpoint message from client: {:?}", message);
                            match self.handle_allocate_message(project_id) {
                                Ok(allocated_endpoint_message) => {
                                    if let Err(err) = self.server_message_sender.send_async(allocated_endpoint_message).await {
                                        tracing::error!("Failed to send allocated endpoint message: {:?}", err);
                                    }
                                }
                                Err(err) => {
                                    tracing::error!("Failed to handle allocate message: {:?}", err);
                                }
                            }
                        }
                        ClientMessage::StoreAsset { key, data } => {
                            tracing::debug!("Got store asset message from client: {} {}b", key, data.len());
                            self.asset_store.write().entry(key).or_default().store(data);
                        }
                    }
                }

                // server opening a new uni stream to a player
                Ok(recv_stream) = self.ambient_server_conn.accept_uni() => {
                    if let Err(err) = self.handle_uni(recv_stream).await {
                        tracing::error!("Failed to handle uni stream: {:?}", err);
                    }
                }

                // server opening a new uni stream to a player
                Ok((send_stream, recv_stream)) = self.ambient_server_conn.accept_bi() => {
                    if let Err(err) = self.handle_bi(send_stream, recv_stream).await {
                        tracing::error!("Failed to handle uni stream: {:?}", err);
                    }
                }

                // server sending a datagram to a player
                Ok(datagram) = self.ambient_server_conn.read_datagram() => {
                    if let Err(err) = self.handle_datagram(datagram).await {
                        tracing::error!("Failed to handle datagram: {:?}", err);
                    }
                }

                err = self.ambient_server_conn.closed() => {
                    tracing::info!("Server connection closed: {:?}", err);
                    // TODO: dispose of ProxyServer
                    break;
                }

                else => {
                    tracing::info!("Proxy server is shutting down");
                    // TODO: dispose of ProxyServer
                    break;
                }
            }
        }
    }

    fn handle_allocate_message(
        &self,
        project_id: impl AsRef<str>,
    ) -> anyhow::Result<ServerMessage> {
        if self.proxy_endpoint.read().is_some() {
            return Err(anyhow::anyhow!("Endpoint already allocated"));
        }
        let external_endpoint = self.ambient_server_conn.remote_address();

        // base port selection on the ip and project id to favour reusing the same port
        let mut hasher = DefaultHasher::new();
        (external_endpoint.ip(), project_id.as_ref()).hash(&mut hasher);
        let Ok(endpoint) = Self::create_proxy_endpoint(self.bind_addr, self.ports.clone(), hasher) else {
            return Err(anyhow::anyhow!("Failed to create proxy endpoint"));
        };

        *self.proxy_endpoint.write() = Some(endpoint.clone());
        let Ok(local_addr) = endpoint.local_addr() else {
            return Err(anyhow::anyhow!("Failed to get local address"));
        };
        Ok(ServerMessage::Allocation {
            id: self.id,
            allocated_endpoint: format!("{}:{}", self.public_host_name, local_addr.port()),
            external_endpoint,
            assets_root: format!("http://{}/content/{}/", self.http_public_addr, self.id),
        })
    }

    async fn accept_connection(&self) -> Option<Connecting> {
        let endpoint_opt = self.proxy_endpoint.read().clone();
        if let Some(proxy_endpoint) = endpoint_opt {
            proxy_endpoint.accept().await
        } else {
            None
        }
    }

    async fn handle_connection(
        &self,
        conn: Connecting,
    ) -> crate::Result<(String, Arc<PlayerConnection>)> {
        tracing::info!(
            "Got a new player connection from: {:?}",
            conn.remote_address()
        );
        let conn = conn.await?;
        // accept the first bi stream from the player to know who they are
        let (send_stream, mut recv_stream) = conn.accept_bi().await?;
        let player_id: String = read_framed(&mut recv_stream, 1024).await?;
        tracing::info!("Connected player id: {}", player_id);

        // notify the server
        self.server_message_sender
            .send_async(ServerMessage::PlayerConnected {
                player_id: player_id.clone(),
            })
            .await
            .map_err(anyhow::Error::from)?;

        // create player connection to handle player side actions
        let player_connection = Arc::new(
            PlayerConnection::new(self.ambient_server_conn.clone(), player_id.clone(), conn)
                .await?,
        );

        // open this bi stream to the server and send the player_id again
        let (mut server_send_stream, server_recv_stream) =
            self.ambient_server_conn.open_bi().await?;
        write_framed(
            &mut server_send_stream,
            &StreamInfo {
                player_id: player_id.clone(),
            },
        )
        .await?;
        write_framed(&mut server_send_stream, &player_id).await?;
        spawn_stream_copy(recv_stream, server_send_stream);
        spawn_stream_copy(server_recv_stream, send_stream);

        // start handling player connection
        {
            let player_connection = player_connection.clone();
            tokio::spawn(async move {
                player_connection.start().await;
            });
        }
        Ok((player_id, player_connection))
    }

    fn get_player_connection(&self, player_id: &str) -> Option<Connection> {
        self.player_conns
            .read()
            .get(player_id)
            .map(|p| p.get_connection())
    }

    async fn handle_uni(&self, mut recv_stream: RecvStream) -> crate::Result<()> {
        // hand decode the first message so then we can just copy the streams without decoding
        let message: StreamInfo = read_framed(&mut recv_stream, 1024).await?;

        // get player connection
        let Some(player_connection) = self.get_player_connection(&message.player_id) else {
            return Err(anyhow!("Unknown player: {}", message.player_id))?;
        };

        // open a uni stream to the player and copy the recv stream there
        let send_stream = player_connection.open_uni().await?;
        spawn_stream_copy(recv_stream, send_stream);

        Ok(())
    }

    async fn handle_bi(
        &self,
        send_stream: SendStream,
        mut recv_stream: RecvStream,
    ) -> crate::Result<()> {
        // hand decode the first message so then we can just copy the streams without decoding
        let message: StreamInfo = read_framed(&mut recv_stream, 1024).await?;

        // get player connection
        let Some(player_connection) = self.get_player_connection(&message.player_id) else {
            return Err(anyhow!("Unknown player: {}", message.player_id))?;
        };

        // open a bi stream to the player and copy the server streams there
        let (player_send_stream, player_recv_stream) = player_connection.open_bi().await?;
        spawn_stream_copy(recv_stream, player_send_stream);
        spawn_stream_copy(player_recv_stream, send_stream);

        Ok(())
    }

    async fn handle_datagram(&self, datagram: Bytes) -> crate::Result<()> {
        let (DatagramInfo { player_id }, data) = drop_prefix(datagram)?;
        let Some(player_connection) = self.get_player_connection(&player_id) else {
            return Err(anyhow!("Unknown player: {}", player_id))?;
        };
        player_connection.send_datagram(data)?;
        Ok(())
    }

    async fn get_asset(
        &self,
        asset_id: String,
        timeout: Duration,
    ) -> anyhow::Result<Option<Bytes>> {
        let mut requested_from_server = false;
        let deadline = tokio::time::Instant::now() + timeout;

        while tokio::time::Instant::now() < deadline {
            let notify = {
                // check if we already have the asset available
                let mut map = self.asset_store.write();
                let asset = map.entry(asset_id.clone()).or_default();
                if let Some(asset_data) = asset.data.clone() {
                    return Ok(Some(asset_data));
                }

                // request the asset from the server (if not already requested)
                if !requested_from_server {
                    requested_from_server = true;
                    if self
                        .server_message_sender
                        .send(ServerMessage::RequestAsset {
                            key: asset_id.clone(),
                        })
                        .is_err()
                    {
                        return Err(anyhow::anyhow!("Failed to request asset: {}", asset_id));
                    };
                }

                // grab the notify so we can wait on it
                asset.notify.clone()
            };

            // wait for the asset to be available (or timeout)
            tokio::select! {
                _ = tokio::time::sleep_until(deadline) => {}
                _ = notify.notified() => {}
            }
        }
        Err(anyhow::anyhow!("Timed out getting asset: {}", asset_id))
    }
}

struct PlayerConnection {
    ambient_server_conn: Connection,
    player_id: String,
    conn: Connection,
}

impl PlayerConnection {
    async fn new(
        ambient_server_conn: Connection,
        player_id: String,
        conn: Connection,
    ) -> crate::Result<Self> {
        Ok(Self {
            ambient_server_conn,
            player_id,
            conn,
        })
    }

    fn get_connection(&self) -> Connection {
        self.conn.clone()
    }

    async fn start(&self) {
        loop {
            tokio::select! {
                // player opening a new uni stream to the server
                Ok(recv_stream) = self.conn.accept_uni() => {
                    if let Err(err) = self.handle_uni(recv_stream).await {
                        tracing::error!("Failed to handle uni stream: {:?}", err);
                    }
                }

                // player opening a new bi stream to the server
                Ok((send_stream, recv_stream)) = self.conn.accept_bi() => {
                    if let Err(err) = self.handle_bi(send_stream, recv_stream).await {
                        tracing::error!("Failed to handle bi stream: {:?}", err);
                    }
                }

                // player sending a datagram to the server
                Ok(datagram) = self.conn.read_datagram() => {
                    if let Err(err) = self.handle_datagram(datagram).await {
                        tracing::error!("Failed to handle datagram: {:?}", err);
                    }
                }

                err = self.conn.closed() => {
                    tracing::info!("Player connection closed: {:?}", err);
                    // TODO: remove player connection
                    break;
                }

                // player connection closed
                else => {
                    tracing::info!("Player connection closed");
                    // TODO: remove player connection
                    break;
                }
            }
        }
    }

    async fn handle_uni(&self, recv_stream: RecvStream) -> crate::Result<()> {
        let mut send_stream = self.ambient_server_conn.open_uni().await?;
        write_framed(
            &mut send_stream,
            &StreamInfo {
                player_id: self.player_id.clone(),
            },
        )
        .await?;
        spawn_stream_copy(recv_stream, send_stream);
        Ok(())
    }

    async fn handle_bi(
        &self,
        send_stream: SendStream,
        recv_stream: RecvStream,
    ) -> crate::Result<()> {
        let (mut server_send_stream, server_recv_stream) =
            self.ambient_server_conn.open_bi().await?;
        write_framed(
            &mut server_send_stream,
            &StreamInfo {
                player_id: self.player_id.clone(),
            },
        )
        .await?;
        spawn_stream_copy(recv_stream, server_send_stream);
        spawn_stream_copy(server_recv_stream, send_stream);
        Ok(())
    }

    async fn handle_datagram(&self, datagram: Bytes) -> crate::Result<()> {
        self.ambient_server_conn.send_datagram(prefix(
            &DatagramInfo {
                player_id: self.player_id.clone(),
            },
            datagram,
        )?)?;
        Ok(())
    }
}

#[derive(Clone, Debug, Default)]
struct Asset {
    // FIXME: cache negative responses?
    data: Option<Bytes>,
    notify: Arc<Notify>,
    // TODO: add timestamp and refresh periodically?
}

impl Asset {
    fn store(&mut self, data: impl Into<Bytes>) {
        self.data = Some(data.into());
        self.notify.notify_waiters();
    }
}

async fn get_asset(
    Path((id, path)): Path<(String, String)>,
    State(proxy_store): State<Arc<ProxyStore>>,
) -> Result<Bytes, StatusCode> {
    tracing::debug!("get_asset: {} {}", id, path);
    let Ok(allocation_id) = uuid::Uuid::try_from(id.as_str()) else {
        tracing::warn!("Invalid allocation id: {}", id);
        return Err(StatusCode::BAD_REQUEST);
    };
    let Some(proxy_server) = proxy_store.get(&allocation_id) else {
        tracing::warn!("Unknown allocation id: {}", id);
        return Err(StatusCode::NOT_FOUND);
    };
    match proxy_server.get_asset(path, ASSET_FETCH_TIMEOUT).await {
        Ok(None) => Err(StatusCode::NOT_FOUND),
        Ok(Some(data)) => Ok(data),
        Err(err) => {
            tracing::error!("Failed to get asset: {:?}", err);
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}
