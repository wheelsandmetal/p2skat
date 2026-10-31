//! Shared game flow for terminal and browser frontends.
use std::cell::RefCell;
use std::collections::VecDeque;
use std::future::Future;

use crate::crypto::protocol::{card_values, encrypt_and_shuffle, verify_hand, DealPositions};
use crate::crypto::sra::SraKeyPair;
use crate::game::bidding::{BidAction, BiddingState, Seat};
use crate::game::card::Card;
use crate::game::contract::{Contract, GameType, Modifiers};
use crate::game::scoring::{count_points, game_value, ScoreBoard};
use crate::game::state::{GameState, Phase};
use crate::game::trick::{legal_plays, Trick};
use crate::net::message::Message;
use anyhow::Result;
use num_bigint::BigUint;

/// Receives must preserve buffered messages when the future is canceled.
#[allow(async_fn_in_trait)]
pub trait Transport {
    async fn send_to(&self, seat: usize, message: &Message) -> Result<()>;
    async fn recv_any(&self) -> Result<(usize, Message)>;
}

#[allow(async_fn_in_trait)]
pub trait Frontend {
    fn set_hand(&mut self, _hand: &[Card]) {}
    fn set_trick(&mut self, _trick: &Trick) {}
    fn set_contract(&mut self, _contract: &Contract) {}
    fn set_scores(&mut self, _scores: &[i32; 3]) {}
    fn set_trick_num(&mut self, _n: u32) {}
    fn set_round(&mut self, _n: u32) {}
    fn set_current_player(&mut self, _seat: usize) {}
    fn show_status(&mut self, _message: &str) -> Result<()> {
        Ok(())
    }
    fn display_tricks(&mut self) -> Result<()> {
        Ok(())
    }
    fn show_bid_action(&mut self, _name: &str, _action: &BidAction) -> Result<()> {
        Ok(())
    }
    fn show_trick_result(&mut self, _winner: &str, _points: u32) -> Result<()> {
        Ok(())
    }
    fn show_round_result(
        &mut self,
        _declarer: &str,
        _value: i32,
        _points: u32,
        _scores: &[i32; 3],
        _names: &[String; 3],
    ) -> Result<()> {
        Ok(())
    }
    async fn get_bid_action(
        &mut self,
        hand: &[Card],
        next_bid: Option<u32>,
        is_responder: bool,
    ) -> Result<BidAction>;
    async fn ask_skat_pickup(&mut self, hand: &[Card]) -> Result<bool>;
    async fn choose_skat_discard(&mut self, hand: &[Card]) -> Result<Vec<Card>>;
    async fn declare_contract(
        &mut self,
        hand: &[Card],
        bid: u32,
        is_hand: bool,
    ) -> Result<Contract>;
    async fn choose_card(
        &mut self,
        hand: &[Card],
        legal: &[Card],
        trick: &Trick,
        contract: &Contract,
        trick_num: u32,
    ) -> Result<Card>;
    async fn wait_for_continue(&mut self) -> Result<bool>;
}

pub struct Session<T> {
    pub my_seat: usize,
    pub my_name: String,
    pub names: [String; 3],
    pub key: SraKeyPair,
    pub prime: BigUint,
    pub transport: T,
    pending: RefCell<VecDeque<(usize, Message)>>,
}

impl<T: Transport> Session<T> {
    pub fn new(
        my_seat: usize,
        my_name: String,
        names: [String; 3],
        key: SraKeyPair,
        prime: BigUint,
        transport: T,
    ) -> Result<Self> {
        anyhow::ensure!(my_seat < 3, "Invalid local seat");
        anyhow::ensure!(
            prime > BigUint::from(32u8) && key.p == prime,
            "Invalid game prime"
        );
        Ok(Self {
            my_seat,
            my_name,
            names,
            key,
            prime,
            transport,
            pending: RefCell::new(VecDeque::new()),
        })
    }

