use std::{
    collections::{hash_map::DefaultHasher, HashMap},
    fmt::Debug,
    hash::{Hash, Hasher},
    io::Write,
    net::{IpAddr, SocketAddr, TcpListener},
    ops::RangeInclusive,
    sync::{Arc, Weak},
    time::Duration,
};

use anyhow::anyhow;
use axum::{
    extract::{Path, State},
    http::{Method, StatusCode},
    routing::get,
    Json, Router,
};
use bytes::Bytes;
use flate2::write::GzDecoder;
use flume::{Receiver, Sender};
use futures::StreamExt;
use parking_lot::RwLock;
use prometheus_client::registry::Registry;
use quinn::{
    Connecting, Connection, Endpoint, RecvStream, SendStream, ServerConfig, TransportConfig, VarInt,
};
use rustls::{Certificate, PrivateKey};
use tokio::{
    sync::Notify,
    task::JoinHandle,
    time::{interval, MissedTickBehavior},
};
use tower_http::cors::CorsLayer;

use crate::{
    bytes::{drop_prefix, prefix, to_binary_prefix},
    configuration::Settings,
    friendly_id::friendly_id,
    protocol::{
        ClientMessage, ClientStreamHeader, DatagramInfo, ProxyStats, ServerMessage,
        ServerStreamHeader, GZIP_COMPRESSION,
    },
    streams::{read_framed, spawn_stream_copy, write_framed, IncomingStream, OutgoingStream},
    telemetry::{AssetRequestResult, ConnectionRole, Metrics},
};

const IDLE_TIMEOUT: Duration = Duration::from_secs(5);
const ASSET_SIZE_LIMIT: u32 = 128 * 1024 * 1024;

struct AppState {
    registry: Registry,
    metrics: Metrics,
    proxies: ProxyStore,
    config: Settings,
}

impl AppState {
    fn with_settings(config: Settings) -> Self {
        let mut registry = Registry::with_prefix("proxy");
        let metrics = Metrics::new_with_registery(&mut registry);
        Self {
            registry,
            metrics,
            proxies: Default::default(),
            config,
        }
    }
}

#[derive(Default)]
struct ProxyStore {
    proxies: RwLock<HashMap<uuid::Uuid, (JoinHandle<()>, Arc<ProxyServer>)>>,
}

impl ProxyStore {
    fn insert(&self, id: uuid::Uuid, proxy: Arc<ProxyServer>, handle: JoinHandle<()>) -> usize {
        let mut map = self.proxies.write();
        map.insert(id, (handle, proxy));
        map.len()
    }

    fn get_proxy(&self, id: &uuid::Uuid) -> Option<Arc<ProxyServer>> {
        self.proxies.read().get(id).map(|(_, proxy)| proxy).cloned()
    }

    fn remove(&self, id: &uuid::Uuid) -> (usize, Option<Arc<ProxyServer>>) {
        let mut map = self.proxies.write();
        let proxy = map.remove(id).map(|(handle, proxy)| {
            handle.abort();
            proxy
        });
        (map.len(), proxy)
    }

    fn fold_proxies<B, F>(&self, init: B, f: F) -> B
    where
        F: FnMut(B, Arc<ProxyServer>) -> B,
    {
        self.proxies
            .read()
            .iter()
            .map(|(_, (_, proxy))| proxy.clone())
            .fold(init, f)
    }
}

pub struct ManagementServer {
    /// Management server endpoint
    endpoint: Endpoint,

    /// HTTP interface listener (ping, assets, etc.)
    http_listener: TcpListener,

    /// Address to bind to
    bind_addr: IpAddr,

    /// Public address of the HTTP interface, hostname:port (to advertise to clients)
    http_public_addr: String,

    /// Certificate chain
    certificate_chain: Vec<Certificate>,

    /// Private key
    private_key: PrivateKey,

    app_state: Arc<AppState>,
}

