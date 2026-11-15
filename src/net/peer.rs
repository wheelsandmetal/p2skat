use anyhow::Result;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::Mutex;

use super::message::{framing, protocol_mismatch, Message, PROTOCOL_VERSION};

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

fn hello(my_name: &str) -> Message {
    Message::Hello {
        name: my_name.to_string(),
        version: PROTOCOL_VERSION,
    }
}

/// Accept the peer's Hello only if it speaks our protocol version. On mismatch,
/// tell the peer why before giving up so both sides see a clear error.
async fn check_hello<W: AsyncWrite + Unpin>(writer: &mut W, msg: Message) -> Result<String> {
    match msg {
        Message::Hello { name, version } if version == PROTOCOL_VERSION => Ok(name),
        Message::Hello { name, version } => {
            let error = protocol_mismatch(&name, version);
            let _ = framing::send(
                writer,
                &Message::Error {
                    message: error.clone(),
                },
            )
            .await;
            anyhow::bail!(error)
        }
        Message::Error { message } => anyhow::bail!("Peer refused the connection: {}", message),
        other => anyhow::bail!("Expected Hello, got {:?}", other),
    }
}

/// Handshake for the listening side: send Hello first, then receive.
pub async fn host_handshake<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut R,
    writer: &mut W,
    my_name: &str,
) -> Result<String> {
    framing::send(writer, &hello(my_name)).await?;
    let msg = framing::recv(reader).await?;
    check_hello(writer, msg).await
}

/// Handshake for the connecting side: receive Hello first, then send.
pub async fn join_handshake<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut R,
    writer: &mut W,
    my_name: &str,
) -> Result<String> {
    let msg = framing::recv(reader).await?;
    let peer_name = check_hello(writer, msg).await?;
    framing::send(writer, &hello(my_name)).await?;
    Ok(peer_name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;
    use tokio::time::{timeout, Duration};

    #[tokio::test]
    async fn canceled_receive_preserves_partial_frames() -> Result<()> {
        let payload = serde_json::to_vec(&hello("peer"))?;
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
            assert!(matches!(msg, Message::Hello { name, .. } if name == "peer"));
            let msg = timeout(Duration::from_secs(1), peer.recv()).await??;
            assert!(matches!(msg, Message::Quit));
        }
        Ok(())
    }

    #[tokio::test]
    async fn handshake_rejects_other_protocol_versions_on_both_sides() -> Result<()> {
        // A peer from before versioning sends Hello without a version field.
        let (host, joiner) = tokio::io::duplex(4096);
        let (mut host_r, mut host_w) = tokio::io::split(host);
        let (mut join_r, mut join_w) = tokio::io::split(joiner);
        let old_peer = async {
            let _ = framing::recv(&mut join_r).await?;
            join_w
                .write_all(&{
                    let json = br#"{"Hello":{"name":"old"}}"#;
                    let mut frame = (json.len() as u32).to_be_bytes().to_vec();
                    frame.extend_from_slice(json);
                    frame
                })
                .await?;
            framing::recv(&mut join_r).await
        };
        let (host_result, old_seen) = timeout(Duration::from_secs(1), async {
            tokio::join!(host_handshake(&mut host_r, &mut host_w, "host"), old_peer)
        })
        .await?;
        let error = host_result.unwrap_err().to_string();
        assert!(error.contains("older p2skat"), "{error}");
        assert!(matches!(old_seen?, Message::Error { message } if message.contains("protocol v2")));

        // A newer joiner refuses an older host and tells it why.
        let (host, joiner) = tokio::io::duplex(4096);
        let (mut host_r, mut host_w) = tokio::io::split(host);
        let (mut join_r, mut join_w) = tokio::io::split(joiner);
        let other = Message::Hello {
            name: "host".into(),
            version: PROTOCOL_VERSION + 1,
        };
        framing::send(&mut host_w, &other).await?;
        let error = join_handshake(&mut join_r, &mut join_w, "me")
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("protocol v3"), "{error}");
        assert!(matches!(
            framing::recv(&mut host_r).await?,
            Message::Error { .. }
        ));
        Ok(())
    }

    #[tokio::test]
    async fn receive_rejects_oversized_frame() -> Result<()> {
        let (mut remote, local) = tokio::io::duplex(64);
        let (reader, writer) = tokio::io::split(local);
        let peer = PeerConnection::new(Box::new(reader), Box::new(writer), 1);
        remote
            .write_all(&((framing::MAX_MESSAGE_BYTES + 1) as u32).to_be_bytes())
            .await?;
        let err = timeout(Duration::from_secs(1), peer.recv())
            .await?
            .unwrap_err();
        assert!(err.to_string().contains("Message too large"));
        Ok(())
    }
    #[tokio::test]
    async fn send_rejects_oversized_frame_before_writing() -> Result<()> {
        let (_remote, local) = tokio::io::duplex(64);
        let (reader, writer) = tokio::io::split(local);
        let peer = PeerConnection::new(Box::new(reader), Box::new(writer), 1);
        let message = Message::Error {
            message: "x".repeat(framing::MAX_MESSAGE_BYTES),
        };
        let error = timeout(Duration::from_secs(1), peer.send(&message))
            .await?
            .unwrap_err();
        assert!(error.to_string().contains("Message too large"));
        Ok(())
    }
}
