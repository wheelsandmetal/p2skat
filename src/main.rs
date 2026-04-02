mod crypto;
mod game;
mod net;
mod sim;
mod ui;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "p2skat", about = "Peer-to-peer Skat card game")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Host a game and wait for 2 players
    Host {
        #[arg(short, long, default_value = "7878")]
        port: u16,
        /// Use embedded Tor for zero-config .onion hosting
        #[arg(long)]
        tor: bool,
    },
    /// Join a hosted game
    Join {
        /// Address of the host (e.g. 192.168.1.5:7878 or foo.onion:7878)
        addr: String,
        /// Your address that the third player should connect to when you're seat 1.
        /// Auto-detected on LAN/Tailscale. Unnecessary with --tor.
        #[arg(long)]
        peer_addr: Option<String>,
        /// Use embedded Tor for zero-config .onion connectivity
        #[arg(long)]
        tor: bool,
    },
    /// Run a headless game simulation
    Simulate {
        /// Number of rounds to simulate
        #[arg(short = 'n', long, default_value = "10")]
        rounds: u32,
        /// Use 2048-bit prime (slower but production-grade)
        #[arg(long)]
        large_prime: bool,
        /// Print detailed game log
        #[arg(short, long)]
        verbose: bool,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    let use_tor = match &cli.command {
        Command::Host { tor, .. } => *tor,
        Command::Join { tor, .. } => *tor,
        _ => false,
    };

    // Start key generation and Tor bootstrap concurrently
    match cli.command {
        Command::Host { port, .. } => {
            let key_task = tokio::task::spawn_blocking(|| {
                let prime = crypto::sra::shared_prime();
                let key = crypto::sra::SraKeyPair::generate(&prime);
                (key, prime)
            });

            let tor_task = async {
                if use_tor {
                    Some(net::tor::bootstrap().await)
                } else {
                    None
                }
            };

            let (key_result, tor_result) = tokio::join!(key_task, tor_task);
            let (key, prime) = key_result?;
            let tor_client = tor_result.transpose()?;

            net::session::host_game(port, key, prime, tor_client).await?;
        }
        Command::Join { addr, peer_addr, .. } => {
            let key_task = tokio::task::spawn_blocking(|| {
                let prime = crypto::sra::shared_prime();
                let key = crypto::sra::SraKeyPair::generate(&prime);
                (key, prime)
            });

            let tor_task = async {
                if use_tor {
                    Some(net::tor::bootstrap().await)
                } else {
                    None
                }
            };

            let (key_result, tor_result) = tokio::join!(key_task, tor_task);
            let (key, prime) = key_result?;
            let tor_client = tor_result.transpose()?;

            net::session::join_game(&addr, key, prime, peer_addr, tor_client).await?;
        }
        Command::Simulate {
            rounds,
            large_prime,
            verbose,
        } => {
            sim::run(&sim::SimConfig {
                rounds,
                use_large_prime: large_prime,
                verbose,
            })
            .map_err(|e: String| anyhow::anyhow!(e))?;
        }
    }

    Ok(())
}
