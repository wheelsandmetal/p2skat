use anyhow::Result;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::Mutex;

use super::message::{framing, Message};

/// A connection to a single peer, transport-agnostic (TCP, Tor, …).
pub struct PeerConnection {
    reader: Mutex<Box<dyn AsyncRead + Unpin + Send>>,
    writer: Mutex<Box<dyn AsyncWrite + Unpin + Send>>,
    pub seat: usize,
}

impl PeerConnection {
    pub fn new(
        reader: Box<dyn AsyncRead + Unpin + Send>,
        writer: Box<dyn AsyncWrite + Unpin + Send>,
        seat: usize,
    ) -> Self {
        Self {
            reader: Mutex::new(reader),
            writer: Mutex::new(writer),
            seat,
        }
    }

    pub async fn send(&self, msg: &Message) -> Result<()> {
        let mut writer = self.writer.lock().await;
        // Box<dyn AsyncWrite + Unpin + Send> implements AsyncWriteExt + Unpin
        framing::send(&mut *writer, msg).await
    }

    pub async fn recv(&self) -> Result<Message> {
        let mut reader = self.reader.lock().await;
        framing::recv(&mut *reader).await
    }
}

/// Handshake for the listening side: send Hello first, then receive.
pub async fn host_handshake<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut R,
    writer: &mut W,
    my_name: &str,
) -> Result<String> {
    framing::send(writer, &Message::Hello { name: my_name.to_string() }).await?;
    match framing::recv(reader).await? {
        Message::Hello { name } => Ok(name),
        other => anyhow::bail!("Expected Hello, got {:?}", other),
    }
}

/// Handshake for the connecting side: receive Hello first, then send.
pub async fn join_handshake<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut R,
    writer: &mut W,
    my_name: &str,
) -> Result<String> {
    let peer_name = match framing::recv(reader).await? {
        Message::Hello { name } => name,
        other => anyhow::bail!("Expected Hello, got {:?}", other),
    };
    framing::send(writer, &Message::Hello { name: my_name.to_string() }).await?;
    Ok(peer_name)
}
