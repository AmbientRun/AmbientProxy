use std::{net::SocketAddr, time::Duration, sync::{Arc, RwLock}, collections::{VecDeque, HashMap}, task::Poll};

use futures::Future;
use quinn::{Endpoint, TransportConfig, ClientConfig, Connection, RecvStream};
use rustls::{Certificate, RootCertStore};

use crate::{protocol::ServerMessage, streams::{IncomingStream, OutgoingStream, read_framed}};

const CERT: &[u8] = include_bytes!("./cert.der");

pub struct Client {
    conn: Connection,
    allocation_id: uuid::Uuid,
    proxy_endpoint: SocketAddr,
    rx: IncomingStream,
    pending_streams: RwLock<HashMap<String, VecDeque<RecvStream>>>,
}

impl Client {
    pub async fn connect(server_addr: SocketAddr) -> anyhow::Result<Self> {
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
            pending_streams: Default::default(),
        })
    }

    pub fn accept_uni(&self, player_id: &str) -> AcceptUni {
        AcceptUni {  }
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

pub struct AcceptUni {

}

impl Future for AcceptUni {
    type Output = anyhow::Result<RecvStream>;

    fn poll(self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<Self::Output> {
        // TODO
        todo!()
    }
}
