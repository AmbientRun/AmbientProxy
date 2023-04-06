use std::{
    collections::{HashMap, VecDeque},
    fmt::Debug,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use bytes::Bytes;
use parking_lot::{Mutex, RwLock};
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
};

const CERT: &[u8] = include_bytes!("./cert.der");

const IDLE_TIMEOUT: Duration = Duration::from_secs(5);

fn default_client_endpoint() -> crate::Result<Endpoint> {
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
    transport.max_idle_timeout(Some(IDLE_TIMEOUT.try_into().expect("Should fit in VarInt")));
    let mut client_config = ClientConfig::new(Arc::new(crypto));
    client_config.transport_config(Arc::new(transport));
    endpoint.set_default_client_config(client_config);
    Ok(endpoint)
}

pub fn builder() -> Builder {
    Builder::new()
}

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

#[derive(Debug, Default)]
pub struct Builder {
    endpoint: Option<Endpoint>,
    proxy_server: Option<String>,
    project_id: String,
    assets_path: Option<PathBuf>,
    user_agent: Option<String>,
}

impl Builder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn endpoint(mut self, endpoint: Endpoint) -> Self {
        self.endpoint = Some(endpoint);
        self
    }

    pub fn proxy_server(mut self, proxy_server: String) -> Self {
        self.proxy_server = Some(proxy_server);
        self
    }

    pub fn project_id(mut self, project_id: String) -> Self {
        self.project_id = project_id;
        self
    }

    pub fn assets_path<P: AsRef<Path>>(mut self, assets_path: P) -> Self {
        self.assets_path = Some(assets_path.as_ref().to_path_buf());
        self
    }

    pub fn user_agent(mut self, user_agent: String) -> Self {
        self.user_agent = Some(user_agent);
        self
    }

    pub async fn build(self) -> anyhow::Result<Client> {
        let endpoint = match self.endpoint {
            Some(endpoint) => endpoint,
            None => default_client_endpoint()?,
        };

        let Some(assets_path) = self.assets_path else {
            return Err(anyhow::anyhow!("assets_path is required"))?;
        };

        let Some(mut proxy_server) = self.proxy_server else {
            return Err(anyhow::anyhow!("proxy_server is required"))?;
        };

        if proxy_server.starts_with("http://") || proxy_server.starts_with("https://") {
            static APP_USER_AGENT: &str =
                concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"),);

            proxy_server = reqwest::ClientBuilder::new()
                .user_agent(self.user_agent.unwrap_or(APP_USER_AGENT.to_string()))
                .build()?
                .get(proxy_server)
                .send()
                .await?
                .error_for_status()?
                .text()
                .await?
                .trim()
                .to_string();

            if proxy_server.is_empty() {
                return Err(anyhow::anyhow!("No proxy servers to use"))?;
            }
        }

        Ok(Client::connect(proxy_server, self.project_id, assets_path, endpoint).await?)
    }
}

pub struct Client {
    conn: Connection,
    rx: IncomingStream,
    tx: OutgoingStream,
    pending: Arc<PendingResources>,
    project_id: String,
    assets_path: PathBuf,
}

impl Client {
    pub async fn connect<T: ToSocketAddrs + Debug + Clone>(
        proxy_server: T,
        project_id: String,
        assets_path: PathBuf,
        endpoint: Endpoint,
    ) -> crate::Result<Self> {
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
            project_id,
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
            project_id,
            assets_path,
        } = self;

