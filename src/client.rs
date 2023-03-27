use std::{
    collections::{HashMap, VecDeque},
    fmt::Debug,
    net::SocketAddr,
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
    protocol::{DatagramInfo, ServerMessage, StreamInfo},
    streams::{read_framed, write_framed, IncomingStream, OutgoingStream},
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
    pub allocation_id: uuid::Uuid,
    pub proxy_endpoint: SocketAddr,
    rx: IncomingStream,
    tx: OutgoingStream, // TODO: implement assets handling
    pending: Arc<PendingResources>,
}

impl Client {
    pub async fn connect<T: ToSocketAddrs + Debug + Clone>(host: T) -> anyhow::Result<Self> {
        let server_addr = lookup_host(host.clone())
            .await?
            .next()
            .ok_or_else(|| anyhow::anyhow!("{:?} not found", host))?;

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

        let conn = endpoint.connect(server_addr, "localhost")?.await?;
        let (send_stream, recv_stream) = conn.accept_bi().await?;
        let mut rx = IncomingStream::new(recv_stream);
        let tx = OutgoingStream::new(send_stream);
        let allocation: ServerMessage = rx.next().await?;
        let ServerMessage::Allocation { id: allocation_id, endpoint: proxy_endpoint } = allocation else {
            return Err(anyhow::anyhow!("Unexpected proxy message: {:?}", allocation));
        };
        println!("{:?} via {:?}", allocation_id, proxy_endpoint);

        Ok(Self {
            conn,
            allocation_id,
            proxy_endpoint,
            rx,
            tx,
            pending: Default::default(),
        })
    }

    pub async fn run(
        mut self,
        on_player_connected: Arc<dyn Fn(String, ProxiedConnection) + Sync + Send>,
    ) {
        loop {
            tokio::select! {
                Ok(message) = self.rx.next::<ServerMessage>() => {
                    match message {
                        ServerMessage::Allocation { .. } => {
                            // shouldn't happen
                            tracing::warn!("Unexpected allocation message: {:?}", message);
                        }
                        ServerMessage::PlayerConnected { player_id } => {
                            tracing::debug!("Player connected: {}", player_id);
                            on_player_connected(player_id.clone(), ProxiedConnection { player_id, conn: self.conn.clone(), pending: self.pending.clone() });
                        }
                        ServerMessage::RequestAsset { key } => {
                            tracing::debug!("Asset requested: {}", key);
                            todo!();
                        }
                    }
                }
                Ok(mut recv_stream) = self.conn.accept_uni() => {
                    let Ok(StreamInfo { player_id }) = read_framed(&mut recv_stream, 1024).await else {
                        tracing::warn!("Failed to read stream info");
                        continue;
                    };
                    self.pending.uni_streams.write().entry(player_id).or_default().push(recv_stream);
                }
                Ok((send_stream, mut recv_stream)) = self.conn.accept_bi() => {
                    let Ok(StreamInfo { player_id }) = read_framed(&mut recv_stream, 1024).await else {
                        tracing::warn!("Failed to read stream info");
                        continue;
                    };
                    self.pending.bi_streams.write().entry(player_id).or_default().push((send_stream, recv_stream));
                }
                Ok(datagram) = self.conn.read_datagram() => {
                    tracing::debug!("Datagram received");
                    let Ok((DatagramInfo { player_id }, data)) = crate::bytes::drop_prefix(datagram) else {
                        tracing::warn!("Failed to read datagram info");
                        continue;
                    };
                    self.pending.datagrams.write().entry(player_id).or_default().push(data);
                }
            }
        }
    }

    pub async fn dump(&mut self) {
        loop {
            tokio::select! {
                Ok(message) = self.rx.next::<ServerMessage>() => {
                    dbg!(message);
                }
                Ok(recv_stream) = self.conn.accept_uni() => {
                    let mut recv = IncomingStream::new(recv_stream);
                    tokio::spawn(async move {
                        let player_id: ServerMessage = recv.next().await.unwrap();
                        dbg!(player_id);
                        while let Ok(buf) = recv.next::<Vec<u8>>().await {
                            dbg!(buf);
                        }
                    });
                }
                Ok((_send_stream, recv_stream)) = self.conn.accept_bi() => {
                    let mut recv = IncomingStream::new(recv_stream);
                    tokio::spawn(async move {
                        let player_id: ServerMessage = recv.next().await.unwrap();
                        dbg!(player_id);
                        while let Ok(buf) = recv.next::<Vec<u8>>().await {
                            dbg!(buf);
                        }
                    });
                }
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct ProxiedConnection {
    conn: Connection,
    player_id: String,
    pending: Arc<PendingResources>,
}

impl ProxiedConnection {
    pub async fn open_uni(&self) -> anyhow::Result<SendStream> {
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

    pub async fn open_bi(&self) -> anyhow::Result<(SendStream, RecvStream)> {
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

    pub async fn send_datagram(&self, data: Bytes) -> anyhow::Result<()> {
        Ok(self.conn.send_datagram(crate::bytes::prefix(
            &DatagramInfo {
                player_id: self.player_id.clone(),
            },
            data,
        )?)?)
    }
}
