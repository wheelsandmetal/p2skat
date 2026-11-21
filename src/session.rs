//! Shared game flow for terminal and browser frontends.
//!
//! # Deal (5 sequential hops)
//!
//! Keys A, B, C belong to seats 0, 1, 2. `F` is the final deck, `H0`, `H1`,
//! `H2` its hand positions. SRA layers commute, so hands are dealt in parallel
//! and layers are stripped in a chain helper → helper → owner:
//!
//! 1. 0 → 1: deck encrypted and shuffled by A.
//! 2. 1 → 2: deck encrypted and shuffled by B.
//! 3. 2 → 0, 1: `F` (encrypted and shuffled by C). Also 2 → 1: `C⁻¹(H0)`,
//!    2 → 0: `C⁻¹(H1)`.
//! 4. 1 → 0: `B⁻¹C⁻¹(H0)`; 0 opens hand 0. 0 → 1: `A⁻¹(H2)` taken from `F`,
//!    then `A⁻¹C⁻¹(H1)`; 1 opens hand 1. Seats 0 and 1 both send before they
//!    receive, so this concurrent exchange cannot deadlock.
//! 5. 1 → 2: `B⁻¹A⁻¹(H2)`; 2 opens hand 2.
//!
//! The skat stays triply encrypted. Hand 2 cannot finish sooner: `F` exists
//! only after hop 2, and two other layers must then come off in sequence.
//!
//! # Skat pickup (2 hops after the request)
//!
//! Right after bidding, the first helper `(d+1)%3` strips its layer from the
//! skat positions of its own copy of `F` and sends them to the second helper
//! `(d+2)%3`. If the declarer `d` picks up, it broadcasts `SkatRequest` (no
//! ciphertexts); the second helper strips its layer and sends the result to
//! `d`, who opens the skat.
//!
//! # Decryption oracle
//!
//! No helper ever decrypts ciphertexts chosen by the player who receives the
//! result. A helper either takes its input from fixed positions of its own
//! copy of `F`, or strips a vector received from another helper and passes the
//! result on to a third player. Every received partial decryption is recorded
//! and recomputed from the revealed keys in `reveal_phase`; a mismatch is
//! reported as cheating. Those checks also catch seat 2 sending seats 0 and 1
//! different final decks: the hand vectors pass between 0 and 1, so their two
//! copies must agree on positions 0..30, and since both decrypt to a full deck
//! the two skat ciphertexts are then the same pair.
use std::cell::RefCell;
use std::collections::VecDeque;
use std::future::Future;
use std::ops::Range;

use crate::crypto::protocol::{
    encrypt_and_shuffle, in_card_group, verify_hand, CardCodec, DealPositions,
};
use crate::crypto::sra::SraKeyPair;
use crate::game::bidding::{BidAction, BiddingState, Seat};
use crate::game::card::Card;
use crate::game::contract::{Contract, GameType, Modifiers};
use crate::game::scoring::{count_points, game_value, null_value, ScoreBoard};
use crate::game::state::{GameState, Phase};
use crate::game::trick::{legal_plays, Trick};
use crate::net::message::{protocol_mismatch, Message, PROTOCOL_VERSION};
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
    fn set_contract(&mut self, _contract: &Contract, _declarer: usize, _bid: u32) {}
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
    codec: CardCodec,
    pending: RefCell<VecDeque<(usize, Message)>>,
    round: RefCell<RoundRecord>,
}

/// What this player received and opened during one round, kept until the keys
/// are revealed so every step can be recomputed and checked.
#[derive(Default)]
struct RoundRecord {
    received: Vec<Received>,
    /// Cards this player decrypted, with their final-deck positions.
    opened: Vec<(usize, Card)>,
    /// The hand another player showed when declaring an ouvert game.
    ouvert_hand: Option<Vec<Card>>,
}

