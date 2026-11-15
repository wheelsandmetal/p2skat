//! Browser callbacks carry public display data; keys remain inside the worker.
use std::{cell::RefCell, rc::Rc};

use anyhow::{anyhow, ensure, Result};
use futures::{
    channel::{mpsc, oneshot},
    lock::Mutex,
    StreamExt,
};
use js_sys::Function;
use serde::Deserialize;
use serde_json::{json, Value};
use wasm_bindgen::prelude::*;

use crate::{
    crypto::{
        protocol::{card_values, encrypt_and_shuffle, verify_hand, DealPositions},
        sra::{shared_prime, SraKeyPair},
    },
    game::{
        bidding::BidAction,
        card::{Card, Suit},
        contract::{Contract, GameType, Modifiers},
        scoring::null_value,
        trick::Trick,
    },
    net::message::Message,
    session::{Frontend, Session, Transport},
};

const MAX_MESSAGE: usize = 64 * 1024;

struct Runtime {
    seat: usize,
    send: Function,
    incoming: RefCell<mpsc::Sender<(usize, Message)>>,
    received: Mutex<mpsc::Receiver<(usize, Message)>>,
    actions: RefCell<mpsc::Sender<Value>>,
    chosen: Mutex<mpsc::Receiver<Value>>,
    cancel: RefCell<Option<oneshot::Sender<()>>>,
}

thread_local! {
    static ACTIVE: RefCell<Option<Rc<Runtime>>> = const { RefCell::new(None) };
}

fn js_error(error: impl std::fmt::Display) -> JsValue {
    JsValue::from_str(&error.to_string())
}
fn callback_error(value: JsValue) -> anyhow::Error {
    anyhow!(
        "Browser callback failed: {}",
        value.as_string().unwrap_or_else(|| format!("{value:?}"))
    )
}

fn active() -> std::result::Result<Rc<Runtime>, JsValue> {
    ACTIVE.with(|slot| {
        slot.borrow()
            .clone()
            .ok_or_else(|| js_error("No game is running"))
    })
}

