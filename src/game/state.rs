use num_bigint::BigUint;

use super::bidding::{BiddingState, Seat};
use super::card::Card;
use super::contract::Contract;
use super::trick::Trick;

/// Top-level game phase.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Phase {
    WaitingForPlayers,
    Shuffling,
    Dealing,
    Bidding,
    PickingUpSkat,
    Playing,
    Scoring,
}

/// Player-local game state (each player maintains their own view).
#[derive(Debug)]
pub struct GameState {
    pub phase: Phase,
    pub my_seat: Seat,

    /// My hand (decrypted cards I can see).
    pub hand: Vec<Card>,

    /// The encrypted deck after all shuffle phases.
    pub encrypted_deck: Vec<BigUint>,

    /// Bidding state.
    pub bidding: BiddingState,

    /// Who won the bidding (declarer).
    pub declarer: Option<Seat>,

    /// The declared contract.
    pub contract: Option<Contract>,

    /// The Skat cards (only visible to declarer after pickup).
    pub skat: Vec<Card>,

    /// Cards put back into the skat by declarer.
    pub skat_put_away: Vec<Card>,

    /// Current trick being played.
    pub current_trick: Trick,

    /// Whose turn it is to play a card.
    pub trick_leader: Seat,
    pub current_player: Seat,

    /// Tricks won by each player (stored as card collections for scoring).
    pub tricks_won: [Vec<Card>; 3],

    /// Number of tricks played so far.
    pub tricks_played: u32,

    /// Round number (for seat rotation).
    pub round: u32,
}

impl GameState {
    pub fn new(my_seat: Seat) -> Self {
        Self {
            phase: Phase::WaitingForPlayers,
            my_seat,
            hand: Vec::new(),
            encrypted_deck: Vec::new(),
            bidding: BiddingState::new(),
            declarer: None,
            contract: None,
            skat: Vec::new(),
            skat_put_away: Vec::new(),
            current_trick: Trick::new(),
            trick_leader: Seat::Forehand,
            current_player: Seat::Forehand,
            tricks_won: [Vec::new(), Vec::new(), Vec::new()],
            tricks_played: 0,
            round: 0,
        }
    }

    /// Start a new round (reset hand-specific state, rotate seats).
    pub fn new_round(&mut self) {
        self.phase = Phase::Shuffling;
        self.hand.clear();
        self.encrypted_deck.clear();
        self.bidding = BiddingState::new();
        self.declarer = None;
        self.contract = None;
        self.skat.clear();
        self.skat_put_away.clear();
        self.current_trick = Trick::new();
        self.trick_leader = Seat::Forehand;
        self.current_player = Seat::Forehand;
        self.tricks_won = [Vec::new(), Vec::new(), Vec::new()];
        self.tricks_played = 0;
        self.round += 1;
    }

    /// Sort hand for display. In suit/grand games, group by effective suit.
    pub fn sort_hand(&mut self) {
        if let Some(contract) = &self.contract {
            self.hand.sort_by(|a, b| {
                let a_trump = contract.is_trump(*a);
                let b_trump = contract.is_trump(*b);
                match (a_trump, b_trump) {
                    (true, false) => std::cmp::Ordering::Less,
                    (false, true) => std::cmp::Ordering::Greater,
                    _ => a.cmp(b),
                }
            });
        } else {
            self.hand.sort();
        }
    }
}
