use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr, TcpListener},
    ops::Range,
    sync::Arc,
};

use anyhow::anyhow;
use axum::{http::Method, routing::get, Router};
use bytes::Bytes;
use flume::{Receiver, Sender};
use futures::StreamExt;
use parking_lot::RwLock;
use quinn::{
    Connecting, Connection, Endpoint, RecvStream, SendStream, ServerConfig, TransportConfig,
};
use rustls::{Certificate, PrivateKey};
use tokio::{runtime::Handle, sync::Notify};
use tower_http::cors::CorsLayer;

use crate::{
    bytes::{drop_prefix, prefix},
    protocol::{ClientMessage, DatagramInfo, ServerMessage, StreamInfo},
    streams::{read_framed, spawn_stream_copy, write_framed, IncomingStream, OutgoingStream},
    Result,
};

const CERT: &[u8] = include_bytes!("./cert.der");
const CERT_KEY: &[u8] = include_bytes!("./cert.key.der");

pub struct ManagementServer {
    endpoint: Endpoint,
    proxies: RwLock<HashMap<uuid::Uuid, Arc<ProxyServer>>>,
}

impl ManagementServer {
    pub fn new(server_addr: SocketAddr) -> Result<Self> {
        let cert = Certificate(CERT.to_vec());
        let cert_key = PrivateKey(CERT_KEY.to_vec());
        let mut server_conf =
            ServerConfig::with_single_cert(vec![cert], cert_key).map_err(anyhow::Error::from)?;
        let mut transport = TransportConfig::default();
        transport.max_idle_timeout(None);
        server_conf.transport = Arc::new(transport);
        let endpoint = Endpoint::server(server_conf, server_addr)?;
        Ok(Self {
            endpoint,
            proxies: Default::default(),
        })
    }

    pub async fn start(&self) {
        while let Some(conn) = self.endpoint.accept().await {
            match self.handle_connection(conn).await {
                Ok((id, proxy_server)) => {
                    self.proxies.write().insert(id, proxy_server);
                }
                Err(err) => {
                    tracing::error!("Failed handling the connection: {:?}", err);
                }
            }
        }
    }

    async fn handle_connection(&self, conn: Connecting) -> Result<(uuid::Uuid, Arc<ProxyServer>)> {
        tracing::info!("Got a new connection from: {:?}", conn.remote_address());
        let conn = conn.await?;
        let allocation_id = uuid::Uuid::new_v4();
        // FIXME: grab IP and ports from config
        let proxy_server = Arc::new(
            ProxyServer::new(
                allocation_id,
                conn,
                IpAddr::from([0, 0, 0, 0]),
                IpAddr::from([0, 0, 0, 0]),
                9000..10000,
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
    public_addr: IpAddr,
    bind_addr: IpAddr,
    ports: Range<u16>,
    ambient_server_conn: Connection,
    server_message_sender: Sender<ServerMessage>,
    client_message_receiver: Receiver<ClientMessage>,
    player_conns: RwLock<HashMap<String, Arc<PlayerConnection>>>,
    asset_store: RwLock<HashMap<String, Asset>>,
}

impl ProxyServer {
    fn create_proxy_endpoint(addr: IpAddr, ports: Range<u16>) -> anyhow::Result<Endpoint> {
        let cert = Certificate(CERT.to_vec());
        let cert_key = PrivateKey(CERT_KEY.to_vec());
        let mut server_conf = ServerConfig::with_single_cert(vec![cert], cert_key)?;
        let mut transport = TransportConfig::default();
        transport.max_idle_timeout(None);
        server_conf.transport = Arc::new(transport);
        // FIXME: pick a few ports at random
        for port in ports {
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
        public_addr: IpAddr,
        bind_addr: IpAddr,
        ports: Range<u16>,
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
            public_addr,
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
                    tracing::debug!("Got a message from client: {:?}", message);
                    match message {
                        ClientMessage::AllocateEndpoint => {
                            match self.handle_allocate_message() {
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

                else => {
                    tracing::info!("Proxy server is shutting down");
                    break;
                }
            }
        }
    }

    fn handle_allocate_message(&self) -> anyhow::Result<ServerMessage> {
        if self.proxy_endpoint.read().is_some() {
            return Err(anyhow::anyhow!("Endpoint already allocated"));
        }
        let Ok(endpoint) = Self::create_proxy_endpoint(self.bind_addr, self.ports.clone()) else {
            return Err(anyhow::anyhow!("Failed to create proxy endpoint"));
        };
        *self.proxy_endpoint.write() = Some(endpoint.clone());
        let Ok(local_addr) = endpoint.local_addr() else {
            return Err(anyhow::anyhow!("Failed to get local address"));
        };
        let allocated_endpoint = SocketAddr::from((self.public_addr, local_addr.port()));
        let external_endpoint = self.ambient_server_conn.remote_address();
        Ok(ServerMessage::Allocation {
            id: self.id,
            allocated_endpoint,
            external_endpoint,
            // FIXME
            assets_root: format!("http://{}:8080/{}", self.public_addr, self.id),
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

    async fn handle_connection(&self, conn: Connecting) -> Result<(String, Arc<PlayerConnection>)> {
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

    async fn handle_uni(&self, mut recv_stream: RecvStream) -> Result<()> {
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

    async fn handle_bi(&self, send_stream: SendStream, mut recv_stream: RecvStream) -> Result<()> {
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

    async fn handle_datagram(&self, datagram: Bytes) -> Result<()> {
        let (DatagramInfo { player_id }, data) = drop_prefix(datagram)?;
        let Some(player_connection) = self.get_player_connection(&player_id) else {
            return Err(anyhow!("Unknown player: {}", player_id))?;
        };
        player_connection.send_datagram(data)?;
        Ok(())
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
    ) -> Result<Self> {
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

                // player connection closed
                else => {
                    tracing::info!("Player connection closed");
                    break;
                }
            }
        }
    }

    async fn handle_uni(&self, recv_stream: RecvStream) -> Result<()> {
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

    async fn handle_bi(&self, send_stream: SendStream, recv_stream: RecvStream) -> Result<()> {
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

    async fn handle_datagram(&self, datagram: Bytes) -> Result<()> {
        self.ambient_server_conn.send_datagram(prefix(
            &DatagramInfo {
                player_id: self.player_id.clone(),
            },
            datagram,
        )?)?;
        Ok(())
    }
}

#[derive(Debug, Default)]
struct Asset {
    data: Option<Bytes>,
    notify: Arc<Notify>,
}

impl Asset {
    fn store(&mut self, data: impl Into<Bytes>) {
        self.data = Some(data.into());
        self.notify.notify_waiters();
    }
}

pub fn start_http_interface(handle: Handle, listener: TcpListener) {
    let router = Router::new()
        .route("/ping", get(|| async move { "ok" }))
        // .nest_service("/content", get_service(ServeDir::new(project_path.join("build"))).handle_error(handle_error))
        .layer(
            CorsLayer::new()
                .allow_origin(tower_http::cors::Any)
                .allow_methods(vec![Method::GET])
                .allow_headers(tower_http::cors::Any),
        );

    handle.spawn(async move {
        axum::Server::from_tcp(listener)
            .unwrap()
            .serve(router.into_make_service())
            .await
            .unwrap();
    });
}