    pub async fn send_to(&self, seat: usize, msg: &Message) -> Result<()> {
        anyhow::ensure!(seat < 3 && seat != self.my_seat, "Invalid destination seat");
        self.transport.send_to(seat, msg).await
    }
    pub async fn broadcast(&self, msg: &Message) -> Result<()> {
        let mut result = Ok(());
        for seat in 0..3 {
            if seat != self.my_seat {
                if let Err(error) = self.send_to(seat, msg).await {
                    result = Err(error);
                }
            }
        }
        result
    }
    async fn receive(&self) -> Result<(usize, Message)> {
        let (seat, msg) = self.transport.recv_any().await?;
        anyhow::ensure!(seat < 3 && seat != self.my_seat, "Invalid sender seat");
        match &msg {
            Message::Quit => anyhow::bail!("{} left the game", self.names[seat]),
            Message::Error { message } => anyhow::bail!("Peer error: {}", message),
            _ => Ok((seat, msg)),
        }
    }
    fn buffer(&self, message: (usize, Message)) -> Result<()> {
        let mut pending = self.pending.borrow_mut();
        anyhow::ensure!(pending.len() < 64, "Too many pending peer messages");
        pending.push_back(message);
        Ok(())
    }
    async fn recv_from_or_quit(&self, seat: usize) -> Result<Message> {
        let queued = {
            let mut pending = self.pending.borrow_mut();
            pending
                .iter()
                .position(|(sender, _)| *sender == seat)
                .and_then(|i| pending.remove(i))
        };
        if let Some((_, msg)) = queued {
            return Ok(msg);
        }
        loop {
            let (sender, msg) = self.receive().await?;
            if sender == seat {
                return Ok(msg);
            }
            self.buffer((sender, msg))?;
        }
    }
    /// Keep processing disconnects while an asynchronous frontend waits for input.
    async fn input<F: Future<Output = Result<R>>, R>(&self, choice: F) -> Result<R> {
        futures::pin_mut!(choice);
        loop {
            match futures::future::select(choice, Box::pin(self.receive())).await {
                futures::future::Either::Left((result, _)) => return result,
                futures::future::Either::Right((message, remaining)) => {
                    self.buffer(message?)?;
                    choice = remaining;
                }
            }
        }
    }
    pub async fn run_game(&mut self, ui: &mut impl Frontend) -> Result<()> {
        let result = self.run_game_inner(ui).await;
        if result.is_err() {
            let _ = self.broadcast(&Message::Quit).await;
        }
        result
    }
    async fn run_game_inner(&mut self, ui: &mut impl Frontend) -> Result<()> {
        let mut state = GameState::new(Seat::from_index(self.my_seat));

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

            let (winner_seat, final_bid) = self.bidding_phase(&mut state, ui).await?;
            state.declarer = Some(winner_seat);
            state.bidding.winner = Some(winner_seat);
            state.bidding.final_bid = Some(final_bid);

            // === Skat Pickup & Declaration ===
            state.phase = Phase::PickingUpSkat;
            let contract = self
                .declaration_phase(&mut state, ui, winner_seat, final_bid)
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

            let plays = self.playing_phase(&mut state, ui).await?;

            // Reveal only after play ends so every peer can score the same hand and skat.
            let revealed_deck = self.reveal_phase(&encrypted_deck).await?;

            // === Scoring ===
            state.phase = Phase::Scoring;
            let declarer_seat = state.declarer.unwrap();
            let declarer_idx = declarer_seat.index();

            let (value, decl_points) = score_revealed_hand(&state, &revealed_deck)?;
            verify_plays(&revealed_deck, &plays, &contract, declarer_idx)?;

            score_board.record(declarer_idx, value, final_bid);
            ui.set_scores(&score_board.scores);

            ui.show_round_result(
                &self.names[declarer_idx],
                value,
                decl_points,
                &score_board.scores,
                &self.names,
            )?;

            // Check if players want to continue
            if self.input(ui.wait_for_continue()).await? {
                self.broadcast(&Message::NextRound).await?;
                for seat in (0..3).filter(|seat| *seat != self.my_seat) {
                    anyhow::ensure!(
                        matches!(self.recv_from_or_quit(seat).await?, Message::NextRound),
                        "Expected agreement to next round"
                    );
                }
                state.new_round();
                ui.set_round(state.round);
                continue;
            } else {
                self.broadcast(&Message::Quit).await?;
                break;
            }
        }

