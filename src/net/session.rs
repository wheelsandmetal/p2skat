use std::sync::Arc;

use anyhow::Result;
use num_bigint::BigUint;
use tokio::net::TcpListener;

use super::message::{framing, Message};
use super::peer::PeerConnection;
use super::tor::ArtiClient;
use crate::crypto::protocol::{
    card_values, encrypt_and_shuffle, verify_hand, DealPositions,
};
use crate::crypto::sra::SraKeyPair;
use crate::game::bidding::{BidAction, BiddingState, Seat};
use crate::game::card::Card;
use crate::game::contract::{Contract, GameType, Modifiers};
use crate::game::scoring::{count_points, game_value, ScoreBoard};
use crate::game::state::{GameState, Phase};
use crate::game::trick::{legal_plays, Trick};
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

impl Session {
    /// Send a message to a specific seat (not our own).
    pub async fn send_to(&self, seat: usize, msg: &Message) -> Result<()> {
        let peer = self.peer_for_seat(seat)?;
        peer.send(msg).await
    }

    /// Broadcast a message to both peers.
    pub async fn broadcast(&self, msg: &Message) -> Result<()> {
        self.peers[0].send(msg).await?;
        self.peers[1].send(msg).await?;
        Ok(())
    }

    /// Receive a message from a specific seat.
    pub async fn recv_from(&self, seat: usize) -> Result<Message> {
        let peer = self.peer_for_seat(seat)?;
        peer.recv().await
    }

    /// Receive a message from any peer, returns (seat, message).
    pub async fn recv_any(&self) -> Result<(usize, Message)> {
        tokio::select! {
            msg = self.peers[0].recv() => Ok((self.peers[0].seat, msg?)),
            msg = self.peers[1].recv() => Ok((self.peers[1].seat, msg?)),
        }
    }

    async fn recv_from_or_quit(&self, seat: usize) -> Result<Message> {
        tokio::select! {
            result = self.recv_from(seat) => result,
            _ = crate::ui::terminal::poll_quit() => Err(crate::ui::terminal::UserQuit.into()),
        }
    }

    async fn recv_any_or_quit(&self) -> Result<(usize, Message)> {
        tokio::select! {
            result = self.recv_any() => result,
            _ = crate::ui::terminal::poll_quit() => Err(crate::ui::terminal::UserQuit.into()),
        }
    }

    fn peer_for_seat(&self, seat: usize) -> Result<&PeerConnection> {
        for peer in &self.peers {
            if peer.seat == seat {
                return Ok(peer);
            }
        }
        anyhow::bail!("No peer for seat {}", seat)
    }

    /// Run the full game loop.
    pub async fn run_game(self) -> Result<()> {
        let result = self.run_game_inner().await;
        match result {
            Err(e) if e.downcast_ref::<crate::ui::terminal::UserQuit>().is_some() => {
                // Terminal cleanup happens via Drop on TerminalUi.
                // TCP connections close when Session drops, peers get errors and exit.
                println!("\nGame ended by player.");
                Ok(())
            }
            other => other,
        }
    }

