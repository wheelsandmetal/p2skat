use super::card::Card;
use super::contract::{Contract, EffectiveSuit};

/// A single trick (3 cards played, one per player).
#[derive(Debug, Clone)]
pub struct Trick {
    /// (player_seat, card) in the order they were played.
    pub cards: Vec<(usize, Card)>,
    pub lead_suit: Option<EffectiveSuit>,
}

impl Trick {
    pub fn new() -> Self {
        Self {
            cards: Vec::with_capacity(3),
            lead_suit: None,
        }
    }

    /// Play a card into this trick.
    pub fn play(&mut self, player: usize, card: Card, contract: &Contract) {
        if self.cards.is_empty() {
            self.lead_suit = Some(contract.effective_suit(card));
        }
        self.cards.push((player, card));
    }

    pub fn is_complete(&self) -> bool {
        self.cards.len() == 3
    }

    /// Determine the winner of a completed trick. Returns the seat index.
    pub fn winner(&self, contract: &Contract) -> usize {
        assert!(self.is_complete(), "Trick not complete");
        let lead = self.lead_suit.unwrap();

        let mut best_player = self.cards[0].0;
        let mut best_card = self.cards[0].1;

        for &(player, card) in &self.cards[1..] {
            if contract.trick_strength(card, best_card, lead) == std::cmp::Ordering::Greater {
                best_player = player;
                best_card = card;
            }
        }

        best_player
    }

    /// Total card points in this trick.
    pub fn points(&self) -> u32 {
        self.cards.iter().map(|(_, c)| c.points()).sum()
    }
}

/// Check if a player can legally play the given card.
/// `hand` is the player's current hand, `card` is the proposed play.
pub fn is_legal_play(
    hand: &[Card],
    card: Card,
    trick: &Trick,
    contract: &Contract,
) -> bool {
    if !hand.contains(&card) {
        return false;
    }

    // If leading, anything is legal.
    if trick.cards.is_empty() {
        return true;
    }

    let lead_suit = trick.lead_suit.unwrap();
    let card_suit = contract.effective_suit(card);

    // If player can follow suit, they must.
    if card_suit == lead_suit {
        return true;
    }

    // Card doesn't match lead — only legal if player has no cards of lead suit.
    let has_lead_suit = hand
        .iter()
        .any(|c| contract.effective_suit(*c) == lead_suit);
    !has_lead_suit
}

/// Get all legal plays from a hand for the current trick state.
pub fn legal_plays(hand: &[Card], trick: &Trick, contract: &Contract) -> Vec<Card> {
    hand.iter()
        .copied()
        .filter(|&c| is_legal_play(hand, c, trick, contract))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::game::card::{Card, Rank, Suit};
    use crate::game::contract::{GameType, Modifiers};

    fn hearts_contract() -> Contract {
        Contract::new(GameType::Suit(Suit::Hearts), Modifiers::default())
    }

    #[test]
    fn trick_winner_basic() {
        let contract = hearts_contract();
        let mut trick = Trick::new();
        trick.play(0, Card::new(Suit::Spades, Rank::Ten), &contract);
        trick.play(1, Card::new(Suit::Spades, Rank::Ace), &contract);
        trick.play(2, Card::new(Suit::Spades, Rank::King), &contract);

        assert_eq!(trick.winner(&contract), 1); // Ace wins
    }

    #[test]
    fn trump_wins_trick() {
        let contract = hearts_contract();
        let mut trick = Trick::new();
        trick.play(0, Card::new(Suit::Spades, Rank::Ace), &contract);
        trick.play(1, Card::new(Suit::Hearts, Rank::Seven), &contract); // trump
        trick.play(2, Card::new(Suit::Spades, Rank::Ten), &contract);

        assert_eq!(trick.winner(&contract), 1);
    }

    #[test]
    fn must_follow_suit() {
        let contract = hearts_contract();
        let hand = vec![
            Card::new(Suit::Spades, Rank::Ace),
            Card::new(Suit::Spades, Rank::Ten),
            Card::new(Suit::Diamonds, Rank::Seven),
        ];

        let mut trick = Trick::new();
        trick.play(0, Card::new(Suit::Spades, Rank::King), &contract);

        // Must play spades
        assert!(is_legal_play(&hand, Card::new(Suit::Spades, Rank::Ace), &trick, &contract));
        assert!(!is_legal_play(&hand, Card::new(Suit::Diamonds, Rank::Seven), &trick, &contract));
    }

    #[test]
    fn can_play_anything_if_void() {
        let contract = hearts_contract();
        let hand = vec![
            Card::new(Suit::Diamonds, Rank::Seven),
            Card::new(Suit::Diamonds, Rank::Eight),
        ];

        let mut trick = Trick::new();
        trick.play(0, Card::new(Suit::Spades, Rank::King), &contract);

        // No spades in hand, anything goes
        assert!(is_legal_play(&hand, Card::new(Suit::Diamonds, Rank::Seven), &trick, &contract));
    }

    #[test]
    fn jack_follows_trump_not_suit() {
        let contract = hearts_contract();
        // Hand has Jack of Spades (which is trump) and no plain hearts
        let hand = vec![
            Card::new(Suit::Spades, Rank::Jack), // trump!
            Card::new(Suit::Diamonds, Rank::Seven),
        ];

        // Someone leads plain spades
        let mut trick = Trick::new();
        trick.play(0, Card::new(Suit::Spades, Rank::Ace), &contract);

        // Jack of Spades is trump, not plain spades — so we can't "follow" with it.
        // We have no plain spades, so we can play anything.
        let plays = legal_plays(&hand, &trick, &contract);
        assert_eq!(plays.len(), 2);
    }
}