        Ok(())
    }

    /// Shuffle phase: each player encrypts and shuffles in sequence.
    async fn shuffle_phase(&mut self) -> Result<Vec<BigUint>> {
        // Previous rounds reveal their keys, so every new deck needs a fresh pair.
        self.key = SraKeyPair::generate(&self.prime);
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
                Ok(message_to_deck(&final_msg, &self.prime)?)
            }
            1 => {
                // Wait for deck from seat 0
                let msg = self.recv_from_or_quit(0).await?;
                let deck = message_to_deck(&msg, &self.prime)?;

                // Encrypt and shuffle, send to seat 2
                let shuffled = encrypt_and_shuffle(&deck, &self.key);
                let msg = deck_to_message(&shuffled);
                self.send_to(2, &msg).await?;

                // Wait for final deck from seat 2
                let final_msg = self.recv_from_or_quit(2).await?;
                Ok(message_to_deck(&final_msg, &self.prime)?)
            }
            2 => {
                // Wait for deck from seat 1
                let msg = self.recv_from_or_quit(1).await?;
                let deck = message_to_deck(&msg, &self.prime)?;

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
        anyhow::ensure!(encrypted_deck.len() == 32, "Expected 32 encrypted cards");

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
                let partial1 = extract_partial_decrypt(&resp, self.my_seat, &indices, &self.prime)?;

                // Send to second other for their layer.
                let msg = Message::PartialDecrypt {
                    target: my_seat,
                    cards: biguints_to_hex(&partial1),
                    indices: indices.clone(),
                };
                self.send_to(others[1], &msg).await?;

                // Receive from second other.
                let resp = self.recv_from_or_quit(others[1]).await?;
                let partial2 = extract_partial_decrypt(&resp, self.my_seat, &indices, &self.prime)?;

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
                let msg = self.recv_from_or_quit(target_player).await?;

                if let Message::PartialDecrypt {
                    target,
                    cards,
                    indices: requested,
                } = msg
                {
                    anyhow::ensure!(
                        target == target_player && requested == indices,
                        "Unexpected decryption target or indices"
                    );
                    let encrypted = hex_to_biguints(&cards, &self.prime)?;
                    anyhow::ensure!(
                        encrypted.len() == indices.len(),
                        "Invalid decryption card count"
                    );
                    let decrypted: Vec<BigUint> =
                        encrypted.iter().map(|c| self.key.decrypt(c)).collect();

                    // If target is asking us, send back.
                    let resp = Message::PartialDecrypt {
                        target: target_player,
                        cards: biguints_to_hex(&decrypted),
                        indices: indices.clone(),
                    };
                    self.send_to(target_player, &resp).await?;
                } else {
                    anyhow::bail!("Expected PartialDecrypt from seat {}", target_player);
                }
            }
        }

        anyhow::ensure!(my_cards.len() == 10, "Expected ten cards in hand");
        ensure_unique_cards(&my_cards)?;
        Ok(my_cards)
    }

    /// Bidding phase using the terminal UI.
    async fn bidding_phase(
        &self,
        state: &mut GameState,
        ui: &mut impl Frontend,
    ) -> Result<(Seat, u32)> {
        loop {
            if state.bidding.is_done() {
                break;
            }

            ui.set_current_player(state.bidding.turn.index());
            if state.bidding.turn == state.my_seat {
                let action = self
                    .input(ui.get_bid_action(
                        &state.hand,
                        state.bidding.next_bid_value(),
                        state.my_seat == state.bidding.responder,
                    ))
                    .await?;

                state
                    .bidding
                    .process(state.my_seat, action)
                    .map_err(|e| anyhow::anyhow!(e))?;

                // Broadcast my action
                let msg = Message::BidMsg {
                    seat: state.my_seat.index(),
                    action: action.into(),
                };
                self.broadcast(&msg).await?;
                ui.show_bid_action(&self.my_name, &action)?;
            } else {
                // Wait for the other player's bid
                let sender = state.bidding.turn.index();
                let msg = self.recv_from_or_quit(sender).await?;
                if let Message::BidMsg { seat: s, action } = msg {
                    anyhow::ensure!(s < 3 && s == sender, "Bid seat does not match sender");
                    let bid_action: BidAction = action.into();
                    state
                        .bidding
                        .process(Seat::from_index(s), bid_action)
                        .map_err(|e| anyhow::anyhow!(e))?;
                    ui.show_bid_action(&self.names[s], &bid_action)?;
                } else {
                    anyhow::bail!("Expected bid from seat {}", sender);
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
        ui: &mut impl Frontend,
        declarer: Seat,
        bid: u32,
    ) -> Result<Contract> {
        if declarer == state.my_seat {
            // I'm declarer. Ask if I want to pick up the Skat.
            let pick_up = self.input(ui.ask_skat_pickup(&state.hand)).await?;

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
                let partial1 =
                    extract_partial_decrypt(&resp, self.my_seat, &skat_indices, &self.prime)?;

                // Ask second other to decrypt
                let msg = Message::PartialDecrypt {
                    target: self.my_seat,
                    cards: biguints_to_hex(&partial1),
                    indices: skat_indices.clone(),
                };
                self.send_to(others[1], &msg).await?;
                let resp = self.recv_from_or_quit(others[1]).await?;
                let partial2 =
                    extract_partial_decrypt(&resp, self.my_seat, &skat_indices, &self.prime)?;

                // Remove my layer
                let skat_plain: Vec<BigUint> =
                    partial2.iter().map(|c| self.key.decrypt(c)).collect();

                let mut skat_cards = Vec::new();
                for val in &skat_plain {
                    let idx: u64 = val.try_into()?;
                    skat_cards.push(
                        Card::from_index(idx)
                            .ok_or_else(|| anyhow::anyhow!("Invalid skat card index"))?,
                    );
                }

                // Add skat to hand
                state.hand.extend(&skat_cards);
                ensure_unique_cards(&state.hand)?;
                state.skat = skat_cards;
                state.sort_hand();

                // Let player choose 2 cards to put away
                let put_away = self.input(ui.choose_skat_discard(&state.hand)).await?;
                anyhow::ensure!(
                    put_away.len() == 2
                        && put_away[0] != put_away[1]
                        && put_away.iter().all(|card| state.hand.contains(card)),
                    "Invalid skat discards"
                );
                state.skat_put_away = put_away.clone();
                for card in &put_away {
                    state.hand.retain(|c| c != card);
                }

                // Declare contract
                let contract = self
                    .input(ui.declare_contract(&state.hand, bid, false))
                    .await?;
                validate_contract(&contract, false)?;

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
                let contract = self
                    .input(ui.declare_contract(&state.hand, bid, true))
                    .await?;
                validate_contract(&contract, true)?;

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
            let mut picked_up = false;
            loop {
                let msg = self.recv_from_or_quit(declarer.index()).await?;
                match msg {
                    Message::SkatRequest => {
                        anyhow::ensure!(!picked_up, "Duplicate skat request");
                        picked_up = true;
                        let decrypt_msg = self.recv_from_or_quit(declarer.index()).await?;
                        if let Message::PartialDecrypt {
                            target,
                            cards,
                            indices,
                        } = decrypt_msg
                        {
                            anyhow::ensure!(
                                target == declarer.index()
                                    && indices == DealPositions::skat_indices(),
                                "Invalid skat decryption request"
                            );
                            let encrypted = hex_to_biguints(&cards, &self.prime)?;
                            anyhow::ensure!(encrypted.len() == 2, "Expected two skat cards");
                            let decrypted: Vec<BigUint> =
                                encrypted.iter().map(|c| self.key.decrypt(c)).collect();
                            let resp = Message::PartialDecrypt {
                                target,
                                cards: biguints_to_hex(&decrypted),
                                indices,
                            };
                            self.send_to(target, &resp).await?;
                        } else {
                            anyhow::bail!("Expected skat decryption request");
                        }
                    }
                    Message::Declaration {
                        game_type,
                        hand,
                        schneider_announced,
                        schwarz_announced,
                        ouvert,
                        revealed_hand,
                    } => {
                        let gt = msg_to_game_type(game_type)?;
                        if let Some(cards) = &revealed_hand {
                            anyhow::ensure!(ouvert && cards.len() == 10, "Invalid revealed hand");
                            let cards = cards
                                .iter()
                                .map(|card| card.to_card())
                                .collect::<Result<Vec<_>>>()?;
                            ensure_unique_cards(&cards)?;
                        }
                        anyhow::ensure!(!ouvert || revealed_hand.is_some(), "Missing ouvert hand");
                        let contract = Contract::new(
                            gt,
                            Modifiers {
                                hand,
                                schneider_announced,
                                schwarz_announced,
                                ouvert,
                            },
                        );
                        validate_contract(&contract, !picked_up)?;
                        ui.show_status(&format!(
                            "{} declares {:?}{}",
                            self.names[declarer.index()],
                            gt,
                            if hand { " Hand" } else { "" }
                        ))?;
                        return Ok(contract);
                    }
                    other => anyhow::bail!("Unexpected declaration message: {:?}", other),
                }
            }
        }
    }

    /// Playing phase: 10 tricks.
    async fn playing_phase(
        &self,
        state: &mut GameState,
        ui: &mut impl Frontend,
    ) -> Result<Vec<(usize, Card)>> {
        let contract = state.contract.unwrap();
        let mut plays = Vec::with_capacity(30);

        for trick_num in 0..10 {
            state.current_trick = Trick::new();
            let leader = state.trick_leader;
            ui.set_trick_num(trick_num + 1);
            ui.set_trick(&state.current_trick);
            ui.display_tricks()?;

            for play_offset in 0..3 {
                let current = Seat::from_index((leader.index() + play_offset) % 3);
                state.current_player = current;
                ui.set_current_player(current.index());

                if current == state.my_seat {
                    // My turn to play
                    let legal = legal_plays(&state.hand, &state.current_trick, &contract);
                    let card = self
                        .input(ui.choose_card(
                            &state.hand,
                            &legal,
                            &state.current_trick,
                            &contract,
                            trick_num + 1,
                        ))
                        .await?;
                    anyhow::ensure!(legal.contains(&card), "Illegal local card play");

                    plays.push((current.index(), card));
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
                        let played = card.to_card()?;
                        plays.push((current.index(), played));
                        state.current_trick.play(current.index(), played, &contract);
                        ui.set_trick(&state.current_trick);
                        ui.display_tricks()?;
                    } else {
                        anyhow::bail!("Expected card play from seat {}", current.index());
                    }
                }
            }

            // Determine trick winner
            let winner = state.current_trick.winner(&contract);
            let trick_points = state.current_trick.points();
            let won_cards: Vec<Card> = state.current_trick.cards.iter().map(|(_, c)| *c).collect();
            state.tricks_won[winner].extend(won_cards);
            state.tricks_played += 1;
            state.trick_leader = Seat::from_index(winner);

            ui.show_trick_result(&self.names[winner], trick_points)?;
        }

        Ok(plays)
    }

    /// Key reveal and verification phase.
    async fn reveal_phase(&self, encrypted_deck: &[BigUint]) -> Result<Vec<Card>> {
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

        for seat in (0..3).filter(|seat| *seat != self.my_seat) {
            match self.recv_from_or_quit(seat).await? {
                Message::RevealKeys { e, d } => {
                    keys_e[seat] = parse_number(&e, &self.prime)?;
                    keys_d[seat] = parse_number(&d, &self.prime)?;
                    anyhow::ensure!(
                        (&keys_e[seat] * &keys_d[seat]) % crate::crypto::sra::phi(&self.prime)
                            == BigUint::from(1u8),
                        "Revealed key pair is inconsistent"
                    );
                }
                other => anyhow::bail!("Expected RevealKeys from seat {}, got {:?}", seat, other),
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

        let cards = verify_hand(
            encrypted_deck,
            [&key_pairs[0], &key_pairs[1], &key_pairs[2]],
        )
        .map_err(|e| anyhow::anyhow!("Verification failed: {}", e))?;
        Ok(cards)
    }
}

// === Helper functions ===

fn verify_plays(
    deck: &[Card],
    plays: &[(usize, Card)],
    contract: &Contract,
    declarer: usize,
) -> Result<()> {
    anyhow::ensure!(deck.len() == 32 && plays.len() == 30, "Incomplete round");
    let mut hands: [Vec<Card>; 3] =
        std::array::from_fn(|seat| deck[DealPositions::player_range(seat)].to_vec());
    if !contract.modifiers.hand {
        hands[declarer].extend_from_slice(&deck[DealPositions::skat_range()]);
        hands[declarer].retain(|card| plays.iter().any(|(_, played)| played == card));
    }
    let mut leader = 0;
    for cards in plays.chunks_exact(3) {
        let mut trick = Trick::new();
        for (offset, &(seat, card)) in cards.iter().enumerate() {
            anyhow::ensure!(seat == (leader + offset) % 3, "Play from wrong seat");
            anyhow::ensure!(
                legal_plays(&hands[seat], &trick, contract).contains(&card),
                "Illegal revealed card play"
            );
            hands[seat].retain(|held| *held != card);
            trick.play(seat, card, contract);
        }
        leader = trick.winner(contract);
    }
    anyhow::ensure!(
        hands.iter().all(Vec::is_empty),
        "Unplayed cards in revealed hands"
    );
    Ok(())
}

fn score_revealed_hand(state: &GameState, deck: &[Card]) -> Result<(i32, u32)> {
    anyhow::ensure!(deck.len() == 32, "Expected 32 revealed cards");
    let declarer = state
        .declarer
        .ok_or_else(|| anyhow::anyhow!("Missing declarer"))?
        .index();
    let contract = state
        .contract
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("Missing contract"))?;
    let mut declarer_cards = deck[DealPositions::player_range(declarer)].to_vec();
    declarer_cards.extend_from_slice(&deck[DealPositions::skat_range()]);

    // The two unplayed cards are the final skat, including in Hand games.
    let mut skat = deck.to_vec();
    for won_cards in &state.tricks_won {
        anyhow::ensure!(won_cards.len() % 3 == 0, "Incomplete trick in scoring");
        for card in won_cards {
            let pos = skat
                .iter()
                .position(|c| c == card)
                .ok_or_else(|| anyhow::anyhow!("Played card missing or duplicated: {}", card))?;
            skat.remove(pos);
        }
    }
    anyhow::ensure!(skat.len() == 2, "Expected 30 played cards");
    anyhow::ensure!(
        skat.iter().all(|c| declarer_cards.contains(c)),
        "Skat contains another player's card"
    );
    if contract.modifiers.hand {
        anyhow::ensure!(
            skat.iter()
                .all(|c| deck[DealPositions::skat_range()].contains(c)),
            "Skat was played in a Hand game"
        );
    }

    let won_cards = &state.tricks_won[declarer];
    let points = count_points(won_cards) + count_points(&skat);
    let value = game_value(
        contract,
        &declarer_cards,
        points,
        (won_cards.len() / 3) as u32,
        10,
    );
    Ok((value, points))
}

fn deck_to_message(deck: &[BigUint]) -> Message {
    Message::ShuffledDeck {
        cards: biguints_to_hex(deck),
    }
}

fn message_to_deck(msg: &Message, prime: &BigUint) -> Result<Vec<BigUint>> {
    match msg {
        Message::ShuffledDeck { cards } => {
            anyhow::ensure!(cards.len() == 32, "Expected 32 encrypted cards");
            hex_to_biguints(cards, prime)
        }
        other => anyhow::bail!("Expected ShuffledDeck, got {:?}", other),
    }
}

fn biguints_to_hex(values: &[BigUint]) -> Vec<String> {
    values.iter().map(|v| v.to_str_radix(16)).collect()
}

fn parse_number(value: &str, prime: &BigUint) -> Result<BigUint> {
    anyhow::ensure!(
        !value.is_empty()
            && value.len() <= prime.to_str_radix(16).len()
            && value.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "Invalid or oversized hex number"
    );
    let number =
        BigUint::parse_bytes(value.as_bytes(), 16).ok_or_else(|| anyhow::anyhow!("Invalid hex"))?;
    anyhow::ensure!(
        number > BigUint::default() && &number < prime,
        "Number out of range"
    );
    Ok(number)
}

fn hex_to_biguints(hex: &[String], prime: &BigUint) -> Result<Vec<BigUint>> {
    anyhow::ensure!(hex.len() <= 32, "Too many encrypted cards");
    hex.iter().map(|value| parse_number(value, prime)).collect()
}

fn extract_partial_decrypt(
    msg: &Message,
    seat: usize,
    expected: &[usize],
    prime: &BigUint,
) -> Result<Vec<BigUint>> {
    match msg {
        Message::PartialDecrypt {
            target,
            cards,
            indices,
        } => {
            anyhow::ensure!(
                *target == seat && indices == expected && cards.len() == expected.len(),
                "Invalid partial decryption response"
            );
            hex_to_biguints(cards, prime)
        }
        other => anyhow::bail!("Expected PartialDecrypt, got {:?}", other),
    }
}

fn ensure_unique_cards(cards: &[Card]) -> Result<()> {
    for (i, card) in cards.iter().enumerate() {
        anyhow::ensure!(!cards[..i].contains(card), "Duplicate card");
    }
    Ok(())
}

fn validate_contract(contract: &Contract, hand: bool) -> Result<()> {
    let modifiers = contract.modifiers;
    anyhow::ensure!(
        modifiers.hand == hand,
        "Contract Hand flag differs from skat choice"
    );
    anyhow::ensure!(
        hand || !(modifiers.schneider_announced || modifiers.schwarz_announced || modifiers.ouvert),
        "Announcements require a Hand game"
    );
    anyhow::ensure!(
        !modifiers.schwarz_announced || modifiers.schneider_announced,
        "Schwarz announcement requires Schneider"
    );
    Ok(())
}

fn game_type_to_msg(gt: GameType) -> crate::net::message::GameTypeMsg {
    match gt {
        GameType::Suit(s) => crate::net::message::GameTypeMsg::Suit(s as u8),
        GameType::Grand => crate::net::message::GameTypeMsg::Grand,
        GameType::Null => crate::net::message::GameTypeMsg::Null,
    }
}

fn msg_to_game_type(msg: crate::net::message::GameTypeMsg) -> Result<GameType> {
    use crate::game::card::Suit;
    Ok(match msg {
        crate::net::message::GameTypeMsg::Suit(s) => {
            let suit = match s {
                0 => Suit::Diamonds,
                1 => Suit::Hearts,
                2 => Suit::Spades,
                3 => Suit::Clubs,
                _ => anyhow::bail!("Invalid suit index"),
            };
            GameType::Suit(suit)
        }
        crate::net::message::GameTypeMsg::Grand => GameType::Grand,
        crate::net::message::GameTypeMsg::Null => GameType::Null,
    })
}

#[cfg(test)]
mod scoring_tests {
    use super::*;

    fn card(index: u64) -> Card {
        Card::from_index(index).unwrap()
    }

    fn revealed_round(hand_game: bool, my_seat: usize) -> (GameState, Vec<Card>) {
        let initial_hand: Vec<_> = [29, 21, 1, 2, 3, 9, 10, 11, 17, 18].map(card).into();
        let initial_skat = [card(4), card(12)];
        let mut deck = initial_hand.clone();
        deck.extend(
            (1..=32)
                .map(card)
                .filter(|c| !initial_hand.contains(c) && !initial_skat.contains(c)),
        );
        deck.extend(initial_skat);

        let (skat, won) = if hand_game {
            (initial_skat, [32, 24, 16, 8, 1, 2].map(card).to_vec())
        } else {
            (
                [card(29), card(21)],
                [32, 24, 16, 8, 4, 12].map(card).to_vec(),
            )
        };
        let other_tricks: Vec<_> = deck
            .iter()
            .copied()
            .filter(|c| !skat.contains(c) && !won.contains(c))
            .collect();
        let mut state = GameState::new(Seat::from_index(my_seat));
        state.declarer = Some(Seat::Forehand);
        state.contract = Some(Contract::new(
            GameType::Grand,
            Modifiers {
                hand: hand_game,
                ..Modifiers::default()
            },
        ));
        state.tricks_won = [
            won,
            other_tricks[..12].to_vec(),
            other_tricks[12..].to_vec(),
        ];
        // Only the declarer knows its discards before key reveal.
        if my_seat == 0 && !hand_game {
            state.skat = initial_skat.to_vec();
            state.skat_put_away = skat.to_vec();
        }
        (state, deck)
    }

    #[test]
    fn peers_score_pickup_from_original_hand_and_reconstructed_discards() {
        for my_seat in 0..3 {
            let (state, deck) = revealed_round(false, my_seat);
            // CJ and SJ were discarded: still with two, despite winning neither jack.
            assert_eq!(score_revealed_hand(&state, &deck).unwrap(), (72, 68));
        }
    }

    #[test]
    fn peers_include_untouched_skat_in_hand_game_points() {
        for my_seat in 0..3 {
            let (state, deck) = revealed_round(true, my_seat);
            // Twenty unseen skat points lift the declarer from 44 to 64.
            assert_eq!(score_revealed_hand(&state, &deck).unwrap(), (96, 64));
        }
    }

    #[test]
    fn scoring_rejects_duplicate_or_missing_played_cards() {
        let (mut state, deck) = revealed_round(false, 1);
        state.tricks_won[1][0] = state.tricks_won[0][0];
        assert!(score_revealed_hand(&state, &deck).is_err());

        let (mut state, deck) = revealed_round(false, 1);
        state.tricks_won[1].truncate(9);
        assert!(score_revealed_hand(&state, &deck).is_err());
    }

    #[test]
    fn scoring_rejects_playing_the_skat_in_a_hand_game() {
        let (mut state, deck) = revealed_round(true, 1);
        state.tricks_won[0][4] = deck[30];
        assert!(score_revealed_hand(&state, &deck).is_err());
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod session_tests {
    use super::*;
    use crate::net::message::{BidActionMsg, CardMsg};
    use tokio::sync::{mpsc, Mutex};

    struct MemoryTransport {
        seat: usize,
        senders: [mpsc::UnboundedSender<(usize, Message)>; 3],
        receiver: Mutex<mpsc::UnboundedReceiver<(usize, Message)>>,
        sent: RefCell<Vec<Message>>,
    }
    impl Transport for MemoryTransport {
        async fn send_to(&self, seat: usize, message: &Message) -> Result<()> {
            self.sent.borrow_mut().push(message.clone());
            self.senders[seat].send((self.seat, message.clone()))?;
            Ok(())
        }
        async fn recv_any(&self) -> Result<(usize, Message)> {
            self.receiver
                .lock()
                .await
                .recv()
                .await
                .ok_or_else(|| anyhow::anyhow!("Disconnected"))
        }
    }
    fn sessions() -> [Session<MemoryTransport>; 3] {
        let channels: [_; 3] = std::array::from_fn(|_| mpsc::unbounded_channel());
        let senders = channels.each_ref().map(|(sender, _)| sender.clone());
        let prime = (BigUint::from(1u8) << 127usize) - BigUint::from(1u8);
        let mut seat = 0;
        channels.map(|(_, receiver)| {
            let transport = MemoryTransport {
                seat,
                senders: senders.clone(),
                receiver: Mutex::new(receiver),
                sent: RefCell::new(Vec::new()),
            };
            let game = Session::new(
                seat,
                format!("Player {seat}"),
                std::array::from_fn(|i| format!("Player {i}")),
                SraKeyPair::generate(&prime),
                prime.clone(),
                transport,
            )
            .unwrap();
            seat += 1;
            game
        })
    }
    #[derive(Default)]
    struct Bot {
        rounds: u32,
        results: Vec<([i32; 3], u32)>,
    }
    impl Frontend for Bot {
        fn set_round(&mut self, round: u32) {
            self.rounds = round;
        }
        fn show_round_result(
            &mut self,
            _: &str,
            _: i32,
            points: u32,
            scores: &[i32; 3],
            _: &[String; 3],
        ) -> Result<()> {
            self.results.push((*scores, points));
            Ok(())
        }
        async fn get_bid_action(
            &mut self,
            _: &[Card],
            _: Option<u32>,
            _: bool,
        ) -> Result<BidAction> {
            Ok(BidAction::Pass)
        }
        async fn ask_skat_pickup(&mut self, _: &[Card]) -> Result<bool> {
            Ok(self.rounds == 1)
        }
        async fn choose_skat_discard(&mut self, hand: &[Card]) -> Result<Vec<Card>> {
            Ok(hand[..2].to_vec())
        }
        async fn declare_contract(&mut self, _: &[Card], _: u32, hand: bool) -> Result<Contract> {
            Ok(Contract::new(
                GameType::Grand,
                Modifiers {
                    hand,
                    ..Default::default()
                },
            ))
        }
        async fn choose_card(
            &mut self,
            _: &[Card],
            legal: &[Card],
            _: &Trick,
            _: &Contract,
            _: u32,
        ) -> Result<Card> {
            Ok(legal[0])
        }
        async fn wait_for_continue(&mut self) -> Result<bool> {
            Ok(self.rounds < 2)
        }
    }

    async fn deal_and_reveal(
        session: &mut Session<MemoryTransport>,
    ) -> Result<(Vec<BigUint>, Vec<Card>)> {
        let deck = session.shuffle_phase().await?;
        let hand = session.deal_phase(&deck).await?;
        session.reveal_phase(&deck).await?;
        Ok((deck, hand))
    }
    #[tokio::test]
    async fn revealed_keys_cannot_decrypt_the_next_round() {
        let [mut a, mut b, mut c] = sessions();
        let mut previous_keys: Option<[SraKeyPair; 3]> = None;

        for _ in 0..2 {
            let (round_a, round_b, round_c) =
                tokio::time::timeout(std::time::Duration::from_secs(10), async {
                    tokio::try_join!(
                        deal_and_reveal(&mut a),
                        deal_and_reveal(&mut b),
                        deal_and_reveal(&mut c),
                    )
                })
                .await
                .expect("round should complete")
                .unwrap();

            assert_eq!(round_a.0, round_b.0);
            assert_eq!(round_a.0, round_c.0);
            let decoded = verify_hand(&round_a.0, [&a.key, &b.key, &c.key]).unwrap();
            for (seat, hand) in [round_a.1, round_b.1, round_c.1].iter().enumerate() {
                assert_eq!(hand, &decoded[DealPositions::player_range(seat)]);
            }
            if let Some(keys) = &previous_keys {
                assert!(verify_hand(&round_a.0, [&keys[0], &keys[1], &keys[2]]).is_err());
            }
            previous_keys = Some([a.key.clone(), b.key.clone(), c.key.clone()]);
        }
    }
    #[tokio::test]
    async fn three_players_complete_pickup_and_hand_rounds_with_fresh_keys() {
        let [mut a, mut b, mut c] = sessions();
        let [mut ui_a, mut ui_b, mut ui_c] = std::array::from_fn::<_, 3, _>(|_| Bot::default());
        let results = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            tokio::join!(
                a.run_game(&mut ui_a),
                b.run_game(&mut ui_b),
                c.run_game(&mut ui_c)
            )
        })
        .await
        .expect("two full rounds should complete");
        assert!(
            results.0.is_ok() && results.1.is_ok() && results.2.is_ok(),
            "{results:?}"
        );
        assert_eq!(ui_a.results.len(), 2);
        assert_eq!(ui_a.results, ui_b.results);
        assert_eq!(ui_a.results, ui_c.results);
        let old_keys = [&a, &b, &c].map(|game| {
            let sent = game.transport.sent.borrow();
            let keys: Vec<_> = sent
                .iter()
                .filter_map(|message| match message {
                    Message::RevealKeys { e, d } => Some((e, d)),
                    _ => None,
                })
                .collect();
            assert_eq!(keys.len(), 4, "two broadcasts per round");
            assert_ne!(keys[0].0, keys[2].0, "fresh round key");
            SraKeyPair {
                e: BigUint::parse_bytes(keys[0].0.as_bytes(), 16).unwrap(),
                d: BigUint::parse_bytes(keys[0].1.as_bytes(), 16).unwrap(),
                p: game.prime.clone(),
            }
        });
        let sent = c.transport.sent.borrow();
        let decks: Vec<_> = sent
            .iter()
            .filter_map(|message| match message {
                Message::ShuffledDeck { .. } => Some(message_to_deck(message, &c.prime).unwrap()),
                _ => None,
            })
            .collect();
        assert!(
            verify_hand(&decks[2], [&old_keys[0], &old_keys[1], &old_keys[2]]).is_err(),
            "previous revealed keys cannot decode the next round"
        );
    }

    #[tokio::test]
    async fn pending_input_retains_messages_and_observes_quit() {
        let [game, peer, _] = sessions();
        peer.send_to(0, &Message::NextRound).await.unwrap();
        let choice = async {
            tokio::task::yield_now().await;
            Ok(42)
        };
        assert_eq!(game.input(choice).await.unwrap(), 42);
        assert!(matches!(
            game.recv_from_or_quit(1).await.unwrap(),
            Message::NextRound
        ));
        peer.send_to(0, &Message::Quit).await.unwrap();
        let result = game.input(std::future::pending::<Result<()>>()).await;
        assert!(result.unwrap_err().to_string().contains("left the game"));
    }

    #[tokio::test]
    async fn bidding_rejects_forged_sender_seat() {
        let [game, peer, _] = sessions();
        peer.send_to(
            0,
            &Message::BidMsg {
                seat: 2,
                action: BidActionMsg::Pass,
            },
        )
        .await
        .unwrap();
        let mut state = GameState::new(Seat::Forehand);
        let error = game
            .bidding_phase(&mut state, &mut Bot::default())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("does not match sender"));
    }

    #[tokio::test]
    async fn playing_rejects_malformed_card_without_panicking() {
        let [game, peer, _] = sessions();
        let mut state = GameState::new(Seat::Forehand);
        state.contract = Some(Contract::new(GameType::Grand, Modifiers::default()));
        state.trick_leader = Seat::Middlehand;
        peer.send_to(
            0,
            &Message::PlayCard {
                card: CardMsg { suit: 255, rank: 0 },
            },
        )
        .await
        .unwrap();
        assert!(game
            .playing_phase(&mut state, &mut Bot::default())
            .await
            .unwrap_err()
            .to_string()
            .contains("Invalid suit"));
    }

    #[test]
    fn invalid_cards_suits_decks_and_numbers_return_errors() {
        assert!(CardMsg { suit: 4, rank: 0 }.to_card().is_err());
        assert!(CardMsg { suit: 0, rank: 8 }.to_card().is_err());
        assert!(msg_to_game_type(crate::net::message::GameTypeMsg::Suit(4)).is_err());
        let prime = BigUint::from(7919u32);
        for bad in ["", "0", "-1", "+1", "g", "ffff", "0000000000000001"] {
            assert!(parse_number(bad, &prime).is_err(), "{bad}");
        }
        assert!(message_to_deck(
            &Message::ShuffledDeck {
                cards: vec!["1".into(); 31]
            },
            &prime
        )
        .is_err());
        let partial = Message::PartialDecrypt {
            target: 0,
            cards: vec!["1".into()],
            indices: vec![32],
        };
        assert!(extract_partial_decrypt(&partial, 0, &[0], &prime).is_err());
    }
}
