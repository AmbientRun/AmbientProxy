use std::{
    collections::{HashMap, VecDeque},
    fmt::Debug,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use bytes::Bytes;
use parking_lot::RwLock;
use quinn::{ClientConfig, Connection, Endpoint, RecvStream, SendStream, TransportConfig};
use rustls::{Certificate, RootCertStore};
use tokio::{
    net::{lookup_host, ToSocketAddrs},
    sync::Notify,
};

use crate::{
    paths::{load_asset_data, path_to_key},
    protocol::{ClientMessage, DatagramInfo, ServerMessage, StreamInfo},
    streams::{read_framed, write_framed, IncomingStream, OutgoingStream},
    Result,
};

const CERT: &[u8] = include_bytes!("./cert.der");

#[derive(Debug)]
struct QueuedResource<T> {
    queue: VecDeque<T>,
    notify: Arc<Notify>,
}

impl<T> QueuedResource<T> {
    fn push(&mut self, value: T) {
        self.queue.push_back(value);
        self.notify.notify_one();
    }

    fn pop(&mut self) -> Option<T> {
        self.queue.pop_front()
    }
}

impl<T> Default for QueuedResource<T> {
    fn default() -> Self {
        Self {
            queue: Default::default(),
            notify: Arc::new(Notify::new()),
        }
    }
}

#[derive(Debug, Default)]
struct PendingResources {
    uni_streams: RwLock<HashMap<String, QueuedResource<RecvStream>>>,
    bi_streams: RwLock<HashMap<String, QueuedResource<(SendStream, RecvStream)>>>,
    datagrams: RwLock<HashMap<String, QueuedResource<Bytes>>>,
}

pub struct Client {
    conn: Connection,
    rx: IncomingStream,
    tx: OutgoingStream,
    pending: Arc<PendingResources>,
    assets_path: PathBuf,
}

impl Client {
    pub async fn connect<T: ToSocketAddrs + Debug + Clone>(
        proxy_server: T,
        assets_path: PathBuf,
    ) -> Result<Self> {
        let mut endpoint = Endpoint::client(SocketAddr::from(([0, 0, 0, 0], 0)))?;

        let cert = Certificate(CERT.to_vec());
        let mut roots = RootCertStore::empty();
        roots.add(&cert).unwrap();
        let crypto = rustls::ClientConfig::builder()
            .with_safe_defaults()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let mut transport = TransportConfig::default();
        transport.keep_alive_interval(Some(Duration::from_secs_f32(1.)));
        transport.max_idle_timeout(None);
        let mut client_config = ClientConfig::new(Arc::new(crypto));
        client_config.transport_config(Arc::new(transport));
        endpoint.set_default_client_config(client_config);

        Self::connect_using_endpoint(proxy_server, assets_path, endpoint).await
    }

    pub async fn connect_using_endpoint<T: ToSocketAddrs + Debug + Clone>(
        proxy_server: T,
        assets_path: PathBuf,
        endpoint: Endpoint,
    ) -> Result<Self> {
        let server_addr = lookup_host(proxy_server.clone())
            .await?
            .next()
            .ok_or_else(|| anyhow::anyhow!("{:?} not found", proxy_server))?;

        let conn = endpoint
            .connect(server_addr, "localhost")
            .map_err(anyhow::Error::from)?
            .await?;
        let (send_stream, recv_stream) = conn.open_bi().await?;
        let rx = IncomingStream::new(recv_stream);
        let tx = OutgoingStream::new(send_stream);

        Ok(Self {
            conn,
            rx,
            tx,
            pending: Default::default(),
            assets_path: assets_path.canonicalize()?,
        })
    }

    pub fn start(
        self,
        on_endpoint_allocated: Arc<dyn Fn(AllocatedEndpoint) + Sync + Send>,
        on_player_connected: Arc<dyn Fn(String, ProxiedConnection) + Sync + Send>,
    ) -> ClientController {
        let Client {
            conn,
            mut rx,
            mut tx,
            pending,
            assets_path,
        } = self;
        let (client_message_channel_tx, client_meesage_channel_rx) =
            flume::unbounded::<ClientMessage>();
        let controller = ClientController {
            tx: client_message_channel_tx,
            assets_path: assets_path.clone(),
        };
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    // messages received from the proxy server
                    Ok(message) = rx.next::<ServerMessage>() => {
                        tracing::debug!("Got server message: {:?}", &message);
                        match message {
                            ServerMessage::Allocation { .. } => {
                                on_endpoint_allocated(message.try_into().expect("Already matched allocation message"));
                            }
                            ServerMessage::PlayerConnected { player_id } => {
                                on_player_connected(player_id.clone(), ProxiedConnection { player_id, conn: conn.clone(), pending: pending.clone() });
                            }
                            ServerMessage::RequestAsset { key } => {
                                // load asset from disk
                                let Ok(data) = load_asset_data(&assets_path, &key).await else {
                                    tracing::warn!("Failed to open asset file: {:?}", &key);
                                    continue;
                                };

                                if let Err(err) = tx.send(&ClientMessage::StoreAsset { key, data }).await {
                                    tracing::warn!("Failed to send asset: {:?}", err);
                                }
                            }
                        }
                    }

                    // resending messages from ClientController to the proxy server
                    Ok(message) = client_meesage_channel_rx.recv_async() => {
                        if let Err(err) = tx.send(&message).await {
                            tracing::warn!("Failed to send client message: {:?}", err);
                        }
                    }

                    // player's uni stream proxied from the proxy server
                    Ok(mut recv_stream) = conn.accept_uni() => {
                        let Ok(StreamInfo { player_id }) = read_framed(&mut recv_stream, 1024).await else {
                            tracing::warn!("Failed to read stream info");
                            continue;
                        };
                        pending.uni_streams.write().entry(player_id).or_default().push(recv_stream);
                    }

                    // player's bi stream proxied from the proxy server
                    Ok((send_stream, mut recv_stream)) = conn.accept_bi() => {
                        let Ok(StreamInfo { player_id }) = read_framed(&mut recv_stream, 1024).await else {
                            tracing::warn!("Failed to read stream info");
                            continue;
                        };
                        pending.bi_streams.write().entry(player_id).or_default().push((send_stream, recv_stream));
                    }

                    // player's datagram proxied from the proxy server
                    Ok(datagram) = conn.read_datagram() => {
                        tracing::debug!("Datagram received");
                        let Ok((DatagramInfo { player_id }, data)) = crate::bytes::drop_prefix(datagram) else {
                            tracing::warn!("Failed to read datagram info");
                            continue;
                        };
                        pending.datagrams.write().entry(player_id).or_default().push(data);
                    }
                }
            }
        });
        controller
    }
}