impl Transport for Rc<Runtime> {
    async fn send_to(&self, seat: usize, message: &Message) -> Result<()> {
        let json = serde_json::to_string(message)?;
        ensure!(json.len() <= MAX_MESSAGE, "Outgoing message is too large");
        self.send
            .call2(
                &JsValue::NULL,
                &JsValue::from_f64(seat as f64),
                &JsValue::from_str(&json),
            )
            .map_err(callback_error)?;
        Ok(())
    }
    async fn recv_any(&self) -> Result<(usize, Message)> {
        // Receiver owns the queue; dropping a pending next() leaves messages queued.
        self.received
            .lock()
            .await
            .next()
            .await
            .ok_or_else(|| anyhow!("Connection closed"))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    seat: usize,
    names: [String; 3],
}

/// Start only after the two local ordered data channels are open.
#[wasm_bindgen]
pub async fn run_game(
    config: &str,
    send: Function,
    render: Function,
) -> std::result::Result<(), JsValue> {
    let config: Config = serde_json::from_str(config).map_err(js_error)?;
    ensure_config(&config).map_err(js_error)?;
    if ACTIVE.with(|slot| slot.borrow().is_some()) {
        return Err(js_error("A game is already running"));
    }
    let (incoming, received) = mpsc::channel(128);
    let (actions, chosen) = mpsc::channel(1);
    let (cancel_tx, cancel_rx) = oneshot::channel();
    let runtime = Rc::new(Runtime {
        seat: config.seat,
        send,
        incoming: RefCell::new(incoming),
        received: Mutex::new(received),
        actions: RefCell::new(actions),
        chosen: Mutex::new(chosen),
        cancel: RefCell::new(Some(cancel_tx)),
    });
    ACTIVE.with(|slot| *slot.borrow_mut() = Some(runtime.clone()));
    let result = async {
        let mut ui = BrowserUi {
            runtime: runtime.clone(),
            render,
            next_id: 0,
            render_error: None,
        };
        ui.emit(json!({"type":"ready"}));
        ui.check()?;
        let prime = shared_prime();
        let key = SraKeyPair::generate(&prime);
        let mut session = Session::new(
            config.seat,
            config.names[config.seat].clone(),
            config.names,
            key,
            prime,
            runtime.clone(),
        )?;
        let game = Box::pin(session.run_game(&mut ui));
        let outcome = match futures::future::select(game, cancel_rx).await {
            futures::future::Either::Left((result, _)) => result,
            futures::future::Either::Right(_) => Err(anyhow!("You left the game")),
        };
        outcome
    }
    .await;
    if result.is_err() {
        for seat in 0..3 {
            if seat != runtime.seat {
                let _ = runtime.send_to(seat, &Message::Quit).await;
            }
        }
    }
    ACTIVE.with(|slot| *slot.borrow_mut() = None);
    result.map_err(js_error)
}

fn ensure_config(config: &Config) -> Result<()> {
    ensure!(config.seat < 3, "Seat must be 0, 1, or 2");
    ensure!(
        config
            .names
            .iter()
            .all(|name| !name.trim().is_empty() && name.chars().count() <= 64),
        "Player names must have 1–64 characters"
    );
    Ok(())
}

#[wasm_bindgen]
pub fn deliver_message(seat: usize, payload: &str) -> std::result::Result<(), JsValue> {
    let runtime = active()?;
    if seat >= 3 || seat == runtime.seat {
        return Err(js_error("Invalid sender seat"));
    }
    if payload.len() > MAX_MESSAGE {
        return Err(js_error("Incoming message is too large"));
    }
    let message = serde_json::from_str(payload).map_err(js_error)?;
    runtime
        .incoming
        .borrow_mut()
        .try_send((seat, message))
        .map_err(|_| js_error("Too many queued peer messages"))?;
    Ok(())
}

#[wasm_bindgen]
pub fn submit_action(payload: &str) -> std::result::Result<(), JsValue> {
    if payload.len() > 4096 {
        return Err(js_error("Action is too large"));
    }
    let value: Value = serde_json::from_str(payload).map_err(js_error)?;
    active()?
        .actions
        .borrow_mut()
        .try_send(value)
        .map_err(|_| js_error("An action is already queued"))?;
    Ok(())
}

#[wasm_bindgen]
pub fn cancel_game() {
    if let Ok(runtime) = active() {
        if let Some(cancel) = runtime.cancel.borrow_mut().take() {
            let _ = cancel.send(());
        }
    }
}

#[wasm_bindgen]
pub fn crypto_self_check() -> std::result::Result<String, JsValue> {
    let prime = shared_prime();
    let keys = [
        SraKeyPair::generate(&prime),
        SraKeyPair::generate(&prime),
        SraKeyPair::generate(&prime),
    ];
    let mut deck = card_values();
    for key in &keys {
        deck = encrypt_and_shuffle(&deck, key);
    }
    // Exercise the same pairwise removal used when each player receives a hand.
    for player in 0..3 {
        for value in &deck[DealPositions::player_range(player)] {
            let mut card = value.clone();
            for other in 0..3 {
                if player != other {
                    card = keys[other].decrypt(&card);
                }
            }
            card = keys[player].decrypt(&card);
            let index: u64 = (&card).try_into().map_err(js_error)?;
            if Card::from_index(index).is_none() {
                return Err(js_error("Deal self-check failed"));
            }
        }
    }
    let verified = verify_hand(&deck, [&keys[0], &keys[1], &keys[2]]).map_err(js_error)?;
    Ok(json!({"ok":true,"prime_bits":prime.bits(),"cards":verified.len(),"hand_sizes":[10,10,10],"skat_size":2}).to_string())
}

fn cards(hand: &[Card]) -> Vec<u64> {
    hand.iter().map(|card| card.to_index()).collect()
}
fn trick_json(trick: &Trick) -> Value {
    json!(trick
        .cards
        .iter()
        .map(|(seat, card)| json!({"seat":seat,"card":card.to_index()}))
        .collect::<Vec<_>>())
}
fn contract_json(contract: &Contract) -> Value {
    let (kind, trump) = match contract.game_type {
        GameType::Grand => ("grand", None),
        GameType::Null => ("null", None),
        GameType::Suit(suit) => (
            "suit",
            Some(match suit {
                Suit::Diamonds => "diamonds",
                Suit::Hearts => "hearts",
                Suit::Spades => "spades",
                Suit::Clubs => "clubs",
            }),
        ),
    };
    json!({"game_type":kind,"trump":trump,"hand":contract.modifiers.hand})
}
fn action_card(value: &Value) -> Result<Card> {
    value
        .as_u64()
        .and_then(Card::from_index)
        .ok_or_else(|| anyhow!("Invalid card"))
}

struct BrowserUi {
    runtime: Rc<Runtime>,
    render: Function,
    next_id: u64,
    render_error: Option<anyhow::Error>,
}
impl BrowserUi {
    fn emit(&mut self, event: Value) {
        if let Err(error) = self
            .render
            .call1(&JsValue::NULL, &JsValue::from_str(&event.to_string()))
        {
            self.render_error = Some(callback_error(error));
        }
    }
    fn check(&mut self) -> Result<()> {
        if let Some(error) = self.render_error.take() {
            Err(error)
        } else {
            Ok(())
        }
    }
    async fn choice(&mut self, kind: &str, mut fields: Value) -> Result<Value> {
        self.next_id += 1;
        fields["type"] = json!("choice");
        fields["id"] = json!(self.next_id);
        fields["kind"] = json!(kind);
        self.emit(fields);
        self.check()?;
        let action = self
            .runtime
            .chosen
            .lock()
            .await
            .next()
            .await
            .ok_or_else(|| anyhow!("Input closed"))?;
        ensure!(
            action["id"].as_u64() == Some(self.next_id),
            "Stale or invalid action"
        );
        Ok(action)
    }
}

impl Frontend for BrowserUi {
    fn set_current_player(&mut self, seat: usize) {
        self.emit(json!({"type":"turn","seat":seat}));
    }
    fn set_hand(&mut self, hand: &[Card]) {
        self.emit(json!({"type":"hand","cards":cards(hand)}));
    }
    fn set_trick(&mut self, trick: &Trick) {
        self.emit(json!({"type":"trick","cards":trick_json(trick)}));
    }
    fn set_contract(&mut self, contract: &Contract) {
        self.emit(json!({"type":"contract","contract":contract_json(contract)}));
    }
    fn set_scores(&mut self, scores: &[i32; 3]) {
        self.emit(json!({"type":"scores","scores":scores}));
    }
    fn set_trick_num(&mut self, n: u32) {
        self.emit(json!({"type":"trick_num","number":n}));
    }
    fn set_round(&mut self, n: u32) {
        self.emit(json!({"type":"round","number":n}));
    }
    fn show_status(&mut self, message: &str) -> Result<()> {
        self.emit(json!({"type":"status","message":message}));
        self.check()
    }
    fn display_tricks(&mut self) -> Result<()> {
        self.check()
    }
    fn show_bid_action(&mut self, name: &str, action: &BidAction) -> Result<()> {
        let (action, value) = match action {
            BidAction::Bid(value) => ("bid", Some(value)),
            BidAction::Hold => ("hold", None),
            BidAction::Pass => ("pass", None),
        };
        self.emit(json!({"type":"bid","name":name,"action":action,"value":value}));
        self.check()
    }
    fn show_trick_result(&mut self, winner: &str, points: u32) -> Result<()> {
        self.emit(json!({"type":"trick_result","winner":winner,"points":points}));
        self.check()
    }
    fn show_round_result(
        &mut self,
        declarer: &str,
        value: i32,
        points: u32,
        scores: &[i32; 3],
        names: &[String; 3],
    ) -> Result<()> {
        self.emit(json!({"type":"round_result","declarer":declarer,"value":value,"points":points,"scores":scores,"names":names}));
        self.check()
    }
    async fn get_bid_action(
        &mut self,
        hand: &[Card],
        next_bid: Option<u32>,
        is_responder: bool,
    ) -> Result<BidAction> {
        let action = self
            .choice(
                "bid",
                json!({"hand":cards(hand),"next_bid":next_bid,"is_responder":is_responder}),
            )
            .await?;
        match action["action"].as_str() {
            Some("pass") => Ok(BidAction::Pass),
            Some("hold") if is_responder => Ok(BidAction::Hold),
            Some("bid") if !is_responder => Ok(BidAction::Bid(
                next_bid.ok_or_else(|| anyhow!("No higher bid"))?,
            )),
            _ => Err(anyhow!("Invalid bid action")),
        }
    }
    async fn ask_skat_pickup(&mut self, hand: &[Card]) -> Result<bool> {
        self.choice("pickup", json!({"hand":cards(hand)})).await?["pickup"]
            .as_bool()
            .ok_or_else(|| anyhow!("Invalid skat choice"))
    }
    async fn choose_skat_discard(&mut self, hand: &[Card]) -> Result<Vec<Card>> {
        let action = self.choice("discard", json!({"hand":cards(hand)})).await?;
        let values = action["cards"]
            .as_array()
            .ok_or_else(|| anyhow!("Choose two cards"))?;
        ensure!(values.len() == 2, "Choose exactly two cards");
        let selected = values.iter().map(action_card).collect::<Result<Vec<_>>>()?;
        ensure!(
            selected[0] != selected[1] && selected.iter().all(|card| hand.contains(card)),
            "Invalid discard"
        );
        Ok(selected)
    }
    async fn declare_contract(
        &mut self,
        hand: &[Card],
        bid: u32,
        is_hand: bool,
    ) -> Result<Contract> {
        let action = self
            .choice(
                "contract",
                json!({
                    "hand": cards(hand),
                    "bid": bid,
                    "is_hand": is_hand,
                    "null_allowed": null_value(Modifiers { hand: is_hand, ..Modifiers::default() }) >= bid,
                }),
            )
            .await?;
        let game_type = match action["game_type"].as_str() {
            Some("grand") => GameType::Grand,
            Some("null") => GameType::Null,
            Some("suit") => GameType::Suit(match action["trump"].as_str() {
                Some("diamonds") => Suit::Diamonds,
                Some("hearts") => Suit::Hearts,
                Some("spades") => Suit::Spades,
                Some("clubs") => Suit::Clubs,
                _ => anyhow::bail!("Invalid trump suit"),
            }),
            _ => anyhow::bail!("Invalid game type"),
        };
        // Match the existing terminal's declaration choices; the core validates the contract.
        Ok(Contract::new(
            game_type,
            Modifiers {
                hand: is_hand,
                ..Modifiers::default()
            },
        ))
    }
    async fn choose_card(
        &mut self,
        hand: &[Card],
        legal: &[Card],
        trick: &Trick,
        contract: &Contract,
        trick_num: u32,
    ) -> Result<Card> {
        let action = self.choice("card", json!({"hand":cards(hand),"legal":cards(legal),"trick":trick_json(trick),"contract":contract_json(contract),"trick_num":trick_num})).await?;
        let card = action_card(&action["card"])?;
        ensure!(
            hand.contains(&card) && legal.contains(&card),
            "Illegal card"
        );
        Ok(card)
    }
    async fn wait_for_continue(&mut self) -> Result<bool> {
        let action = self.choice("continue", json!({})).await?;
        action["continue"]
            .as_bool()
            .ok_or_else(|| anyhow!("Invalid next-round choice"))
    }
}
