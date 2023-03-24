use anyhow::anyhow;
use futures::{SinkExt, StreamExt};
use quinn::{RecvStream, SendStream};
use serde::{de::DeserializeOwned, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt, copy};
use tokio_util::codec::{FramedRead, FramedWrite, LengthDelimitedCodec};

#[derive(Debug)]
pub struct IncomingStream {
    stream: FramedRead<quinn::RecvStream, LengthDelimitedCodec>,
}

impl IncomingStream {
    pub fn new(stream: quinn::RecvStream) -> Self {
        let mut codec = LengthDelimitedCodec::new();
        codec.set_max_frame_length(1_024 * 1_024 * 1_024);
        Self {
            stream: FramedRead::new(stream, codec),
        }
    }

    /// Reads the next frame from the incoming stream
    pub async fn next<T: DeserializeOwned + std::fmt::Debug>(&mut self) -> anyhow::Result<T> {
        let buf = self
            .stream
            .next()
            .await
            .ok_or_else(|| anyhow!("Reading error"))??;

        bincode::deserialize(&buf).map_err(Into::into)
    }
}

pub struct OutgoingStream {
    stream: FramedWrite<quinn::SendStream, LengthDelimitedCodec>,
}

impl OutgoingStream {
    pub fn new(stream: quinn::SendStream) -> Self {
        let mut codec = LengthDelimitedCodec::new();
        codec.set_max_frame_length(1_024 * 1_024 * 1_024);
        Self {
            stream: FramedWrite::new(stream, codec),
        }
    }

    /// Sends raw bytes over the network
    pub async fn send_bytes(&mut self, bytes: Vec<u8>) -> anyhow::Result<()> {
        self.stream.send(bytes.into()).await?;

        Ok(())
    }

    pub async fn send<T: Serialize>(&mut self, value: &T) -> anyhow::Result<()> {
        let bytes = bincode::serialize(value)?;
        self.send_bytes(bytes).await
    }
}


pub async fn read_framed<T: DeserializeOwned + std::fmt::Debug>(recv_stream: &mut RecvStream, max_size: u32) -> anyhow::Result<T> {
    let message_len = recv_stream.read_u32().await?;
    if message_len > max_size {
        return Err(anyhow!("Message too long: {}", message_len));
    }
    let mut buffer = vec![0u8; message_len as usize];
    recv_stream.read_exact(&mut buffer).await?;
    Ok(bincode::deserialize(&buffer)?)
}

pub async fn write_framed<T: Serialize>(send_stream: &mut SendStream, value: &T) -> anyhow::Result<()> {
    let bytes = bincode::serialize(value)?;
    send_stream.write_u32(bytes.len() as u32).await?;
    send_stream.write_all(&bytes).await?;
    Ok(())
}

pub fn spawn_stream_copy(mut recv_stream: RecvStream, mut send_stream: SendStream) {
    tokio::spawn(async move {
        if let Err(err) = copy(&mut recv_stream, &mut send_stream).await {
            tracing::error!("Failed to copy streams: {:?}", err);
        }
    });
}