/// A partial decryption received from `from`: final-deck `indices` with the
/// layers of the `stripped` seats removed.
struct Received {
    from: usize,
    indices: Range<usize>,
    stripped: Vec<usize>,
    values: Vec<BigUint>,
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
        anyhow::ensure!(key.p == prime, "Invalid game prime");
        let codec = CardCodec::new(&prime).map_err(anyhow::Error::msg)?;
        Ok(Self {
            my_seat,
            my_name,
            names,
            key,
            prime,
            transport,
            codec,
            pending: RefCell::new(VecDeque::new()),
            round: RefCell::new(RoundRecord::default()),
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
    /// Every transport starts here, so mismatched builds fail before dealing.
    async fn check_protocol(&self) -> Result<()> {
        self.broadcast(&Message::Protocol {
            version: PROTOCOL_VERSION,
        })
        .await?;
        for seat in (0..3).filter(|seat| *seat != self.my_seat) {
            match self.recv_from_or_quit(seat).await? {
                Message::Protocol { version } if version == PROTOCOL_VERSION => {}
                Message::Protocol { version } => {
                    anyhow::bail!(protocol_mismatch(&self.names[seat], version))
                }
                _ => anyhow::bail!(protocol_mismatch(&self.names[seat], 0)),
            }
        }
        Ok(())
    }
    async fn run_game_inner(&mut self, ui: &mut impl Frontend) -> Result<()> {
        self.check_protocol().await?;
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
            ui.show_status("Bidding")?;

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
            ui.set_contract(&contract, winner_seat.index(), final_bid);
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

            let (value, decl_points) = score_revealed_hand(&state, &revealed_deck, final_bid)?;
            verify_plays(&revealed_deck, &plays, &contract, declarer_idx)?;
            if let Some(shown) = &self.round.borrow().ouvert_hand {
                verify_ouvert_hand(&revealed_deck, &plays, &contract, declarer_idx, shown)
                    .map_err(|error| {
                        anyhow::anyhow!("Cheating detected: {} {error}", self.names[declarer_idx])
                    })?;
            }

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
        *self.round.borrow_mut() = RoundRecord::default();
        let initial = self.codec.deck();

        match self.my_seat {
            0 => {
                // I'm first: encrypt, shuffle, send to seat 1
                let shuffled = encrypt_and_shuffle(&initial, &self.key);
                let msg = deck_to_message(&shuffled);
                self.send_to(1, &msg).await?;

                // Wait for final deck from seat 2
                self.recv_deck(2).await
            }
            1 => {
                // Wait for deck from seat 0
                let deck = self.recv_deck(0).await?;

                // Encrypt and shuffle, send to seat 2
                let shuffled = encrypt_and_shuffle(&deck, &self.key);
                let msg = deck_to_message(&shuffled);
                self.send_to(2, &msg).await?;

                // Wait for final deck from seat 2
                self.recv_deck(2).await
            }
            2 => {
                // Wait for deck from seat 1
                let deck = self.recv_deck(1).await?;

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

    /// Receive a shuffled deck from `seat` and check every value before using it.
    async fn recv_deck(&self, seat: usize) -> Result<Vec<BigUint>> {
        let msg = self.recv_from_or_quit(seat).await?;
        let deck = message_to_deck(&msg, &self.prime)?;
        self.check_card_group(seat, &deck)?;
        Ok(deck)
    }

    /// Every received deck value and partial decryption must be an honest-looking
    /// encrypted card (see [`in_card_group`]), and no value may repeat: SRA layers
    /// are bijections, so distinct cards stay distinct.
    fn check_card_group(&self, seat: usize, values: &[BigUint]) -> Result<()> {
        for (i, value) in values.iter().enumerate() {
            anyhow::ensure!(
                in_card_group(value, &self.prime),
                "Cheating detected: {} sent a value outside the card group",
                self.names[seat]
            );
            anyhow::ensure!(
                !values[..i].contains(value),
                "Cheating detected: {} sent the same encrypted card twice",
                self.names[seat]
            );
        }
        Ok(())
    }

    /// Deal phase, hops 3–5 of the schedule in the module docs. Returns this
    /// player's hand in deck-position order.
    async fn deal_phase(&self, deck: &[BigUint]) -> Result<Vec<Card>> {
        anyhow::ensure!(deck.len() == 32, "Expected 32 encrypted cards");
        let hand = DealPositions::player_range;
        let own = match self.my_seat {
            0 => {
                // Hop 4. Hand 2 starts its chain here, straight from the deck.
                self.send_partial(1, hand(2), &[0], &self.strip(&deck[hand(2)]))
                    .await?;
                let hand1 = self.recv_partial(2, hand(1), &[2]).await?;
                self.send_partial(1, hand(1), &[2, 0], &self.strip(&hand1))
                    .await?;
                self.recv_partial(1, hand(0), &[2, 1]).await?
            }
            1 => {
                // Hop 4: finish hand 0 for seat 0.
                let hand0 = self.recv_partial(2, hand(0), &[2]).await?;
                self.send_partial(0, hand(0), &[2, 1], &self.strip(&hand0))
                    .await?;
                // Hop 5: finish hand 2 for seat 2.
                let hand2 = self.recv_partial(0, hand(2), &[0]).await?;
                self.send_partial(2, hand(2), &[0, 1], &self.strip(&hand2))
                    .await?;
                self.recv_partial(0, hand(1), &[2, 0]).await?
            }
            2 => {
                // Hop 3, right behind the final deck.
                self.send_partial(1, hand(0), &[2], &self.strip(&deck[hand(0)]))
                    .await?;
                self.send_partial(0, hand(1), &[2], &self.strip(&deck[hand(1)]))
                    .await?;
                self.recv_partial(1, hand(2), &[0, 1]).await?
            }
            _ => unreachable!(),
        };
        self.open_cards(hand(self.my_seat), &own)
    }

    /// Remove this player's layer.
    fn strip(&self, values: &[BigUint]) -> Vec<BigUint> {
        values.iter().map(|value| self.key.decrypt(value)).collect()
    }

    async fn send_partial(
        &self,
        seat: usize,
        indices: Range<usize>,
        stripped: &[usize],
        values: &[BigUint],
    ) -> Result<()> {
        debug_assert_eq!(stripped.last(), Some(&self.my_seat));
        debug_assert_eq!(indices.len(), values.len());
        self.send_to(
            seat,
            &Message::PartialDecrypt {
                indices: indices.collect(),
                stripped: stripped.to_vec(),
                cards: biguints_to_hex(values),
            },
        )
        .await
    }

    /// Receive the one partial decryption expected from `seat` at this step and
    /// record it for the check after the key reveal.
    async fn recv_partial(
        &self,
        seat: usize,
        indices: Range<usize>,
        stripped: &[usize],
    ) -> Result<Vec<BigUint>> {
        let msg = self.recv_from_or_quit(seat).await?;
        let values = extract_partial_decrypt(&msg, &indices, stripped, &self.prime)?;
        self.check_card_group(seat, &values)?;
        self.round.borrow_mut().received.push(Received {
            from: seat,
            indices,
            stripped: stripped.to_vec(),
            values: values.clone(),
        });
        Ok(values)
    }

    /// Remove the last layer (mine) and decode; remembers what was opened.
    fn open_cards(&self, indices: Range<usize>, values: &[BigUint]) -> Result<Vec<Card>> {
        let cards = self
            .strip(values)
            .iter()
            .map(|value| {
                self.codec
                    .decode(value)
                    .ok_or_else(|| anyhow::anyhow!("Decrypted value is not a card"))
            })
            .collect::<Result<Vec<_>>>()?;
        ensure_unique_cards(&cards)?;
        self.round
            .borrow_mut()
            .opened
            .extend(indices.zip(cards.iter().copied()));
        Ok(cards)
    }

    /// Skat helpers for declarer `d`: the first strips its layer before the
    /// declarer decides, the second finishes on request.
    fn skat_helpers(declarer: usize) -> (usize, usize) {
        ((declarer + 1) % 3, (declarer + 2) % 3)
    }

    /// Run by every player right after bidding. The second helper returns the
    /// skat with the first helper's layer removed.
    async fn prepare_skat(
        &self,
        declarer: usize,
        deck: &[BigUint],
    ) -> Result<Option<Vec<BigUint>>> {
        let (first, second) = Self::skat_helpers(declarer);
        let skat = DealPositions::skat_range();
        if self.my_seat == first {
            self.send_partial(second, skat.clone(), &[first], &self.strip(&deck[skat]))
                .await?;
        } else if self.my_seat == second {
            return Ok(Some(self.recv_partial(first, skat, &[first]).await?));
        }
        Ok(None)
    }

    /// Declarer: request the skat and open it (2 hops).
    async fn pick_up_skat(&self) -> Result<Vec<Card>> {
        self.broadcast(&Message::SkatRequest).await?;
        let (first, second) = Self::skat_helpers(self.my_seat);
        let skat = DealPositions::skat_range();
        let values = self
            .recv_partial(second, skat.clone(), &[first, second])
            .await?;
        self.open_cards(skat, &values)
    }

    /// Second helper, on `SkatRequest`: strip my layer and send to the declarer.
    async fn release_skat(&self, declarer: usize, prepared: Option<&[BigUint]>) -> Result<()> {
        if let Some(values) = prepared {
            let (first, second) = Self::skat_helpers(declarer);
            self.send_partial(
                declarer,
                DealPositions::skat_range(),
                &[first, second],
                &self.strip(values),
            )
            .await?;
        }
        Ok(())
    }

    /// Recompute every received partial decryption and every opened card from
    /// the revealed keys.
    fn verify_round(&self, deck: &[BigUint], keys: &[SraKeyPair], revealed: &[Card]) -> Result<()> {
        let round = self.round.borrow();
        for received in &round.received {
            let mut expected = deck[received.indices.clone()].to_vec();
            for &seat in &received.stripped {
                expected = expected
                    .iter()
                    .map(|value| keys[seat].decrypt(value))
                    .collect();
            }
            anyhow::ensure!(
                expected == received.values,
                "Cheating detected: {} sent a partial decryption of deck positions {:?} \
                 that does not match the revealed keys",
                self.names[received.from],
                received.indices
            );
        }
        for &(position, card) in &round.opened {
            anyhow::ensure!(
                revealed[position] == card,
                "Cheating detected: the card dealt to you at deck position {} differs \
                 from the revealed deck",
                position
            );
        }
        Ok(())
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
                let skat_cards = self.pick_up_skat().await?;

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
                validate_contract(&contract, false, bid)?;

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
                validate_contract(&contract, true, bid)?;

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
            // Strip the first layer now, while the declarer decides.
            let prepared = self
                .prepare_skat(declarer.index(), &state.encrypted_deck)
                .await?;
            // Wait for declarer's actions.
            // Might get SkatRequest or Declaration.
            let mut picked_up = false;
            loop {
                let msg = self.recv_from_or_quit(declarer.index()).await?;
                match msg {
                    Message::SkatRequest => {
                        anyhow::ensure!(!picked_up, "Duplicate skat request");
                        picked_up = true;
                        self.release_skat(declarer.index(), prepared.as_deref())
                            .await?;
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
                            // Checked against the revealed deck after play.
                            self.round.borrow_mut().ouvert_hand = Some(cards);
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
                        validate_contract(&contract, !picked_up, bid)?;
                        ui.show_status(&format!(
                            "{} declares {}{}",
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
        self.verify_round(encrypted_deck, &key_pairs, &cards)?;
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

/// An ouvert hand is the ten cards the declarer plays: in a Hand game exactly
/// the dealt hand. The error completes "Cheating detected: <declarer> …".
fn verify_ouvert_hand(
    deck: &[Card],
    plays: &[(usize, Card)],
    contract: &Contract,
    declarer: usize,
    shown: &[Card],
) -> Result<()> {
    let held: Vec<Card> = if contract.modifiers.hand {
        deck[DealPositions::player_range(declarer)].to_vec()
    } else {
        plays
            .iter()
            .filter(|(seat, _)| *seat == declarer)
            .map(|(_, card)| *card)
            .collect()
    };
    anyhow::ensure!(
        shown.len() == held.len() && shown.iter().all(|card| held.contains(card)),
        "showed an ouvert hand that differs from the cards they held"
    );
    Ok(())
}

fn score_revealed_hand(state: &GameState, deck: &[Card], bid: u32) -> Result<(i32, u32)> {
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
        bid,
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
    expected_indices: &Range<usize>,
    expected_stripped: &[usize],
    prime: &BigUint,
) -> Result<Vec<BigUint>> {
    match msg {
        Message::PartialDecrypt {
            indices,
            stripped,
            cards,
        } => {
            anyhow::ensure!(
                indices.iter().copied().eq(expected_indices.clone())
                    && stripped == expected_stripped
                    && cards.len() == expected_indices.len(),
                "Unexpected partial decryption: positions {:?} without layers {:?}, \
                 expected positions {:?} without layers {:?}",
                indices,
                stripped,
                expected_indices,
                expected_stripped
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

fn validate_contract(contract: &Contract, hand: bool, bid: u32) -> Result<()> {
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
    // Null has a fixed value, so a Null below the bid is known to be overbid before play.
    anyhow::ensure!(
        contract.game_type != GameType::Null || null_value(modifiers) >= bid,
        "Null is worth less than the bid"
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
    fn ouvert_hand_must_match_the_revealed_deck() {
        let (_, deck) = revealed_round(true, 1);
        let ouvert = Modifiers {
            hand: true,
            ouvert: true,
            ..Modifiers::default()
        };
        let contract = Contract::new(GameType::Grand, ouvert);
        let mut shown = deck[DealPositions::player_range(0)].to_vec();
        shown.reverse();
        assert!(verify_ouvert_hand(&deck, &[], &contract, 0, &shown).is_ok());
        // Showing a skat card instead of a held one is a lie about the hand.
        shown[4] = deck[DealPositions::skat_range()][0];
        let error = verify_ouvert_hand(&deck, &[], &contract, 0, &shown).unwrap_err();
        assert!(error
            .to_string()
            .contains("differs from the cards they held"));
        assert!(verify_ouvert_hand(&deck, &[], &contract, 1, &deck[..10]).is_err());
        // Without Hand (not declarable today) the shown cards are the ones played.
        let null_ouvert = Modifiers {
            ouvert: true,
            ..Modifiers::default()
        };
        let contract = Contract::new(GameType::Null, null_ouvert);
        let plays: Vec<(usize, Card)> = deck[20..30].iter().map(|card| (2, *card)).collect();
        assert!(verify_ouvert_hand(&deck, &plays, &contract, 2, &deck[20..30]).is_ok());
        assert!(verify_ouvert_hand(&deck, &plays, &contract, 2, &deck[19..29]).is_err());
    }

    #[test]
    fn peers_score_pickup_from_original_hand_and_reconstructed_discards() {
        for my_seat in 0..3 {
            let (state, deck) = revealed_round(false, my_seat);
            // CJ and SJ were discarded: still with two, despite winning neither jack.
            assert_eq!(score_revealed_hand(&state, &deck, 18).unwrap(), (72, 68));
        }
    }

    #[test]
    fn peers_include_untouched_skat_in_hand_game_points() {
        for my_seat in 0..3 {
            let (state, deck) = revealed_round(true, my_seat);
            // Twenty unseen skat points lift the declarer from 44 to 64.
            assert_eq!(score_revealed_hand(&state, &deck, 18).unwrap(), (96, 64));
        }
    }

    #[test]
    fn scoring_rejects_duplicate_or_missing_played_cards() {
        let (mut state, deck) = revealed_round(false, 1);
        state.tricks_won[1][0] = state.tricks_won[0][0];
        assert!(score_revealed_hand(&state, &deck, 18).is_err());

        let (mut state, deck) = revealed_round(false, 1);
        state.tricks_won[1].truncate(9);
        assert!(score_revealed_hand(&state, &deck, 18).is_err());
    }

    #[test]
    fn peers_score_overbid_and_lost_games_identically() {
        for my_seat in 0..3 {
            // Grand with 2 is worth 72: overbid at 77, so lost at 4 * 24, doubled.
            let (state, deck) = revealed_round(false, my_seat);
            assert_eq!(score_revealed_hand(&state, &deck, 77).unwrap(), (-192, 68));
        }
    }

    #[test]
    fn null_below_the_bid_cannot_be_declared() {
        let null = |hand| {
            Contract::new(
                GameType::Null,
                Modifiers {
                    hand,
                    ..Modifiers::default()
                },
            )
        };
        assert!(validate_contract(&null(false), false, 23).is_ok());
        assert!(validate_contract(&null(false), false, 24).is_err());
        assert!(validate_contract(&null(true), true, 35).is_ok());
        assert!(validate_contract(&null(true), true, 36).is_err());
        let grand = Contract::new(GameType::Grand, Modifiers::default());
        assert!(validate_contract(&grand, false, 264).is_ok());
    }

    #[test]
    fn scoring_rejects_playing_the_skat_in_a_hand_game() {
        let (mut state, deck) = revealed_round(true, 1);
        state.tricks_won[0][4] = deck[30];
        assert!(score_revealed_hand(&state, &deck, 18).is_err());
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod session_tests {
    use super::*;
    use crate::net::message::{BidActionMsg, CardMsg};
    use std::cell::Cell;
    use tokio::sync::{mpsc, Mutex};

    type Tamper = Box<dyn FnMut(usize, &mut Message)>;
    /// (sender, message, hop depth)
    type Envelope = (usize, Message, u32);

    struct MemoryTransport {
        seat: usize,
        senders: [mpsc::UnboundedSender<Envelope>; 3],
        receiver: Mutex<mpsc::UnboundedReceiver<Envelope>>,
        sent: RefCell<Vec<Message>>,
        /// Sequential hops so far: a message's depth is one more than the
        /// deepest message its sender had received when sending it.
        hops: Cell<u32>,
        /// Lets a test play a cheating peer by rewriting outgoing messages.
        tamper: RefCell<Option<Tamper>>,
    }
    impl Transport for MemoryTransport {
        async fn send_to(&self, seat: usize, message: &Message) -> Result<()> {
            let mut message = message.clone();
            if let Some(tamper) = self.tamper.borrow_mut().as_mut() {
                tamper(seat, &mut message);
            }
            self.sent.borrow_mut().push(message.clone());
            self.senders[seat].send((self.seat, message, self.hops.get() + 1))?;
            Ok(())
        }
        async fn recv_any(&self) -> Result<(usize, Message)> {
            let (seat, message, depth) = self
                .receiver
                .lock()
                .await
                .recv()
                .await
                .ok_or_else(|| anyhow::anyhow!("Disconnected"))?;
            self.hops.set(self.hops.get().max(depth));
            Ok((seat, message))
        }
    }
    fn sessions() -> [Session<MemoryTransport>; 3] {
        sessions_with((BigUint::from(1u8) << 127usize) - BigUint::from(1u8))
    }
    /// 2^128 + 0x30a3 is a safe prime (like the real 2048-bit one) and ≡ 3 mod 4,
    /// so -1 is a quadratic non-residue.
    fn safe_prime() -> BigUint {
        (BigUint::from(1u8) << 128usize) + BigUint::from(0x30a3u32)
    }
    fn sessions_with(prime: BigUint) -> [Session<MemoryTransport>; 3] {
        let channels: [_; 3] = std::array::from_fn(|_| mpsc::unbounded_channel());
        let senders = channels.each_ref().map(|(sender, _)| sender.clone());
        let mut seat = 0;
        channels.map(|(_, receiver)| {
            let transport = MemoryTransport {
                seat,
                senders: senders.clone(),
                receiver: Mutex::new(receiver),
                sent: RefCell::new(Vec::new()),
                hops: Cell::new(0),
                tamper: RefCell::new(None),
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
        /// Play one Grand Hand Ouvert round instead of two plain rounds.
        ouvert: bool,
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
            Ok(self.rounds == 1 && !self.ouvert)
        }
        async fn choose_skat_discard(&mut self, hand: &[Card]) -> Result<Vec<Card>> {
            Ok(hand[..2].to_vec())
        }
        async fn declare_contract(&mut self, _: &[Card], _: u32, hand: bool) -> Result<Contract> {
            Ok(Contract::new(
                GameType::Grand,
                Modifiers {
                    hand,
                    ouvert: self.ouvert,
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
            Ok(self.rounds < 2 && !self.ouvert)
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

    async fn deal(session: &mut Session<MemoryTransport>) -> Result<(Vec<BigUint>, Vec<Card>)> {
        let deck = session.shuffle_phase().await?;
        let hand = session.deal_phase(&deck).await?;
        Ok((deck, hand))
    }
    fn hop_counts(games: [&Session<MemoryTransport>; 3]) -> [u32; 3] {
        games.map(|game| game.transport.hops.get())
    }
    fn reset_hops(games: [&Session<MemoryTransport>; 3]) {
        for game in games {
            game.transport.hops.set(0);
        }
    }
    fn timeout<F: Future>(future: F) -> tokio::time::Timeout<F> {
        tokio::time::timeout(std::time::Duration::from_secs(10), future)
    }

    #[tokio::test]
    async fn deal_takes_five_hops_and_gives_disjoint_hands() {
        let [mut a, mut b, mut c] = sessions();
        let (deal_a, deal_b, deal_c) =
            timeout(async { tokio::try_join!(deal(&mut a), deal(&mut b), deal(&mut c)) })
                .await
                .expect("deal should complete")
                .unwrap();
        // Seats 0 and 1 hold their hands after hop 4 (the in-memory clock may
        // count a buffered hop-4 message early), seat 2 after hop 5.
        let hops = hop_counts([&a, &b, &c]);
        assert_eq!(hops.iter().max(), Some(&5), "{hops:?}");
        assert_eq!(hops[2], 5, "{hops:?}");

        assert_eq!(deal_a.0, deal_b.0);
        assert_eq!(deal_a.0, deal_c.0);
        let decoded = verify_hand(&deal_a.0, [&a.key, &b.key, &c.key]).unwrap();
        let hands = [&deal_a.1, &deal_b.1, &deal_c.1];
        let mut all: Vec<Card> = decoded[DealPositions::skat_range()].to_vec();
        for (seat, hand) in hands.iter().enumerate() {
            assert_eq!(hand.len(), 10);
            assert_eq!(**hand, decoded[DealPositions::player_range(seat)]);
            all.extend(hand.iter());
        }
        ensure_unique_cards(&all).unwrap();
        assert_eq!(all.len(), 32);

        // Every partial decryption a player received passes the reveal check.
        let reveal = timeout(async {
            tokio::try_join!(
                a.reveal_phase(&deal_a.0),
                b.reveal_phase(&deal_b.0),
                c.reveal_phase(&deal_c.0)
            )
        });
        reveal.await.unwrap().unwrap();
        assert_eq!(a.round.borrow().received.len(), 2);
        assert_eq!(b.round.borrow().received.len(), 3);
        assert_eq!(c.round.borrow().received.len(), 1);
    }

    #[tokio::test]
    async fn reveal_detects_a_tampered_partial_decryption() {
        let [mut a, mut b, mut c] = sessions();
        // Seat 0 cheats on the hand-2 vector it passes to seat 1 (hop 4): it
        // swaps two values, so seat 2 still gets a plausible hand.
        *a.transport.tamper.borrow_mut() = Some(Box::new(|seat, message| {
            if let Message::PartialDecrypt { indices, cards, .. } = message {
                if seat == 1 && indices[0] == 20 {
                    cards.swap(0, 1);
                }
            }
        }));
        let results = timeout(async {
            tokio::join!(
                deal_and_reveal(&mut a),
                deal_and_reveal(&mut b),
                deal_and_reveal(&mut c)
            )
        })
        .await
        .expect("round should complete");
        assert!(results.0.is_ok(), "the cheater's own checks pass");
        let error_b = results.1.unwrap_err().to_string();
        assert!(
            error_b.contains("Cheating detected: Player 0") && error_b.contains("20..30"),
            "{error_b}"
        );
        let error_c = results.2.unwrap_err().to_string();
        assert!(error_c.contains("Cheating detected"), "{error_c}");
    }

    #[tokio::test]
    async fn a_sign_flipped_deck_value_is_rejected_at_once() {
        let prime = safe_prime();
        let [mut a, mut b, _c] = sessions_with(prime.clone());
        // Seat 0 tags one card as -c: SRA keeps the Legendre symbol, so seat 0
        // could follow that card through every later shuffle.
        let tamper_prime = prime.clone();
        *a.transport.tamper.borrow_mut() = Some(Box::new(move |_, message| {
            if let Message::ShuffledDeck { cards } = message {
                let value = BigUint::parse_bytes(cards[0].as_bytes(), 16).unwrap();
                cards[0] = (&tamper_prime - value).to_str_radix(16);
            }
        }));
        let error = timeout(async {
            tokio::select! {
                result = b.shuffle_phase() => result,
                _ = a.shuffle_phase() => unreachable!("seat 0 waits for seat 2"),
            }
        })
        .await
        .expect("seat 1 decides without waiting")
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "Cheating detected: Player 0 sent a value outside the card group"
        );
    }

    #[tokio::test]
    async fn partials_must_be_distinct_values_in_the_card_group() {
        let prime = safe_prime();
        let [_a, b, c] = sessions_with(prime.clone());
        let codec = CardCodec::new(&prime).unwrap();
        let hand0 = DealPositions::player_range(0);
        let honest: Vec<BigUint> = codec.deck()[hand0.clone()]
            .iter()
            .map(|value| c.key.encrypt(value))
            .collect();
        let mut flipped = honest.clone();
        flipped[3] = &prime - &flipped[3];
        let mut one = honest.clone();
        one[0] = BigUint::from(1u8);
        let mut repeated = honest.clone();
        repeated[9] = repeated[2].clone();
        for (values, expected) in [
            (flipped, "Player 2 sent a value outside the card group"),
            (one, "Player 2 sent a value outside the card group"),
            (repeated, "Player 2 sent the same encrypted card twice"),
        ] {
            c.send_partial(1, hand0.clone(), &[2], &values)
                .await
                .unwrap();
            let error = b.recv_partial(2, hand0.clone(), &[2]).await.unwrap_err();
            assert_eq!(error.to_string(), format!("Cheating detected: {expected}"));
        }
        c.send_partial(1, hand0.clone(), &[2], &honest)
            .await
            .unwrap();
        assert_eq!(b.recv_partial(2, hand0, &[2]).await.unwrap(), honest);
    }

    #[tokio::test]
    async fn rounds_complete_with_a_safe_prime() {
        let [mut a, mut b, mut c] = sessions_with(safe_prime());
        timeout(async {
            tokio::try_join!(
                deal_and_reveal(&mut a),
                deal_and_reveal(&mut b),
                deal_and_reveal(&mut c)
            )
        })
        .await
        .expect("round should complete")
        .unwrap();
    }

    #[tokio::test]
    async fn helpers_reject_a_partial_for_other_positions_or_layers() {
        let [game, peer, _] = sessions();
        // A peer cannot get a hand-0 helper step applied to the skat.
        peer.send_partial(
            0,
            DealPositions::skat_range(),
            &[1],
            &vec![BigUint::from(5u8); 2],
        )
        .await
        .unwrap();
        let error = game
            .recv_partial(1, DealPositions::player_range(0), &[2, 1])
            .await
            .unwrap_err();
        assert!(error.to_string().contains("Unexpected partial decryption"));
    }

    #[tokio::test]
    async fn skat_pickup_takes_two_hops_and_is_checked_at_reveal() {
        let [mut a, mut b, mut c] = sessions();
        let (deal_a, deal_b, deal_c) =
            timeout(async { tokio::try_join!(deal(&mut a), deal(&mut b), deal(&mut c)) })
                .await
                .unwrap()
                .unwrap();
        let deck = deal_a.0.clone();
        reset_hops([&a, &b, &c]);

        // Seat 1 declares: seat 2 strips first, seat 0 finishes.
        let declarer = async { b.pick_up_skat().await };
        let first_helper = async {
            assert!(c.prepare_skat(1, &deal_c.0).await?.is_none());
            anyhow::ensure!(matches!(
                c.recv_from_or_quit(1).await?,
                Message::SkatRequest
            ));
            Ok(())
        };
        let second_helper = async {
            let prepared = a.prepare_skat(1, &deal_a.0).await?;
            anyhow::ensure!(matches!(
                a.recv_from_or_quit(1).await?,
                Message::SkatRequest
            ));
            a.release_skat(1, prepared.as_deref()).await
        };
        let (skat, _, _) =
            timeout(async { tokio::try_join!(declarer, first_helper, second_helper) })
                .await
                .unwrap()
                .unwrap();
        assert_eq!(b.transport.hops.get(), 2);
        let decoded = verify_hand(&deck, [&a.key, &b.key, &c.key]).unwrap();
        assert_eq!(skat, decoded[DealPositions::skat_range()]);

        let reveal = timeout(async {
            tokio::try_join!(
                a.reveal_phase(&deal_a.0),
                b.reveal_phase(&deal_b.0),
                c.reveal_phase(&deal_c.0)
            )
        });
        reveal.await.unwrap().unwrap();
        // Seat 0 checks seat 2's step; seat 1 checks the result it opened.
        assert_eq!(a.round.borrow().received.len(), 3);
        assert_eq!(b.round.borrow().received.len(), 4);
    }

    #[tokio::test]
    async fn mismatched_protocol_versions_fail_clearly() {
        let [game, old, newer] = sessions();
        // Seat 1 runs a build from before versioning and starts dealing.
        old.send_to(0, &Message::ShuffledDeck { cards: vec![] })
            .await
            .unwrap();
        newer
            .send_to(
                0,
                &Message::Protocol {
                    version: PROTOCOL_VERSION + 1,
                },
            )
            .await
            .unwrap();
        let error = game.check_protocol().await.unwrap_err().to_string();
        assert!(error.contains("Player 1 runs an older p2skat"), "{error}");

        let [game, peer, other] = sessions();
        let current = Message::Protocol {
            version: PROTOCOL_VERSION,
        };
        peer.send_to(0, &current).await.unwrap();
        other
            .send_to(
                0,
                &Message::Protocol {
                    version: PROTOCOL_VERSION + 1,
                },
            )
            .await
            .unwrap();
        let error = game.check_protocol().await.unwrap_err().to_string();
        assert!(
            error.contains("Player 2 runs p2skat protocol v3"),
            "{error}"
        );
    }

    async fn play_ouvert_round(lie: bool) -> [Result<()>; 3] {
        let [mut a, mut b, mut c] = sessions();
        if lie {
            // Seat 0 (the declarer when everyone passes) shows a card it does not hold.
            *a.transport.tamper.borrow_mut() = Some(Box::new(|_, message| {
                if let Message::Declaration {
                    revealed_hand: Some(cards),
                    ..
                } = message
                {
                    let held: Vec<Card> =
                        cards.iter().map(|card| card.to_card().unwrap()).collect();
                    let other = (1..=32)
                        .map(|index| Card::from_index(index).unwrap())
                        .find(|card| !held.contains(card))
                        .unwrap();
                    cards[0] = other.into();
                }
            }));
        }
        let [mut ui_a, mut ui_b, mut ui_c] = std::array::from_fn::<_, 3, _>(|_| Bot {
            ouvert: true,
            ..Bot::default()
        });
        let results = timeout(async {
            tokio::join!(
                a.run_game(&mut ui_a),
                b.run_game(&mut ui_b),
                c.run_game(&mut ui_c)
            )
        })
        .await
        .expect("the round should complete");
        [results.0, results.1, results.2]
    }

    #[tokio::test]
    async fn an_ouvert_hand_is_checked_against_the_revealed_deck() {
        for result in play_ouvert_round(false).await {
            result.unwrap();
        }
        let [_, b, c] = play_ouvert_round(true).await;
        for error in [b.unwrap_err(), c.unwrap_err()] {
            assert_eq!(
                error.to_string(),
                "Cheating detected: Player 0 showed an ouvert hand that differs from the cards they held"
            );
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
            indices: vec![32],
            stripped: vec![1],
            cards: vec!["1".into()],
        };
        assert!(extract_partial_decrypt(&partial, &(0..1), &[1], &prime).is_err());
        let partial = Message::PartialDecrypt {
            indices: vec![0],
            stripped: vec![2],
            cards: vec!["1".into()],
        };
        assert!(extract_partial_decrypt(&partial, &(0..1), &[1], &prime).is_err());
        assert!(extract_partial_decrypt(&partial, &(0..2), &[2], &prime).is_err());
        assert!(extract_partial_decrypt(&partial, &(0..1), &[2], &prime).is_ok());
    }
}