        let (client_message_channel_tx, client_meesage_channel_rx) =
            flume::unbounded::<ClientMessage>();
        let endpoint_allocated = Arc::new(RwLock::new(false));
        let endpoint_allocated_notify = Arc::new(Notify::new());
        let controller = ClientController::new(
            client_message_channel_tx,
            project_id,
            assets_path.clone(),
            endpoint_allocated.clone(),
            endpoint_allocated_notify.clone(),
        );

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    // messages received from the proxy server
                    Ok(message) = rx.next::<ServerMessage>() => {
                        tracing::debug!("Got server message: {:?}", &message);
                        match message {
                            ServerMessage::Allocation { .. } => {
                                *endpoint_allocated.write() = true;
                                endpoint_allocated_notify.notify_waiters();
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

                    // proxy server connection closed
                    err = conn.closed() => {
                        tracing::info!("Proxy connection closed: {:?}", err);
                        break;
                    }

                    else => {
                        tracing::info!("Proxy connection closed");
                        break;
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
    project_id: String,
    assets_path: PathBuf,

    endpoint_allocated: Arc<RwLock<bool>>,
    endpoint_allocated_notify: Arc<Notify>,

    pre_cache_assets_stack: Arc<Mutex<Vec<PathBuf>>>,
    pre_caching_task_running: Arc<RwLock<bool>>,
    allocation_requested: bool,
}

impl ClientController {
    fn new(
        tx: flume::Sender<ClientMessage>,
        project_id: String,
        assets_path: PathBuf,
        endpoint_allocated: Arc<RwLock<bool>>,
        endpoint_allocated_notify: Arc<Notify>,
    ) -> Self {
        Self {
            tx,
            project_id,
            assets_path,
            endpoint_allocated,
            endpoint_allocated_notify,
            pre_cache_assets_stack: Default::default(),
            pre_caching_task_running: Default::default(),
            allocation_requested: false,
        }
    }

    pub async fn allocate_endpoint(&mut self) -> crate::Result<()> {
        self.tx
            .send_async(ClientMessage::AllocateEndpoint {
                project_id: self.project_id.clone(),
            })
            .await
            .map_err(|_| anyhow::anyhow!("Failed to send allocate endpoint message"))?;

        self.allocation_requested = true;
        if !self.pre_cache_assets_stack.lock().is_empty() {
            // there are some assets to pre-cache -> start the pre-caching task
            self.start_pre_caching_task_if_needed();
        }

        Ok(())
    }

    pub async fn store_asset(&mut self, key: String, data: Vec<u8>) -> crate::Result<()> {
        tracing::debug!("Storing asset: {:?}", &key);
        Self::send_store_asset_message(&self.tx, key, data).await
    }

    async fn send_store_asset_message(
        tx: &flume::Sender<ClientMessage>,
        key: String,
        data: Vec<u8>,
    ) -> crate::Result<()> {
        tx.send_async(ClientMessage::StoreAsset { key, data })
            .await
            .map_err(|_| anyhow::anyhow!("Failed to send store asset message"))?;
        Ok(())
    }

    pub fn pre_cache_assets(&mut self, directory: impl AsRef<Path>) -> crate::Result<()> {
        // get the path to the directory and make sure it's inside the assets directory
        let path = self.assets_path.join(directory).canonicalize()?;
        if !path.starts_with(&self.assets_path) {
            return Err(anyhow::anyhow!("Directory is outside of assets directory"))?;
        };

        // push the directory onto the stack for later processing
        self.pre_cache_assets_stack.lock().push(path);

        if self.allocation_requested {
            // allocation has already been requested -> start the pre-caching task
            self.start_pre_caching_task_if_needed();
        }

        Ok(())
    }

    fn start_pre_caching_task_if_needed(&self) {
        // make sure we only start one task
        let mut pre_caching_task_running = self.pre_caching_task_running.write();
        if *pre_caching_task_running {
            return;
        }
        *pre_caching_task_running = true;

        let endpoint_allocated = self.endpoint_allocated.clone();
        let endpoint_allocated_notify = self.endpoint_allocated_notify.clone();
        let assets_path = self.assets_path.clone();
        let pre_cache_assets_stack = self.pre_cache_assets_stack.clone();
        let tx = self.tx.clone();
        let pre_caching_task_running = self.pre_caching_task_running.clone();

        tokio::spawn(async move {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
            while tokio::time::Instant::now() < deadline {
                let allocated = *endpoint_allocated.read();
                if allocated {
                    if let Err(err) =
                        Self::drain_pre_cache_assets_stack(assets_path, pre_cache_assets_stack, tx)
                            .await
                    {
                        tracing::warn!("Failed to pre-cache assets: {:?}", err);
                    }
                    break;
                } else {
                    tokio::select! {
                        _ = tokio::time::sleep_until(deadline) => {
                            tracing::warn!("Timed out waiting for proxy endpoint allocation - skipping asset pre-caching");
                            break;
                        }
                        _ = endpoint_allocated_notify.notified() => {}
                    }
                }
            }
            *pre_caching_task_running.write() = false;
        });
    }

    async fn drain_pre_cache_assets_stack(
        assets_path: PathBuf,
        pre_cache_assets_stack: Arc<Mutex<Vec<PathBuf>>>,
        tx: flume::Sender<ClientMessage>,
    ) -> crate::Result<()> {
        while let Some(path) = {
            // this block is needed to make sure the lock guard is dropped before the await
            let path = pre_cache_assets_stack.lock().pop();
            path
        } {
            let mut dir = tokio::fs::read_dir(path).await?;
            while let Some(entry) = dir.next_entry().await? {
                let entry_path = entry.path();
                let Ok(file_type) = entry.file_type().await else {
                    tracing::warn!("Failed to get file type: {:?}", &entry_path);
                    continue;
                };
                if file_type.is_file() {
                    // entry is a file -> store it
                    let Ok(key_path) = entry_path.strip_prefix(&assets_path) else {
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
                    if let Err(err) = Self::send_store_asset_message(&tx, key, data).await {
                        tracing::warn!("Failed to store asset: {:?}", err);
                    }
                } else if file_type.is_dir() {
                    // entry is a directory -> dive into it
                    pre_cache_assets_stack.lock().push(entry_path);
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
    pub async fn open_uni(&self) -> crate::Result<SendStream> {
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

    pub async fn open_bi(&self) -> crate::Result<(SendStream, RecvStream)> {
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

    pub async fn send_datagram(&self, data: Bytes) -> crate::Result<()> {
        Ok(self.conn.send_datagram(crate::bytes::prefix(
            &DatagramInfo {
                player_id: self.player_id.clone(),
            },
            data,
        )?)?)
    }
}
