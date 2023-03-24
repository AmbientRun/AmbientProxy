use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr, TcpListener},
    ops::Range,
    sync::{Arc, RwLock},
};

use anyhow::anyhow;
use axum::{http::Method, routing::get, Router};
use flume::{Receiver, Sender};
use futures::StreamExt;
use quinn::{
    Connecting, Connection, Endpoint, RecvStream, SendStream, ServerConfig, TransportConfig,
};
use rustls::{Certificate, PrivateKey};
use tokio::{
    runtime::Handle,
};
use tower_http::cors::CorsLayer;

use crate::{
    protocol::{ClientMessage, ServerMessage, OpenedStreamInfo, StreamRequestInfo},
    streams::{read_framed, spawn_stream_copy, write_framed, IncomingStream, OutgoingStream},
};

const CERT: &[u8] = include_bytes!("./cert.der");
const CERT_KEY: &[u8] = include_bytes!("./cert.key.der");

pub struct ManagementServer {
    endpoint: Endpoint,
    proxies: RwLock<HashMap<uuid::Uuid, Arc<ProxyServer>>>,
}

impl ManagementServer {
    pub fn new(server_addr: SocketAddr) -> anyhow::Result<Self> {
        let cert = Certificate(CERT.to_vec());
        let cert_key = PrivateKey(CERT_KEY.to_vec());
        let mut server_conf = ServerConfig::with_single_cert(vec![cert], cert_key)?;
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
                    // FIXME: handle that unwrap
                    self.proxies.write().unwrap().insert(id, proxy_server);
                }
                Err(err) => {
                    tracing::error!("Failed handling the connection: {:?}", err);
                }
            }
        }
    }

    async fn handle_connection(
        &self,
        conn: Connecting,
    ) -> anyhow::Result<(uuid::Uuid, Arc<ProxyServer>)> {
        tracing::info!("Got a new connection from: {:?}", conn.remote_address());
        let conn = conn.await?;
        let allocation_id = uuid::Uuid::new_v4();
        // FIXME: grab IP and ports from config
        let proxy_server =
            Arc::new(ProxyServer::new(conn, IpAddr::from([0, 0, 0, 0]), 9000..10000).await?);
        tracing::info!(
            "Created allocation {} at {}",
            proxy_server.allocation_id,
            proxy_server.proxy_endpoint.local_addr()?
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
    allocation_id: uuid::Uuid,
    proxy_endpoint: Endpoint,
    ambient_server_conn: Connection,
    server_message_sender: Sender<ServerMessage>,
    client_message_receiver: Receiver<ClientMessage>,
    player_conns: RwLock<HashMap<String, Arc<PlayerConnection>>>,
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
        ambient_server_conn: Connection,
        addr: IpAddr,
        ports: Range<u16>,
    ) -> anyhow::Result<Self> {
        let allocation_id = uuid::Uuid::new_v4();
        let proxy_endpoint = Self::create_proxy_endpoint(addr, ports)?;

        // open a bi stream to the newly connected server
        let (send_stream, recv_stream) = ambient_server_conn.open_bi().await?;
        let mut tx = OutgoingStream::new(send_stream);
        let mut rx = IncomingStream::new(recv_stream);

        let (client_message_sender, client_message_receiver) = flume::unbounded::<ClientMessage>();
        let (server_message_sender, server_message_receiver) = flume::unbounded::<ServerMessage>();

        tokio::spawn(async move {
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

        // send back the allocation details
        server_message_sender.send_async(ServerMessage::Allocation {
            id: allocation_id,
            endpoint: proxy_endpoint.local_addr()?,
        })
        .await?;

        Ok(Self {
            allocation_id,
            proxy_endpoint,
            ambient_server_conn,
            server_message_sender,
            client_message_receiver,
            player_conns: Default::default(),
        })
    }

    async fn start(&self) {
        let mut client_message_receiver = self.client_message_receiver.stream();
        loop {
            tokio::select! {
                // new player connection
                Some(conn) = self.proxy_endpoint.accept() => {
                    match self.handle_connection(conn).await {
                        Ok((player_id, connection)) => {
                            // FIXME: handle that unwrap
                            self.player_conns.write().unwrap().insert(player_id, connection);
                        }
                        Err(err) => {
                            tracing::error!("Failed to handle incoming client connection: {:?}", err);
                        }
                    }
                }

                // server send a message to the proxy
                Some(message) = client_message_receiver.next() => {
                    // TODO: process message
                    tracing::warn!("Got a message but processing is not implemented yet: {:?}", message);
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
            }
        }
    }

    async fn handle_connection(
        &self,
        conn: Connecting,
    ) -> anyhow::Result<(String, Arc<PlayerConnection>)> {
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
        self.server_message_sender.send_async(ServerMessage::PlayerConnected { player_id: player_id.clone() }).await?;

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
            &OpenedStreamInfo {
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
        // FIXME: handle unwrap
        self.player_conns
            .read()
            .unwrap()
            .get(player_id)
            .map(|p| p.get_connection())
    }

    async fn handle_uni(&self, mut recv_stream: RecvStream) -> anyhow::Result<()> {
        // hand decode the first message so then we can just copy the streams without decoding
        let message: StreamRequestInfo = read_framed(&mut recv_stream, 1024).await?;

        // get player connection
        let Some(player_connection) = self.get_player_connection(&message.player_id) else {
            return Err(anyhow!("Unknown player: {}", message.player_id));
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
    ) -> anyhow::Result<()> {
        // hand decode the first message so then we can just copy the streams without decoding
        let message: StreamRequestInfo = read_framed(&mut recv_stream, 1024).await?;

        // get player connection
        let Some(player_connection) = self.get_player_connection(&message.player_id) else {
            return Err(anyhow!("Unknown player: {}", message.player_id));
        };

        // open a bi stream to the player and copy the server streams there
        let (player_send_stream, player_recv_stream) = player_connection.open_bi().await?;
        spawn_stream_copy(recv_stream, player_send_stream);
        spawn_stream_copy(player_recv_stream, send_stream);

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
    ) -> anyhow::Result<Self> {
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
            }
        }
    }

    async fn handle_uni(&self, recv_stream: RecvStream) -> anyhow::Result<()> {
        let mut send_stream = self.ambient_server_conn.open_uni().await?;
        write_framed(
            &mut send_stream,
            &OpenedStreamInfo {
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
    ) -> anyhow::Result<()> {
        let (mut server_send_stream, server_recv_stream) =
            self.ambient_server_conn.open_bi().await?;
        write_framed(
            &mut server_send_stream,
            &OpenedStreamInfo {
                player_id: self.player_id.clone(),
            },
        )
        .await?;
        spawn_stream_copy(recv_stream, server_send_stream);
        spawn_stream_copy(server_recv_stream, send_stream);
        Ok(())
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
