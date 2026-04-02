use std::sync::Arc;

use anyhow::Result;
use safelog::DisplayRedacted;
use arti_client::TorClient;
use futures::StreamExt;
use tor_cell::relaycell::msg::Connected;
use tor_hsservice::{config::OnionServiceConfigBuilder, handle_rend_requests, RunningOnionService, StreamRequest};
use tor_rtcompat::PreferredRuntime;

pub type ArtiClient = TorClient<PreferredRuntime>;

/// Bootstrap an embedded Tor client with ephemeral state.
pub async fn bootstrap() -> Result<ArtiClient> {
    let state_dir = std::env::temp_dir().join(format!("p2skat-{}", std::process::id()));
    let cache_dir = state_dir.join("cache");

    let config = arti_client::config::TorClientConfigBuilder::from_directories(state_dir, cache_dir)
        .build()
        .map_err(|e| anyhow::anyhow!("Tor config error: {}", e))?;

    eprintln!("Bootstrapping Tor client...");
    let client = TorClient::create_bootstrapped(config).await?;
    eprintln!("Tor client ready.");
    Ok(client)
}

/// Create an ephemeral onion service. Returns the service handle, its .onion address,
/// and a receiver that yields `StreamRequest`s for incoming connections.
pub async fn create_onion_service(
    client: &ArtiClient,
    nickname: &str,
) -> Result<(
    Arc<RunningOnionService>,
    String,
    futures::stream::BoxStream<'static, StreamRequest>,
)> {
    let svc_config = OnionServiceConfigBuilder::default()
        .nickname(nickname.parse().map_err(|e| anyhow::anyhow!("Bad nickname: {}", e))?)
        .build()
        .map_err(|e| anyhow::anyhow!("Onion service config error: {}", e))?;

    let (svc, rend_requests) = client
        .launch_onion_service(svc_config)?
        .ok_or_else(|| anyhow::anyhow!("Onion services not supported by this Tor client"))?;

    // Wait for the service to have an onion address
    eprintln!("Waiting for onion service '{}' to publish...", nickname);
    let mut events: tor_hsservice::status::OnionServiceStatusStream = svc.status_events();
    loop {
        if svc.onion_address().is_some() {
            break;
        }
        if StreamExt::next(&mut events).await.is_none() {
            anyhow::bail!("Onion service status stream ended before publish");
        }
    }

    let onion_addr = svc
        .onion_address()
        .ok_or_else(|| anyhow::anyhow!("No onion address available"))?;
    let addr_string = onion_addr.display_unredacted().to_string();
    eprintln!("Onion service published: {}", addr_string);

    let stream_requests = handle_rend_requests(rend_requests).boxed();

    Ok((svc, addr_string, stream_requests))
}

/// Accept one incoming connection on an onion service stream.
pub async fn accept_stream(
    incoming: &mut futures::stream::BoxStream<'static, StreamRequest>,
) -> Result<(super::connect::BoxReader, super::connect::BoxWriter)> {
    let req = incoming
        .next()
        .await
        .ok_or_else(|| anyhow::anyhow!("Onion service incoming stream ended"))?;

    let data_stream = req
        .accept(Connected::new_empty())
        .await
        .map_err(|e| anyhow::anyhow!("Failed to accept onion stream: {}", e))?;

    let (r, w) = tokio::io::split(data_stream);
    Ok((Box::new(r), Box::new(w)))
}