#[derive(Debug, Clone)]
pub struct AllocatedEndpoint {
    pub id: uuid::Uuid,
    pub allocated_endpoint: String,
    pub external_endpoint: SocketAddr,
    pub assets_root: String,
}

impl TryFrom<ServerMessage> for AllocatedEndpoint {
    type Error = ();

    fn try_from(value: ServerMessage) -> std::result::Result<Self, Self::Error> {
        match value {
            ServerMessage::Allocation {
                id,
                allocated_endpoint,
                external_endpoint,
                assets_root,
            } => Ok(Self {
                id,
                allocated_endpoint,
                external_endpoint,
                assets_root,
            }),
            _ => Err(()),
        }
    }
}

pub struct ClientController {
    tx: flume::Sender<ClientMessage>,
    assets_path: PathBuf,
}

impl ClientController {
    pub async fn allocate_endpoint(&mut self) -> Result<()> {
        self.tx
            .send_async(ClientMessage::AllocateEndpoint)
            .await
            .map_err(|_| anyhow::anyhow!("Failed to send allocate endpoint message"))?;
        Ok(())
    }

    pub async fn store_asset(&mut self, key: String, data: Vec<u8>) -> Result<()> {
        tracing::debug!("Storing asset: {:?}", &key);
        self.tx
            .send_async(ClientMessage::StoreAsset { key, data })
            .await
            .map_err(|_| anyhow::anyhow!("Failed to send store asset message"))?;
        Ok(())
    }

