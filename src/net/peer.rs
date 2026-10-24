use anyhow::Result;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::Mutex;

use super::message::{framing, Message};

/// A connection to a single peer, transport-agnostic (TCP, Tor, …).
pub struct PeerConnection {
    reader: Mutex<framing::Reader<Box<dyn AsyncRead + Unpin + Send>>>,
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
            reader: Mutex::new(framing::Reader::new(reader)),
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
        reader.recv().await
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

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;
    use tokio::time::{timeout, Duration};

    #[tokio::test]
    async fn canceled_receive_preserves_partial_frames() -> Result<()> {
        let payload = serde_json::to_vec(&Message::Hello { name: "peer".into() })?;
        let mut frame = (payload.len() as u32).to_be_bytes().to_vec();
        frame.extend_from_slice(&payload);

        for cut in [1, 2, 3, 4, 5, frame.len() - 1] {
            let (mut remote, local) = tokio::io::duplex(4096);
            let (reader, writer) = tokio::io::split(local);
            let peer = PeerConnection::new(Box::new(reader), Box::new(writer), 1);
            remote.write_all(&frame[..cut]).await?;

            tokio::select! {
                biased;
                msg = peer.recv() => panic!("Incomplete frame received: {msg:?}"),
                _ = std::future::ready(()) => {},
            }

            remote.write_all(&frame[cut..]).await?;
            framing::send(&mut remote, &Message::Quit).await?;
            let msg = timeout(Duration::from_secs(1), peer.recv()).await??;
            assert!(matches!(msg, Message::Hello { name } if name == "peer"));
            let msg = timeout(Duration::from_secs(1), peer.recv()).await??;
            assert!(matches!(msg, Message::Quit));
        }
        Ok(())
    }

    #[tokio::test]
    async fn receive_rejects_oversized_frame() -> Result<()> {
        let (mut remote, local) = tokio::io::duplex(64);
        let (reader, writer) = tokio::io::split(local);
        let peer = PeerConnection::new(Box::new(reader), Box::new(writer), 1);
        remote.write_all(&(10 * 1024 * 1024 + 1_u32).to_be_bytes()).await?;
        let err = timeout(Duration::from_secs(1), peer.recv()).await?.unwrap_err();
        assert!(err.to_string().contains("Message too large"));
        Ok(())
    }
}