    async fn run_game_inner(&self) -> Result<()> {
        let mut state = GameState::new(Seat::from_index(self.my_seat));
        let mut ui = TerminalUi::new(self.my_seat, &self.names)?;

        state.phase = Phase::Shuffling;
        let mut score_board = ScoreBoard::new();
        state.round = 1;
        ui.set_round(state.round);

        loop {

            // === Shuffle & Deal Phase ===
            ui.show_status("Shuffling and dealing cards...")?;
            let encrypted_deck = self.shuffle_phase().await?;
            state.encrypted_deck = encrypted_deck.clone();

            state.phase = Phase::Dealing;
            let my_hand = self.deal_phase(&encrypted_deck).await?;
            state.hand = my_hand;
            state.sort_hand();
            ui.set_hand(&state.hand);

            // === Bidding Phase ===
            state.phase = Phase::Bidding;
            state.bidding = BiddingState::new();
            ui.show_status("=== Bidding ===")?;

            let (winner_seat, final_bid) = self.bidding_phase(&mut state, &mut ui).await?;
            state.declarer = Some(winner_seat);
            state.bidding.winner = Some(winner_seat);
            state.bidding.final_bid = Some(final_bid);

            // === Skat Pickup & Declaration ===
            state.phase = Phase::PickingUpSkat;
            let contract = self
                .declaration_phase(&mut state, &mut ui, winner_seat, final_bid)
                .await?;
            state.contract = Some(contract);
            state.sort_hand();
            ui.set_contract(&contract);
            ui.set_hand(&state.hand);

            // === Playing Phase ===
            state.phase = Phase::Playing;
            state.trick_leader = Seat::Forehand;
            state.current_player = Seat::Forehand;
            state.tricks_played = 0;
            state.tricks_won = [Vec::new(), Vec::new(), Vec::new()];

            self.playing_phase(&mut state, &mut ui).await?;

            // === Scoring ===
            state.phase = Phase::Scoring;
            let declarer_seat = state.declarer.unwrap();
            let declarer_idx = declarer_seat.index();

            // Declarer's cards include trick cards + skat
            let mut declarer_all_cards = state.tricks_won[declarer_idx].clone();
            declarer_all_cards.extend(&state.skat_put_away);

            let decl_points = count_points(&state.tricks_won[declarer_idx])
                + count_points(&state.skat_put_away);
            let decl_tricks = (state.tricks_won[declarer_idx].len() / 3) as u32;

            let value = game_value(
                &contract,
                &declarer_all_cards,
                decl_points,
                decl_tricks,
                10,
            );

            score_board.record(declarer_idx, value, final_bid);
            ui.set_scores(&score_board.scores);

            ui.show_round_result(
                &self.names[declarer_idx],
                value,
                decl_points,
                &score_board.scores,
                &self.names,
            )?;

            // === Key reveal & verification ===
            self.reveal_phase(&encrypted_deck).await?;

            // Check if players want to continue
            if ui.wait_for_continue()? {
                state.new_round();
                ui.set_round(state.round);
                continue;
            } else {
                break;
            }
        }

        ui.cleanup()?;
        println!("\nFinal scores:");
        for (i, name) in self.names.iter().enumerate() {
            println!("  {}: {}", name, score_board.scores[i]);
        }
        Ok(())
    }

    /// Shuffle phase: each player encrypts and shuffles in sequence.
    async fn shuffle_phase(&self) -> Result<Vec<BigUint>> {
        let values = card_values();
        let initial: Vec<BigUint> = values.into_iter().collect();

        match self.my_seat {
            0 => {
                // I'm first: encrypt, shuffle, send to seat 1
                let shuffled = encrypt_and_shuffle(&initial, &self.key);
                let msg = deck_to_message(&shuffled);
                self.send_to(1, &msg).await?;

                // Wait for final deck from seat 2
                let final_msg = self.recv_from_or_quit(2).await?;
                Ok(message_to_deck(&final_msg)?)
            }
            1 => {
                // Wait for deck from seat 0
                let msg = self.recv_from_or_quit(0).await?;
                let deck = message_to_deck(&msg)?;

                // Encrypt and shuffle, send to seat 2
                let shuffled = encrypt_and_shuffle(&deck, &self.key);
                let msg = deck_to_message(&shuffled);
                self.send_to(2, &msg).await?;

                // Wait for final deck from seat 2
                let final_msg = self.recv_from_or_quit(2).await?;
                Ok(message_to_deck(&final_msg)?)
            }
            2 => {
                // Wait for deck from seat 1
                let msg = self.recv_from_or_quit(1).await?;
                let deck = message_to_deck(&msg)?;

                // Encrypt and shuffle
                let shuffled = encrypt_and_shuffle(&deck, &self.key);

                // Broadcast final deck to both peers
                let msg = deck_to_message(&shuffled);
                self.broadcast(&msg).await?;

                Ok(shuffled)
            }
            _ => unreachable!(),
        }
    }

