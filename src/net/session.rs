use std::sync::Arc;

use anyhow::Result;
use num_bigint::BigUint;
use tokio::net::TcpListener;

use super::message::{framing, Message};
use super::peer::PeerConnection;
use super::tor::ArtiClient;
use crate::crypto::sra::SraKeyPair;
use crate::game::bidding::Seat;
use crate::ui::terminal::TerminalUi;

/// A game session managing two peer connections.
pub struct Session {
    pub my_seat: usize,
    pub my_name: String,
    pub peers: [Arc<PeerConnection>; 2],
    pub names: [String; 3],
    pub key: SraKeyPair,
    pub prime: BigUint,
}

pub struct NativeTransport {
    pub peers: [Arc<PeerConnection>; 2],
}

impl crate::session::Transport for NativeTransport {
    async fn send_to(&self, seat: usize, message: &Message) -> Result<()> {
        let peer = self
            .peers
            .iter()
            .find(|peer| peer.seat == seat)
            .ok_or_else(|| anyhow::anyhow!("No peer for seat {}", seat))?;
        peer.send(message).await
    }
    async fn recv_any(&self) -> Result<(usize, Message)> {
        tokio::select! {
            msg = self.peers[0].recv() => Ok((self.peers[0].seat, msg?)),
            msg = self.peers[1].recv() => Ok((self.peers[1].seat, msg?)),
            quit = crate::ui::terminal::poll_quit() => Err(quit.into()),
        }
    }
}

impl Session {
    pub async fn run_game(self) -> Result<()> {
        let mut ui = TerminalUi::new(self.my_seat, &self.names)?;
        let mut game = crate::session::Session::new(
            self.my_seat,
            self.my_name,
            self.names,
            self.key,
            self.prime,
            NativeTransport { peers: self.peers },
        )?;
        let result = game.run_game(&mut ui).await;
        ui.cleanup()?;
        if result.is_ok() {
            ui.print_final_scores();
        }
        match result {
            Err(error)
                if error
                    .downcast_ref::<crate::ui::terminal::UserQuit>()
                    .is_some() =>
            {
                println!("\nGame ended by player.");
                Ok(())
            }
            other => other,
        }
    }
}

