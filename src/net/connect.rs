use anyhow::Result;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;

use super::tor::ArtiClient;

pub type BoxReader = Box<dyn AsyncRead + Unpin + Send>;
pub type BoxWriter = Box<dyn AsyncWrite + Unpin + Send>;

/// Connect to addr. When a TorClient is provided, all traffic is routed through Tor
/// with retries (onion service descriptors take time to propagate).
/// Otherwise, plain TCP.
pub async fn connect(addr: &str, tor: Option<&ArtiClient>) -> Result<(BoxReader, BoxWriter)> {
    if let Some(client) = tor {
        let mut attempts = 0;
        let max_attempts = 12;
        loop {
            attempts += 1;
            match client.connect(addr).await {
                Ok(stream) => {
                    let (r, w) = tokio::io::split(stream);
                    return Ok((Box::new(r), Box::new(w)));
                }
                Err(e) if attempts < max_attempts => {
                    eprintln!(
                        "Tor connect attempt {}/{} failed: {}. Retrying in 10s...",
                        attempts, max_attempts, e
                    );
                    tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                }
                Err(e) => return Err(e.into()),
            }
        }
    } else {
        let stream = TcpStream::connect(addr).await?;
        let (r, w) = tokio::io::split(stream);
        Ok((Box::new(r), Box::new(w)))
    }
}