    /// Deal phase: decrypt cards for each player via sequential decryption.
    async fn deal_phase(&self, encrypted_deck: &[BigUint]) -> Result<Vec<Card>> {
        let my_seat = self.my_seat;

        // For each player's hand, the other two players need to remove their encryption layers.
        // Then the owner removes theirs to get plaintext.

        let mut my_cards = Vec::new();

        for target_player in 0..3 {
            let indices = DealPositions::player_indices(target_player);
            let target_encrypted: Vec<BigUint> =
                indices.iter().map(|&i| encrypted_deck[i].clone()).collect();

            if target_player == my_seat {
                // I need others to decrypt for me.
                // The two non-me players decrypt sequentially.
                let others: Vec<usize> = (0..3).filter(|&s| s != my_seat).collect();

                // Send my encrypted cards to first other player for partial decrypt.
                let msg = Message::PartialDecrypt {
                    target: my_seat,
                    cards: biguints_to_hex(&target_encrypted),
                    indices: indices.clone(),
                };
                self.send_to(others[0], &msg).await?;

                // Receive partially decrypted from first other.
                let resp = self.recv_from_or_quit(others[0]).await?;
                let partial1 = extract_partial_decrypt(&resp)?;

                // Send to second other for their layer.
                let msg = Message::PartialDecrypt {
                    target: my_seat,
                    cards: biguints_to_hex(&partial1),
                    indices: indices.clone(),
                };
                self.send_to(others[1], &msg).await?;

                // Receive from second other.
                let resp = self.recv_from_or_quit(others[1]).await?;
                let partial2 = extract_partial_decrypt(&resp)?;

                // Remove my own layer.
                let plaintext: Vec<BigUint> =
                    partial2.iter().map(|c| self.key.decrypt(c)).collect();

                // Convert to cards.
                for val in &plaintext {
                    let idx: u64 = val
                        .try_into()
                        .map_err(|_| anyhow::anyhow!("Invalid card value: {}", val))?;
                    let card = Card::from_index(idx)
                        .ok_or_else(|| anyhow::anyhow!("Invalid card index: {}", idx))?;
                    my_cards.push(card);
                }
            } else {
                // Another player needs decryption. Wait for request, decrypt, forward.
                let msg = self.recv_from_any_matching(|m| matches!(m, Message::PartialDecrypt { target, .. } if *target == target_player)).await?;

                if let Message::PartialDecrypt { cards, .. } = msg {
                    let encrypted = hex_to_biguints(&cards)?;
                    let decrypted: Vec<BigUint> =
                        encrypted.iter().map(|c| self.key.decrypt(c)).collect();

                    // If target is asking us, send back.
                    let resp = Message::PartialDecrypt {
                        target: target_player,
                        cards: biguints_to_hex(&decrypted),
                        indices: indices.clone(),
                    };
                    self.send_to(target_player, &resp).await?;
                }
            }
        }

        Ok(my_cards)
    }

    async fn recv_from_any_matching<F>(&self, pred: F) -> Result<Message>
    where
        F: Fn(&Message) -> bool,
    {
        // Simple: try receiving from either peer
        loop {
            let (_, msg) = self.recv_any_or_quit().await?;
            if pred(&msg) {
                return Ok(msg);
            }
        }
    }

    /// Bidding phase using the terminal UI.
    async fn bidding_phase(
        &self,
        state: &mut GameState,
        ui: &mut TerminalUi,
    ) -> Result<(Seat, u32)> {
        loop {
            if state.bidding.is_done() {
                break;
            }

            if state.bidding.turn == state.my_seat {
                let action = ui.get_bid_action(
                    &state.hand,
                    state.bidding.next_bid_value(),
                    state.my_seat == state.bidding.responder,
                )?;

                state.bidding.process(state.my_seat, action).map_err(|e| anyhow::anyhow!(e))?;

                // Broadcast my action
                let msg = Message::BidMsg {
                    seat: state.my_seat.index(),
                    action: action.into(),
                };
                self.broadcast(&msg).await?;
                ui.show_bid_action(&self.my_name, &action)?;
            } else {
                // Wait for the other player's bid
                let (_seat, msg) = self.recv_any_or_quit().await?;
                if let Message::BidMsg { seat: s, action } = msg {
                    let bid_action: BidAction = action.into();
                    state
                        .bidding
                        .process(Seat::from_index(s), bid_action).map_err(|e| anyhow::anyhow!(e))?;
                    ui.show_bid_action(&self.names[s], &bid_action)?;
                }
            }
        }

        let winner = state.bidding.winner.unwrap();
        let bid = state.bidding.final_bid.unwrap();
        ui.show_status(&format!(
            "{} wins the bidding at {}",
            self.names[winner.index()],
            bid
        ))?;

        Ok((winner, bid))
    }