impl ManagementServer {
    pub fn new(config: Settings) -> crate::Result<Self> {
        let bind_addr = config.get_bind_addr();
        let server_addr = SocketAddr::from((bind_addr, config.management_port));
        let http_addr = SocketAddr::from((bind_addr, config.get_http_port()));

        let certificate_chain = config.load_certificate_chain()?;
        let private_key = config.load_private_key()?;

        let mut tls_config = rustls::ServerConfig::builder()
            .with_safe_default_cipher_suites()
            .with_safe_default_kx_groups()
            .with_protocol_versions(&[&rustls::version::TLS13])
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(certificate_chain.clone(), private_key.clone())?;
        tls_config.max_early_data_size = u32::MAX;
        tls_config.alpn_protocols = vec![b"ambient-proxy-03".to_vec()];

        let mut server_conf = ServerConfig::with_crypto(Arc::new(tls_config));
        let mut transport = TransportConfig::default();
        transport.max_idle_timeout(Some(IDLE_TIMEOUT.try_into().expect("Should fit in VarInt")));
        server_conf.transport = Arc::new(transport);

        let endpoint = Endpoint::server(server_conf, server_addr)?;
        let http_public_host_name = config.get_http_public_host_name();
        let http_port = config.get_http_port();
        Ok(Self {
            endpoint,
            http_listener: TcpListener::bind(http_addr)?,
            bind_addr,
            http_public_addr: format!("{http_public_host_name}:{http_port}"),
            certificate_chain,
            private_key,
            app_state: Arc::new(AppState::with_settings(config)),
        })
    }

    pub async fn start(self) {
        let Self {
            endpoint,
            http_listener,
            bind_addr,
            http_public_addr,
            certificate_chain,
            private_key,
            app_state,
        } = self;

        Self::start_http_interface(http_listener, app_state.clone());

        let (tx, rx) = flume::unbounded();

        let mut metrics_update_interval = interval(Duration::from_secs_f32(10.));
        metrics_update_interval.set_missed_tick_behavior(MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                Some(conn) = endpoint.accept() => {
                    match Self::handle_connection(
                        conn,
                        bind_addr,
                        app_state.clone(),
                        http_public_addr.clone(),
                        certificate_chain.clone(),
                        private_key.clone(),
                        tx.clone(),
                    )
                    .await
                    {
                        Ok((id, handle, proxy_server)) => {
                            let proxies_count = app_state.proxies.insert(id, proxy_server, handle);
                            app_state.metrics.inc_connections(ConnectionRole::Server);
                            app_state.metrics.set_current_connections(ConnectionRole::Server, proxies_count);
                        }
                        Err(err) => {
                            tracing::error!("Failed handling the connection: {:?}", err);
                        }
                    }
                }
                Ok(event) = rx.recv_async() => {
                    match event {
                        ProxyEvent::Stopped { id } => {
                            let (proxies_count, _) = app_state.proxies.remove(&id);
                            app_state.metrics.set_current_connections(ConnectionRole::Server, proxies_count);
                        }
                    }
                }
                _ = metrics_update_interval.tick() => {
                    let client_connections_count = app_state.proxies.fold_proxies(0, |count, proxy| {
                        count + proxy.player_conns.read().len()
                    });
                    app_state.metrics.set_current_connections(ConnectionRole::Client, client_connections_count);

                    let assets_size = app_state.proxies.fold_proxies(0, |size, proxy| {
                        size + proxy.total_assets_size()
                    });
                    app_state.metrics.set_current_assets_size_bytes(assets_size);
                }
            }
        }
    }

    fn start_http_interface(listener: TcpListener, app_state: Arc<AppState>) {
        tracing::debug!(
            "Starting HTTP interface on: {:?}",
            listener
                .local_addr()
                .expect("Listener should have a local address")
        );
        let router = Router::new()
            .route("/ping", get(|| async move { "ok" }))
            .route("/content/:id/*path", get(get_asset))
            .route("/metrics", get(get_metrics))
            .route("/stats/:id", get(get_stats))
            .with_state(app_state)
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
        app_state: Arc<AppState>,
        http_public_addr: String,
        certificate_chain: Vec<Certificate>,
        private_key: PrivateKey,
        event_tx: flume::Sender<ProxyEvent>,
    ) -> crate::Result<(uuid::Uuid, JoinHandle<()>, Arc<ProxyServer>)> {
        tracing::info!("Got a new connection from: {:?}", conn.remote_address());
        let conn = conn.await?;
        let allocation_id = uuid::Uuid::new_v4();
        let proxy_server = Arc::new(
            ProxyServer::new(
                allocation_id,
                conn,
                app_state.config.get_http_public_host_name(),
                http_public_addr,
                bind_addr,
                app_state.config.proxy_port_range(),
                certificate_chain,
                private_key,
                event_tx,
                app_state,
            )
            .await?,
        );

        // start the proxy server
        let handle = {
            let proxy_server = proxy_server.clone();
            let weak_proxy_server = Arc::downgrade(&proxy_server);
            tokio::spawn(async move {
                proxy_server.start(weak_proxy_server).await;
            })
        };

        Ok((allocation_id, handle, proxy_server))
    }
}