/// Host a game: listen for 2 connections, coordinate the peer mesh, then start.
pub async fn host_game(
    port: u16,
    key: SraKeyPair,
    prime: BigUint,
    tor: Option<ArtiClient>,
) -> Result<()> {
    let my_name = get_player_name();

    enum Acceptor {
        Tcp(TcpListener),
        Tor(futures::stream::BoxStream<'static, tor_hsservice::StreamRequest>),
    }

    // Keep the onion service handle alive for the entire function.
    let mut _onion_svc = None;

    let (mut acceptor, addr1_ip) = if let Some(ref client) = tor {
        let (svc, onion_addr, incoming) =
            super::tor::create_onion_service(client, "skat-host").await?;
        _onion_svc = Some(svc);
        println!("Hosting game on Tor: {}:443", onion_addr);
        println!("Share this address with players. Waiting for 2 players...");
        (Acceptor::Tor(incoming), None)
    } else {
        let listener = TcpListener::bind(format!("0.0.0.0:{}", port)).await?;
        println!("Hosting game on port {}. Waiting for 2 players...", port);
        (Acceptor::Tcp(listener), None)
    };

    // Helper closure-like accept
    async fn accept_one(
        acceptor: &mut Acceptor,
    ) -> Result<(
        super::connect::BoxReader,
        super::connect::BoxWriter,
        Option<std::net::IpAddr>,
    )> {
        match acceptor {
            Acceptor::Tor(ref mut incoming) => {
                let (r, w) = super::tor::accept_stream(incoming).await?;
                Ok((r, w, None))
            }
            Acceptor::Tcp(ref listener) => {
                let (stream, addr) = listener.accept().await?;
                let (r, w) = tokio::io::split(stream);
                Ok((Box::new(r) as _, Box::new(w) as _, Some(addr.ip())))
            }
        }
    }

    let (mut r1, mut w1, conn1_ip) = accept_one(&mut acceptor).await?;
    let addr1_ip = addr1_ip.or(conn1_ip);

    println!("Player connected.");
    let name1 = super::peer::host_handshake(&mut r1, &mut w1, &my_name).await?;
    println!("  -> {}", name1);

    // Accept second player (seat 2)
    let (mut r2, mut w2, _) = accept_one(&mut acceptor).await?;

    println!("Player connected.");
    let name2 = super::peer::host_handshake(&mut r2, &mut w2, &my_name).await?;
    println!("  -> {}", name2);

    let names = [my_name.clone(), name1.clone(), name2.clone()];

    framing::send(
        &mut w1,
        &Message::GameStart {
            seats: names.to_vec(),
            your_seat: 1,
        },
    )
    .await?;
    framing::send(
        &mut w2,
        &Message::GameStart {
            seats: names.to_vec(),
            your_seat: 2,
        },
    )
    .await?;

    // Seat 1 reports its listen port (and optionally a full address override for Tor).
    let peer_addr = match framing::recv(&mut r1).await? {
        Message::PeerListen {
            port,
            addr_override,
        } => match addr_override {
            Some(address) => address,
            None => format!(
                "{}:{}",
                addr1_ip.ok_or_else(|| anyhow::anyhow!("Missing peer address"))?,
                port
            ),
        },
        other => anyhow::bail!("Expected PeerListen from seat 1, got {:?}", other),
    };
    framing::send(&mut w2, &Message::PeerConnect { addr: peer_addr }).await?;

    println!("All players connected! Starting game...");
    println!(
        "  Forehand: {}, Middlehand: {}, Rearhand: {}",
        names[0], names[1], names[2]
    );

    let peer1 = Arc::new(PeerConnection::new(r1, w1, 1));
    let peer2 = Arc::new(PeerConnection::new(r2, w2, 2));

    let session = Session {
        my_seat: 0,
        my_name,
        peers: [peer1, peer2],
        names,
        key,
        prime,
    };
    session.run_game().await
}

/// Join a hosted game.
///
/// `peer_addr`: address others should use to reach you when you're assigned seat 1.
/// - LAN / Tailscale: auto-detected if omitted (uses UDP routing table trick).
/// - Tor (`--tor`): an ephemeral onion service is created automatically.
pub async fn join_game(
    host_addr: &str,
    key: SraKeyPair,
    prime: BigUint,
    peer_addr: Option<String>,
    tor: Option<ArtiClient>,
) -> Result<()> {
    let (mut reader, mut writer) = super::connect::connect(host_addr, tor.as_ref()).await?;
    println!("Connected to {}", host_addr);

    let my_name = get_player_name();
    let host_name = super::peer::join_handshake(&mut reader, &mut writer, &my_name).await?;
    println!("Host: {}", host_name);

    let msg = framing::recv(&mut reader).await?;
    let (names, my_seat) = match msg {
        Message::GameStart { seats, your_seat } => {
            anyhow::ensure!(
                seats.len() == 3 && (your_seat == 1 || your_seat == 2),
                "Invalid seat assignment"
            );
            println!("Assigned seat: {}", Seat::from_index(your_seat).name());
            (seats, your_seat)
        }
        _ => anyhow::bail!("Expected GameStart"),
    };
    println!(
        "  Forehand: {}, Middlehand: {}, Rearhand: {}",
        names[0], names[1], names[2]
    );

    let other_seat: usize;
    let other_peer: Arc<PeerConnection>;

    // Keep the onion service handle alive for the entire function.
    let mut _onion_svc = None;

    if my_seat == 1 {
        other_seat = 2;

        if tor.is_some() {
            // Create an ephemeral onion service for seat 2 to connect to
            let (svc, onion_addr, mut incoming) =
                super::tor::create_onion_service(tor.as_ref().unwrap(), "skat-peer").await?;
            _onion_svc = Some(svc);
            let my_onion = format!("{}:443", onion_addr);

            framing::send(
                &mut writer,
                &Message::PeerListen {
                    port: 7878,
                    addr_override: Some(my_onion),
                },
            )
            .await?;
            println!("Waiting for peer via Tor...");

            let (mut pr, mut pw) = super::tor::accept_stream(&mut incoming).await?;
            super::peer::host_handshake(&mut pr, &mut pw, &my_name).await?;
            other_peer = Arc::new(PeerConnection::new(pr, pw, other_seat));
        } else {
            let peer_listener = TcpListener::bind("0.0.0.0:0").await?;
            let port = peer_listener.local_addr()?.port();

            framing::send(
                &mut writer,
                &Message::PeerListen {
                    port,
                    addr_override: peer_addr,
                },
            )
            .await?;
            println!("Waiting for peer on port {}...", port);

            let (peer_stream, _) = peer_listener.accept().await?;
            let (mut pr, mut pw) = tokio::io::split(peer_stream);
            super::peer::host_handshake(&mut pr, &mut pw, &my_name).await?;
            other_peer = Arc::new(PeerConnection::new(Box::new(pr), Box::new(pw), other_seat));
        }
    } else {
        // my_seat == 2
        other_seat = 1;

        let peer_connect_addr = match framing::recv(&mut reader).await? {
            Message::PeerConnect { addr } => addr,
            other => anyhow::bail!("Expected PeerConnect, got {:?}", other),
        };

        println!("Connecting to peer at {}...", peer_connect_addr);
        let (mut pr, mut pw) = super::connect::connect(&peer_connect_addr, tor.as_ref()).await?;
        super::peer::join_handshake(&mut pr, &mut pw, &my_name).await?;
        other_peer = Arc::new(PeerConnection::new(pr, pw, other_seat));
    }

    let host_peer = Arc::new(PeerConnection::new(reader, writer, 0));
    println!("All peers connected. Game starting!");

    let session = Session {
        my_seat,
        my_name,
        peers: [host_peer, other_peer],
        names: [names[0].clone(), names[1].clone(), names[2].clone()],
        key,
        prime,
    };
    session.run_game().await
}

fn get_player_name() -> String {
    use std::io::Write;
    print!("Enter your name: ");
    std::io::stdout().flush().unwrap();
    let mut name = String::new();
    std::io::stdin().read_line(&mut name).unwrap();
    name.trim().to_string()
}