    /// Declaration phase: skat pickup and contract declaration.
    async fn declaration_phase(
        &self,
        state: &mut GameState,
        ui: &mut TerminalUi,
        declarer: Seat,
        bid: u32,
    ) -> Result<Contract> {
        if declarer == state.my_seat {
            // I'm declarer. Ask if I want to pick up the Skat.
            let pick_up = ui.ask_skat_pickup(&state.hand)?;

            if pick_up {
                // Request skat decryption from others.
                self.broadcast(&Message::SkatRequest).await?;

                // Decrypt skat cards (same process as dealing).
                let skat_indices = DealPositions::skat_indices();
                let skat_encrypted: Vec<BigUint> = skat_indices
                    .iter()
                    .map(|&i| state.encrypted_deck[i].clone())
                    .collect();

                let others: Vec<usize> = (0..3).filter(|&s| s != self.my_seat).collect();

                // Ask first other to decrypt
                let msg = Message::PartialDecrypt {
                    target: self.my_seat,
                    cards: biguints_to_hex(&skat_encrypted),
                    indices: skat_indices.clone(),
                };
                self.send_to(others[0], &msg).await?;
                let resp = self.recv_from_or_quit(others[0]).await?;
                let partial1 = extract_partial_decrypt(&resp)?;

                // Ask second other to decrypt
                let msg = Message::PartialDecrypt {
                    target: self.my_seat,
                    cards: biguints_to_hex(&partial1),
                    indices: skat_indices.clone(),
                };
                self.send_to(others[1], &msg).await?;
                let resp = self.recv_from_or_quit(others[1]).await?;
                let partial2 = extract_partial_decrypt(&resp)?;

                // Remove my layer
                let skat_plain: Vec<BigUint> =
                    partial2.iter().map(|c| self.key.decrypt(c)).collect();

                let mut skat_cards = Vec::new();
                for val in &skat_plain {
                    let idx: u64 = val.try_into()?;
                    skat_cards.push(Card::from_index(idx).unwrap());
                }

                // Add skat to hand
                state.hand.extend(&skat_cards);
                state.skat = skat_cards;
                state.sort_hand();

                // Let player choose 2 cards to put away
                let put_away = ui.choose_skat_discard(&state.hand)?;
                state.skat_put_away = put_away.clone();
                for card in &put_away {
                    state.hand.retain(|c| c != card);
                }

                // Declare contract
                let contract = ui.declare_contract(&state.hand, bid, false)?;

                let msg = Message::Declaration {
                    game_type: game_type_to_msg(contract.game_type),
                    hand: false,
                    schneider_announced: contract.modifiers.schneider_announced,
                    schwarz_announced: contract.modifiers.schwarz_announced,
                    ouvert: contract.modifiers.ouvert,
                    revealed_hand: None,
                };
                self.broadcast(&msg).await?;

                Ok(contract)
            } else {
                // Hand game
                let contract = ui.declare_contract(&state.hand, bid, true)?;

                let msg = Message::Declaration {
                    game_type: game_type_to_msg(contract.game_type),
                    hand: true,
                    schneider_announced: contract.modifiers.schneider_announced,
                    schwarz_announced: contract.modifiers.schwarz_announced,
                    ouvert: contract.modifiers.ouvert,
                    revealed_hand: if contract.modifiers.ouvert {
                        Some(state.hand.iter().map(|c| (*c).into()).collect())
                    } else {
                        None
                    },
                };
                self.broadcast(&msg).await?;

                Ok(contract)
            }
        } else {
            // Wait for declarer's actions.
            // Might get SkatRequest or Declaration.
            loop {
                let (_seat, msg) = self.recv_any_or_quit().await?;
                match msg {
                    Message::SkatRequest => {
                        // We might need to help decrypt the skat. Wait for decrypt request.
                        let (_, decrypt_msg) = self.recv_any_or_quit().await?;
                        if let Message::PartialDecrypt { target, cards, indices } = decrypt_msg {
                            let encrypted = hex_to_biguints(&cards)?;
                            let decrypted: Vec<BigUint> =
                                encrypted.iter().map(|c| self.key.decrypt(c)).collect();
                            let resp = Message::PartialDecrypt {
                                target,
                                cards: biguints_to_hex(&decrypted),
                                indices,
                            };
                            self.send_to(target, &resp).await?;
                        }
                    }
                    Message::Declaration {
                        game_type,
                        hand,
                        schneider_announced,
                        schwarz_announced,
                        ouvert,
                        ..
                    } => {
                        let gt = msg_to_game_type(game_type);
                        let contract = Contract::new(
                            gt,
                            Modifiers {
                                hand,
                                schneider_announced,
                                schwarz_announced,
                                ouvert,
                            },
                        );
                        ui.show_status(&format!(
                            "{} declares {:?}{}",
                            self.names[declarer.index()],
                            gt,
                            if hand { " Hand" } else { "" }
                        ))?;
                        return Ok(contract);
                    }
                    _ => {}
                }
            }
        }
    }