    pub async fn pre_cache_assets(&mut self, directory: impl AsRef<Path>) -> Result<()> {
        // get the path to the directory and make sure it's inside the assets directory
        let path = self.assets_path.join(directory).canonicalize()?;
        if !path.starts_with(&self.assets_path) {
            return Err(anyhow::anyhow!("Directory is outside of assets directory"))?;
        };

        // prepare the stack to dive deep into the directory
        let mut path_stack = Vec::new();
        path_stack.push(path);

        while let Some(path) = path_stack.pop() {
            let mut dir = tokio::fs::read_dir(path).await?;
            while let Some(entry) = dir.next_entry().await? {
                let entry_path = entry.path();
                let Ok(file_type) = entry.file_type().await else {
                    tracing::warn!("Failed to get file type: {:?}", &entry_path);
                    continue;
                };
                if file_type.is_file() {
                    // entry is a file -> store it
                    let Ok(key_path) = entry_path.strip_prefix(&self.assets_path) else {
                        tracing::warn!("Failed to strip prefix: {:?}", &entry_path);
                        continue;
                    };
                    let Ok(key) = path_to_key(key_path) else {
                        tracing::warn!("Failed to convert path to key: {:?}", &key_path);
                        continue;
                    };
                    let Ok(data) = tokio::fs::read(&entry_path).await else {
                        tracing::warn!("Failed to read asset file: {:?}", &entry_path);
                        continue;
                    };
                    if let Err(err) = self.store_asset(key, data).await {
                        tracing::warn!("Failed to store asset: {:?}", err);
                    }
                } else if file_type.is_dir() {
                    // entry is a directory -> dive into it
                    path_stack.push(entry_path);
                }
            }
        }

        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct ProxiedConnection {
    conn: Connection,
    player_id: String,
    pending: Arc<PendingResources>,
}

impl ProxiedConnection {
    pub async fn open_uni(&self) -> Result<SendStream> {
        let mut send_stream = self.conn.open_uni().await?;
        write_framed(
            &mut send_stream,
            &StreamInfo {
                player_id: self.player_id.clone(),
            },
        )
        .await?;
        Ok(send_stream)
    }

    pub async fn open_bi(&self) -> Result<(SendStream, RecvStream)> {
        let (mut send_stream, recv_stream) = self.conn.open_bi().await?;
        write_framed(
            &mut send_stream,
            &StreamInfo {
                player_id: self.player_id.clone(),
            },
        )
        .await?;
        Ok((send_stream, recv_stream))
    }

    pub async fn accept_uni(&self) -> RecvStream {
        loop {
            let notify = {
                let mut map = self.pending.uni_streams.write();
                let resource = map.entry(self.player_id.clone()).or_default();
                if let Some(stream) = resource.pop() {
                    return stream;
                }
                resource.notify.clone()
            };
            notify.notified().await;
        }
    }

    pub async fn accept_bi(&self) -> (SendStream, RecvStream) {
        loop {
            let notify = {
                let mut map = self.pending.bi_streams.write();
                let resource = map.entry(self.player_id.clone()).or_default();
                if let Some(streams) = resource.pop() {
                    return streams;
                }
                resource.notify.clone()
            };
            notify.notified().await;
        }
    }

    pub async fn read_datagram(&self) -> Bytes {
        loop {
            let notify = {
                let mut map = self.pending.datagrams.write();
                let resource = map.entry(self.player_id.clone()).or_default();
                if let Some(datagram) = resource.pop() {
                    return datagram;
                }
                resource.notify.clone()
            };
            notify.notified().await;
        }
    }

    pub async fn send_datagram(&self, data: Bytes) -> Result<()> {
        Ok(self.conn.send_datagram(crate::bytes::prefix(
            &DatagramInfo {
                player_id: self.player_id.clone(),
            },
            data,
        )?)?)
    }
}