#[derive(Clone, Debug)]
enum ProxyEvent {
    Stopped { id: uuid::Uuid },
}

struct ProxyServer {
    id: uuid::Uuid,
    proxy_endpoint: RwLock<Option<Endpoint>>,
    allocated_endpoint: RwLock<String>,
    public_host_name: String,
    http_public_addr: String,
    bind_addr: IpAddr,
    ports: RangeInclusive<u16>,
    certificate_chain: Vec<Certificate>,
    private_key: PrivateKey,

    ambient_server_conn: Connection,
    server_message_sender: Sender<ServerMessage>,
    client_message_receiver: Receiver<ClientMessage>,
    message_processing_handle: JoinHandle<()>,
    message_processing_started: Arc<RwLock<bool>>,
    message_processing_started_notify: Arc<Notify>,
    event_tx: flume::Sender<ProxyEvent>,

    player_conns: RwLock<HashMap<String, (JoinHandle<()>, Arc<PlayerConnection>)>>,
    asset_store: RwLock<HashMap<String, Asset>>,
    metrics: Metrics,
}

impl ProxyServer {
    const BIND_ATTEMPTS: i32 = 10;

    fn create_proxy_endpoint(
        addr: IpAddr,
        ports: RangeInclusive<u16>,
        mut allocation_seed: impl Hasher,
        cert_chain: Vec<Certificate>,
        cert_key: PrivateKey,
    ) -> anyhow::Result<Endpoint> {
        let mut tls_config = rustls::ServerConfig::builder()
            .with_safe_default_cipher_suites()
            .with_safe_default_kx_groups()
            .with_protocol_versions(&[&rustls::version::TLS13])
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(cert_chain, cert_key)?;
        tls_config.max_early_data_size = u32::MAX;
        tls_config.alpn_protocols = vec![b"ambient-02".to_vec()];

        let mut server_conf = ServerConfig::with_crypto(Arc::new(tls_config));
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
        certificate_chain: Vec<Certificate>,
        private_key: PrivateKey,
        event_tx: flume::Sender<ProxyEvent>,
        app_state: Arc<AppState>,
    ) -> anyhow::Result<Self> {
        // create communication channels
        let (client_message_sender, client_message_receiver) = flume::unbounded::<ClientMessage>();
        let (server_message_sender, server_message_receiver) = flume::unbounded::<ServerMessage>();

        // FIXME: move message processing into start
        let message_processing_started = Arc::new(RwLock::new(false));
        let message_processing_started_notify = Arc::new(Notify::new());

        let message_processing_handle = {
            let ambient_server_conn = ambient_server_conn.clone();
            let message_processing_started = message_processing_started.clone();
            let message_processing_started_notify = message_processing_started_notify.clone();
            tokio::spawn(async move {
                // accept a bi stream from the newly connected server
                tracing::debug!("Accepting a bi stream from the game server");
                let Ok((send_stream, recv_stream)) = ambient_server_conn.accept_bi().await else {
                    tracing::error!("Failed to accept a bi stream from the game server");
                    return;
                };
                let mut tx = OutgoingStream::new(send_stream);
                let mut rx = IncomingStream::new(recv_stream);

                tracing::debug!("Starting message processing");
                *message_processing_started.write() = true;
                message_processing_started_notify.notify_waiters();
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
                        err = ambient_server_conn.closed() => {
                            tracing::info!("Server connection closed: {:?}", err);
                            break;
                        }
                    }
                }
            })
        };

        Ok(Self {
            id,
            proxy_endpoint: Default::default(),
            allocated_endpoint: Default::default(),
            public_host_name,
            http_public_addr,
            bind_addr,
            ports,
            certificate_chain,
            private_key,
            ambient_server_conn,
            server_message_sender,
            client_message_receiver,
            message_processing_handle,
            message_processing_started,
            message_processing_started_notify,
            event_tx,
            player_conns: Default::default(),
            asset_store: Default::default(),
            metrics: app_state.metrics.clone(),
        })
    }

    async fn start(&self, weak_self: Weak<Self>) {
        let mut client_message_receiver = self.client_message_receiver.stream();
        let (player_event_tx, player_event_rx) = flume::unbounded();

        // wait for message processing
        // FIXME: move message processing into here
        loop {
            if *self.message_processing_started.read() {
                break;
            }
            self.message_processing_started_notify.notified().await;
        }

        loop {
            tokio::select! {
                // new player connection
                Some(conn) = self.accept_connection() => {
                    match self.handle_connection(conn, player_event_tx.clone()).await {
                        Ok((player_id, handle, connection)) => {
                            self.player_conns.write().insert(player_id, (handle, connection));
                            self.metrics.inc_connections(ConnectionRole::Client);
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
                    }
                }

                // server opening a new uni stream to a player
                Ok(recv_stream) = self.ambient_server_conn.accept_uni() => {
                    // handling this might involve receiving assets (we don't want to block other processing)
                    if let Err(err) = self.handle_uni(weak_self.clone(), recv_stream).await {
                        tracing::error!("Failed to handle uni stream: {:?}", err);
                    }
                }

                // server opening a new uni stream to a player
                Ok((send_stream, recv_stream)) = self.ambient_server_conn.accept_bi() => {
                    if let Err(err) = self.handle_bi(send_stream, recv_stream).await {
                        tracing::error!("Failed to handle bi stream: {:?}", err);
                    }
                }

                // server sending a datagram to a player
                Ok(datagram) = self.ambient_server_conn.read_datagram() => {
                    if let Err(err) = self.handle_datagram(datagram).await {
                        tracing::error!("Failed to handle datagram: {:?}", err);
                    }
                }

                Ok(player_event) = player_event_rx.recv_async() => {
                    match player_event {
                        PlayerEvent::Disconnected { player_id } => {
                            if let Some((handle, _)) = self.player_conns.write().remove(&player_id) {
                                handle.abort();
                            }
                        }
                    }
                }

                // ambient server connection closed
                err = self.ambient_server_conn.closed() => {
                    tracing::info!("Server connection closed: {:?}", err);
                    if let Err(err) = self.event_tx.send_async(ProxyEvent::Stopped { id: self.id }).await {
                        tracing::error!("Failed to send proxy stopped event: {:?}", err);
                    }
                    break;
                }

                else => {
                    tracing::info!("Proxy server is shutting down");
                    if let Err(err) = self.event_tx.send_async(ProxyEvent::Stopped { id: self.id }).await {
                        tracing::error!("Failed to send proxy stopped event: {:?}", err);
                    }
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
        let Ok(endpoint) = Self::create_proxy_endpoint(self.bind_addr, self.ports.clone(), hasher, self.certificate_chain.clone(), self.private_key.clone()) else {
            return Err(anyhow::anyhow!("Failed to create proxy endpoint"));
        };

        *self.proxy_endpoint.write() = Some(endpoint.clone());
        let Ok(local_addr) = endpoint.local_addr() else {
            return Err(anyhow::anyhow!("Failed to get local address"));
        };
        let allocated_endpoint = format!("{}:{}", self.public_host_name, local_addr.port());
        *self.allocated_endpoint.write() = allocated_endpoint.clone();

        Ok(ServerMessage::Allocation {
            id: self.id,
            allocated_endpoint,
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
        event_tx: flume::Sender<PlayerEvent>,
    ) -> crate::Result<(String, JoinHandle<()>, Arc<PlayerConnection>)> {
        tracing::info!(
            "Got a new player connection from: {:?}",
            conn.remote_address()
        );
        let conn = conn.await?;

        // assign random id to player
        let player_id = friendly_id();
        tracing::info!("Generated id for connected player: {}", player_id);

        // notify the server
        self.server_message_sender
            .send_async(ServerMessage::PlayerConnected {
                player_id: player_id.clone(),
            })
            .await
            .map_err(anyhow::Error::from)?;

        // create player connection to handle player side actions
        let player_connection = Arc::new(
            PlayerConnection::new(
                self.ambient_server_conn.clone(),
                player_id.clone(),
                conn,
                event_tx,
            )
            .await?,
        );

        // start handling player connection
        let handle = {
            let player_connection = player_connection.clone();
            tokio::spawn(async move {
                player_connection.start().await;
            })
        };
        Ok((player_id, handle, player_connection))
    }

    fn get_player_connection(&self, player_id: &str) -> Option<Connection> {
        self.player_conns
            .read()
            .get(player_id)
            .map(|(_, p)| p.get_connection())
    }

    async fn handle_uni(
        &self,
        proxy_server: Weak<ProxyServer>,
        mut recv_stream: RecvStream,
    ) -> crate::Result<()> {
        // hand decode the first message so then we can just copy the streams without decoding in case of proxied stream
        let header: ClientStreamHeader = read_framed(&mut recv_stream, 1024).await?;
        tracing::debug!(
            "Got a new uni stream from the game server with header: {:?}",
            header
        );

        match header {
            ClientStreamHeader::OpenPlayerStream { player_id } => {
                // get player connection
                let Some(player_connection) = self.get_player_connection(&player_id) else {
                    return Err(anyhow!("Unknown player: {}", player_id))?;
                };

                // open a uni stream to the player and copy the recv stream there
                let send_stream = player_connection.open_uni().await?;
                spawn_stream_copy(recv_stream, send_stream);
                Ok(())
            }
            ClientStreamHeader::StoreAsset {
                key,
                length,
                compression,
            } => {
                // store the asset in the asset store
                tracing::debug!(
                    "Storing asset (receiving data): {} {}B",
                    key,
                    to_binary_prefix(length)
                );

                if !compression.is_empty() && compression != GZIP_COMPRESSION {
                    return Err(anyhow!("Unsupported asset compression: {:?}", compression))?;
                }

                if length > ASSET_SIZE_LIMIT {
                    return Err(anyhow!(
                        "Asset too large: {} > {}",
                        length,
                        ASSET_SIZE_LIMIT
                    ))?;
                }

                // read the asset data in a separate task
                tokio::spawn(async move {
                    let Ok(mut data) = recv_stream.read_to_end(length as usize).await else {
                        tracing::warn!("Failed to read asset data");
                        return;
                    };

                    // get the proxy server that is handling this uni stream
                    let Some(proxy_server) = proxy_server.upgrade() else {
                        tracing::debug!("Proxy server has been dropped");
                        return;
                    };

                    // decompress if needed
                    if compression == GZIP_COMPRESSION {
                        let mut decoder = GzDecoder::new(Vec::new());
                        if let Err(err) = decoder.write_all(&data) {
                            tracing::warn!("Failed to decompress asset data: {}", err);
                            return;
                        }
                        let Ok(mut decompressed) = decoder.finish() else {
                            tracing::warn!("Failed to decompress asset data");
                            return;
                        };
                        std::mem::swap(&mut data, &mut decompressed);
                    }

                    let size = data.len();

                    proxy_server
                        .asset_store
                        .write()
                        .entry(key.clone())
                        .or_default()
                        .store(data);

                    if compression.is_empty() {
                        tracing::debug!("Stored asset: {} {}B", key, to_binary_prefix(length));
                    } else {
                        tracing::debug!(
                            "Stored asset: {} {}B ({}B after decompression)",
                            key,
                            to_binary_prefix(length),
                            to_binary_prefix(size as u64),
                        );
                    }
                });
                Ok(())
            }
        }
    }

    async fn handle_bi(
        &self,
        send_stream: SendStream,
        mut recv_stream: RecvStream,
    ) -> crate::Result<()> {
        // hand decode the first message so then we can just copy the streams without decoding
        let ClientStreamHeader::OpenPlayerStream { player_id } = read_framed(&mut recv_stream, 1024).await? else {
            return Err(anyhow!("Unexpected header"))?;
        };

        // get player connection
        let Some(player_connection) = self.get_player_connection(&player_id) else {
            return Err(anyhow!("Unknown player: {}", player_id))?;
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

    fn stats(&self) -> ProxyStats {
        ProxyStats {
            allocated_endpoint: self.allocated_endpoint.read().clone(),
            total_assets_size: self.total_assets_size(),
            total_assets_count: self.asset_store.read().len(),
            players_count: self.player_conns.read().len(),
        }
    }

    fn total_assets_size(&self) -> usize {
        self.asset_store
            .read()
            .values()
            .map(|asset| asset.data.as_ref().map(|data| data.len()).unwrap_or(0))
            .sum()
    }
}

impl Drop for ProxyServer {
    fn drop(&mut self) {
        tracing::info!("Shutting down proxy server: {}", self.id);

        // abort the message processing task
        self.message_processing_handle.abort();

        // close the proxy endpoint
        if let Some(proxy_endpoint) = self.proxy_endpoint.write().take() {
            proxy_endpoint.close(VarInt::from_u32(0), b"");
        }
    }
}

#[derive(Clone, Debug)]
enum PlayerEvent {
    Disconnected { player_id: String },
}

struct PlayerConnection {
    ambient_server_conn: Connection,
    player_id: String,
    conn: Connection,
    event_tx: flume::Sender<PlayerEvent>,
}

impl PlayerConnection {
    async fn new(
        ambient_server_conn: Connection,
        player_id: String,
        conn: Connection,
        event_tx: flume::Sender<PlayerEvent>,
    ) -> crate::Result<Self> {
        Ok(Self {
            ambient_server_conn,
            player_id,
            conn,
            event_tx,
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

                err = self.ambient_server_conn.closed() => {
                    tracing::info!("Ambient server connection closed: {:?}", err);
                    // no-op - ProxyServer will handle this
                    break;
                }

                err = self.conn.closed() => {
                    tracing::info!("Player connection closed: {:?}", err);
                    if let Err(err) = self.event_tx.send_async(PlayerEvent::Disconnected {
                        player_id: self.player_id.clone(),
                    }).await {
                        tracing::error!("Failed to send player disconnected event: {:?}", err);
                    }
                    break;
                }

                // player connection closed
                else => {
                    tracing::info!("Player connection closed");
                    if let Err(err) = self.event_tx.send_async(PlayerEvent::Disconnected {
                        player_id: self.player_id.clone(),
                    }).await {
                        tracing::error!("Failed to send player disconnected event: {:?}", err);
                    }
                    break;
                }
            }
        }
    }

    async fn handle_uni(&self, recv_stream: RecvStream) -> crate::Result<()> {
        let mut send_stream = self.ambient_server_conn.open_uni().await?;
        write_framed(
            &mut send_stream,
            &ServerStreamHeader::PlayerStreamOpened {
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
            &ServerStreamHeader::PlayerStreamOpened {
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

impl Drop for PlayerConnection {
    fn drop(&mut self) {
        tracing::info!("Shutting down player connection: {}", self.player_id);

        // close the player connection
        self.conn.close(VarInt::from_u32(0), b"");
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

#[tracing::instrument(skip(app_state))]
async fn get_asset(
    Path((id, path)): Path<(String, String)>,
    State(app_state): State<Arc<AppState>>,
) -> Result<Bytes, StatusCode> {
    tracing::debug!("get_asset");
    let Ok(allocation_id) = uuid::Uuid::try_from(id.as_str()) else {
        tracing::warn!("Invalid allocation id: {}", id);
        app_state.metrics.inc_asset_requests(AssetRequestResult::Malformed);
        return Err(StatusCode::BAD_REQUEST);
    };
    let Some(proxy_server) = app_state.proxies.get_proxy(&allocation_id) else {
        tracing::warn!("Unknown allocation id: {}", id);
        app_state.metrics.inc_asset_requests(AssetRequestResult::AllocationNotFound);
        return Err(StatusCode::NOT_FOUND);
    };
    match proxy_server
        .get_asset(path, app_state.config.get_assets_download_timeout())
        .await
    {
        Ok(None) => {
            app_state
                .metrics
                .inc_asset_requests(AssetRequestResult::AssetNotFound);
            Err(StatusCode::NOT_FOUND)
        }
        Ok(Some(data)) => {
            app_state
                .metrics
                .inc_asset_requests(AssetRequestResult::Success);
            app_state
                .metrics
                .observe_asset_response_size_bytes(data.len());
            Ok(data)
        }
        Err(err) => {
            tracing::error!("Failed to get asset: {:?}", err);
            app_state
                .metrics
                .inc_asset_requests(AssetRequestResult::Error);
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

async fn get_metrics(
    State(app_state): State<Arc<AppState>>,
) -> Result<axum::response::Response, axum::http::StatusCode> {
    use axum::response::IntoResponse;
    use prometheus_client::encoding::text::encode;

    let mut body = String::new();
    if encode(&mut body, &app_state.registry).is_err() {
        return Err(axum::http::StatusCode::INTERNAL_SERVER_ERROR);
    }
    let mut response = body.into_response();
    response.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static(
            "application/openmetrics-text; version=1.0.0; charset=utf-8",
        ),
    );
    Ok(response)
}

#[tracing::instrument(skip(app_state))]
async fn get_stats(
    Path(id): Path<String>,
    State(app_state): State<Arc<AppState>>,
) -> Result<Json<ProxyStats>, StatusCode> {
    let Ok(allocation_id) = uuid::Uuid::try_from(id.as_str()) else {
        tracing::warn!("Invalid allocation id: {}", id);
        return Err(StatusCode::BAD_REQUEST);
    };
    let Some(proxy_server) = app_state.proxies.get_proxy(&allocation_id) else {
        tracing::debug!("Unknown allocation id: {}", id);
        return Err(StatusCode::NOT_FOUND);
    };
    Ok(Json(proxy_server.stats()))
}