    /// Playing phase: 10 tricks.
    async fn playing_phase(
        &self,
        state: &mut GameState,
        ui: &mut TerminalUi,
    ) -> Result<()> {
        let contract = state.contract.unwrap();

        for trick_num in 0..10 {
            state.current_trick = Trick::new();
            let leader = state.trick_leader;
            ui.set_trick_num(trick_num + 1);
            ui.set_trick(&state.current_trick);
            ui.display_tricks()?;

            for play_offset in 0..3 {
                let current = Seat::from_index((leader.index() + play_offset) % 3);
                state.current_player = current;

                if current == state.my_seat {
                    // My turn to play
                    let legal = legal_plays(&state.hand, &state.current_trick, &contract);
                    let card = ui.choose_card(&state.hand, &legal, &state.current_trick, &contract, trick_num + 1)?;

                    state.current_trick.play(current.index(), card, &contract);
                    state.hand.retain(|c| *c != card);
                    ui.set_trick(&state.current_trick);
                    ui.set_hand(&state.hand);
                    ui.display_tricks()?;

                    let msg = Message::PlayCard { card: card.into() };
                    self.broadcast(&msg).await?;
                } else {
                    // Wait for their card
                    let msg = self.recv_from_or_quit(current.index()).await?;
                    if let Message::PlayCard { card } = msg {
                        let played = card.to_card();
                        state.current_trick.play(current.index(), played, &contract);
                        ui.set_trick(&state.current_trick);
                        ui.display_tricks()?;
                    }
                }
            }

            // Determine trick winner
            let winner = state.current_trick.winner(&contract);
            let trick_points = state.current_trick.points();
            let won_cards: Vec<Card> =
                state.current_trick.cards.iter().map(|(_, c)| *c).collect();
            state.tricks_won[winner].extend(won_cards);
            state.tricks_played += 1;
            state.trick_leader = Seat::from_index(winner);

            ui.show_trick_result(&self.names[winner], trick_points)?;
        }

        Ok(())
    }

    /// Key reveal and verification phase.
    async fn reveal_phase(&self, encrypted_deck: &[BigUint]) -> Result<()> {
        // Broadcast my keys
        let msg = Message::RevealKeys {
            e: self.key.e.to_str_radix(16),
            d: self.key.d.to_str_radix(16),
        };
        self.broadcast(&msg).await?;

        // Collect other keys
        let mut keys_e = vec![BigUint::default(); 3];
        let mut keys_d = vec![BigUint::default(); 3];
        keys_e[self.my_seat] = self.key.e.clone();
        keys_d[self.my_seat] = self.key.d.clone();

        for _ in 0..2 {
            let (seat, msg) = self.recv_any_or_quit().await?;
            if let Message::RevealKeys { e, d } = msg {
                keys_e[seat] = BigUint::parse_bytes(e.as_bytes(), 16)
                    .ok_or_else(|| anyhow::anyhow!("Invalid key hex"))?;
                keys_d[seat] = BigUint::parse_bytes(d.as_bytes(), 16)
                    .ok_or_else(|| anyhow::anyhow!("Invalid key hex"))?;
            }
        }

        // Verify: reconstruct all keys and check the deck
        let key_pairs: Vec<SraKeyPair> = (0..3)
            .map(|i| SraKeyPair {
                e: keys_e[i].clone(),
                d: keys_d[i].clone(),
                p: self.prime.clone(),
            })
            .collect();

        let result = verify_hand(
            encrypted_deck,
            [&key_pairs[0], &key_pairs[1], &key_pairs[2]],
        );
        match result {
            Ok(_) => {
                eprintln!("Verification passed: all cards accounted for.");
            }
            Err(e) => {
                eprintln!("WARNING: Verification failed! {}", e);
            }
        }

        Ok(())
    }
}

// === Helper functions ===

fn deck_to_message(deck: &[BigUint]) -> Message {
    Message::ShuffledDeck {
        cards: biguints_to_hex(deck),
    }
}

fn message_to_deck(msg: &Message) -> Result<Vec<BigUint>> {
    match msg {
        Message::ShuffledDeck { cards } => hex_to_biguints(cards),
        other => anyhow::bail!("Expected ShuffledDeck, got {:?}", other),
    }
}

fn biguints_to_hex(values: &[BigUint]) -> Vec<String> {
    values.iter().map(|v| v.to_str_radix(16)).collect()
}

fn hex_to_biguints(hex: &[String]) -> Result<Vec<BigUint>> {
    hex.iter()
        .map(|s| {
            BigUint::parse_bytes(s.as_bytes(), 16)
                .ok_or_else(|| anyhow::anyhow!("Invalid hex: {}", s))
        })
        .collect()
}

fn extract_partial_decrypt(msg: &Message) -> Result<Vec<BigUint>> {
    match msg {
        Message::PartialDecrypt { cards, .. } => hex_to_biguints(cards),
        other => anyhow::bail!("Expected PartialDecrypt, got {:?}", other),
    }
}

fn game_type_to_msg(gt: GameType) -> super::message::GameTypeMsg {
    match gt {
        GameType::Suit(s) => super::message::GameTypeMsg::Suit(s as u8),
        GameType::Grand => super::message::GameTypeMsg::Grand,
        GameType::Null => super::message::GameTypeMsg::Null,
    }
}

fn msg_to_game_type(msg: super::message::GameTypeMsg) -> GameType {
    use crate::game::card::Suit;
    match msg {
        super::message::GameTypeMsg::Suit(s) => {
            let suit = match s {
                0 => Suit::Diamonds,
                1 => Suit::Hearts,
                2 => Suit::Spades,
                3 => Suit::Clubs,
                _ => panic!("Invalid suit index"),
            };
            GameType::Suit(suit)
        }
        super::message::GameTypeMsg::Grand => GameType::Grand,
        super::message::GameTypeMsg::Null => GameType::Null,
    }
}

/// Host a game: listen for 2 connections, coordinate the peer mesh, then start.
pub async fn host_game(port: u16, key: SraKeyPair, prime: BigUint, tor: Option<ArtiClient>) -> Result<()> {
    let my_name = get_player_name();

    enum Acceptor {
        Tcp(TcpListener),
        Tor(futures::stream::BoxStream<'static, tor_hsservice::StreamRequest>),
    }

    // Keep the onion service handle alive for the entire function.
    let mut _onion_svc = None;

    let (mut acceptor, addr1_ip) = if let Some(ref client) = tor {
        let (svc, onion_addr, incoming) = super::tor::create_onion_service(client, "skat-host").await?;
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
    async fn accept_one(acceptor: &mut Acceptor) -> Result<(super::connect::BoxReader, super::connect::BoxWriter, Option<std::net::IpAddr>)> {
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

    framing::send(&mut w1, &Message::GameStart { seats: names.to_vec(), your_seat: 1 }).await?;
    framing::send(&mut w2, &Message::GameStart { seats: names.to_vec(), your_seat: 2 }).await?;

    // Seat 1 reports its listen port (and optionally a full address override for Tor).
    let peer_addr = match framing::recv(&mut r1).await? {
        Message::PeerListen { port, addr_override } => {
            addr_override.unwrap_or_else(|| format!("{}:{}", addr1_ip.unwrap(), port))
        }
        other => anyhow::bail!("Expected PeerListen from seat 1, got {:?}", other),
    };
    framing::send(&mut w2, &Message::PeerConnect { addr: peer_addr }).await?;

    println!("All players connected! Starting game...");
    println!("  Forehand: {}, Middlehand: {}, Rearhand: {}", names[0], names[1], names[2]);

    let peer1 = Arc::new(PeerConnection::new(r1, w1, 1));
    let peer2 = Arc::new(PeerConnection::new(r2, w2, 2));

    let session = Session { my_seat: 0, my_name, peers: [peer1, peer2], names, key, prime };
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
            println!("Assigned seat: {}", Seat::from_index(your_seat).name());
            (seats, your_seat)
        }
        _ => anyhow::bail!("Expected GameStart"),
    };
    println!("  Forehand: {}, Middlehand: {}, Rearhand: {}", names[0], names[1], names[2]);

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

            framing::send(&mut writer, &Message::PeerListen { port: 7878, addr_override: Some(my_onion) }).await?;
            println!("Waiting for peer via Tor...");

            let (mut pr, mut pw) = super::tor::accept_stream(&mut incoming).await?;
            super::peer::host_handshake(&mut pr, &mut pw, &my_name).await?;
            other_peer = Arc::new(PeerConnection::new(pr, pw, other_seat));
        } else {
            let peer_listener = TcpListener::bind("0.0.0.0:0").await?;
            let port = peer_listener.local_addr()?.port();

            framing::send(&mut writer, &Message::PeerListen { port, addr_override: peer_addr }).await?;
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
