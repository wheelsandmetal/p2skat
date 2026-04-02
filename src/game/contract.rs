use super::card::{Card, Rank, Suit};

/// The type of Skat game being played.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GameType {
    /// A suit game — the named suit (plus Jacks) are trump.
    Suit(Suit),
    /// Grand — only Jacks are trump.
    Grand,
    /// Null — no trumps, declarer must take zero tricks.
    Null,
}

impl GameType {
    /// Base value for this game type (used in score calculation).
    pub fn base_value(self) -> u32 {
        match self {
            GameType::Suit(suit) => suit.base_value(),
            GameType::Grand => 24,
            GameType::Null => 23,
        }
    }
}

/// Modifiers that affect scoring.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Modifiers {
    /// Declarer plays without picking up the Skat.
    pub hand: bool,
    /// Schneider announced by declarer (requires hand).
    pub schneider_announced: bool,
    /// Schwarz announced by declarer (requires hand).
    pub schwarz_announced: bool,
    /// Ouvert — declarer reveals hand (requires hand).
    pub ouvert: bool,
}

/// A fully declared contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Contract {
    pub game_type: GameType,
    pub modifiers: Modifiers,
}

impl Contract {
    pub fn new(game_type: GameType, modifiers: Modifiers) -> Self {
        Self {
            game_type,
            modifiers,
        }
    }

    /// Is this card trump in the current contract?
    pub fn is_trump(&self, card: Card) -> bool {
        match self.game_type {
            GameType::Null => false,
            GameType::Grand => card.rank == Rank::Jack,
            GameType::Suit(trump_suit) => {
                card.rank == Rank::Jack || card.suit == trump_suit
            }
        }
    }

    /// Get the "effective suit" of a card under this contract.
    /// Jacks belong to the trump suit in Suit/Grand games.
    pub fn effective_suit(&self, card: Card) -> EffectiveSuit {
        if self.is_trump(card) {
            EffectiveSuit::Trump
        } else {
            EffectiveSuit::Plain(card.suit)
        }
    }

    /// Compare two cards for trick-taking strength.
    /// Returns Ordering from the perspective of `a` vs `b`.
    /// `lead_suit` is the effective suit of the card that was led.
    pub fn trick_strength(&self, a: Card, b: Card, lead_suit: EffectiveSuit) -> std::cmp::Ordering {
        let a_eff = self.effective_suit(a);
        let b_eff = self.effective_suit(b);

        // Determine if each card "counts" (is trump or follows lead)
        let a_relevant = a_eff == EffectiveSuit::Trump || a_eff == lead_suit;
        let b_relevant = b_eff == EffectiveSuit::Trump || b_eff == lead_suit;

        match (a_relevant, b_relevant) {
            (false, false) => std::cmp::Ordering::Equal, // neither follows — first played wins
            (true, false) => std::cmp::Ordering::Greater,
            (false, true) => std::cmp::Ordering::Less,
            (true, true) => {
                // Both are relevant. Trump beats non-trump.
                let a_trump = a_eff == EffectiveSuit::Trump;
                let b_trump = b_eff == EffectiveSuit::Trump;
                match (a_trump, b_trump) {
                    (true, false) => std::cmp::Ordering::Greater,
                    (false, true) => std::cmp::Ordering::Less,
                    _ => {
                        // Same effective suit. Compare by rank.
                        self.rank_order(a).cmp(&self.rank_order(b))
                    }
                }
            }
        }
    }

    /// Numeric rank ordering for comparison (higher = stronger).
    fn rank_order(&self, card: Card) -> u32 {
        match self.game_type {
            GameType::Null => {
                // Null: normal ranking 7 < 8 < 9 < 10 < J < Q < K < A
                match card.rank {
                    Rank::Seven => 0,
                    Rank::Eight => 1,
                    Rank::Nine => 2,
                    Rank::Ten => 3,
                    Rank::Jack => 4,
                    Rank::Queen => 5,
                    Rank::King => 6,
                    Rank::Ace => 7,
                }
            }
            GameType::Grand | GameType::Suit(_) => {
                if card.rank == Rank::Jack {
                    // Jacks ranked by suit: Clubs > Spades > Hearts > Diamonds
                    match card.suit {
                        Suit::Diamonds => 100,
                        Suit::Hearts => 101,
                        Suit::Spades => 102,
                        Suit::Clubs => 103,
                    }
                } else {
                    // Normal cards: 7 < 8 < 9 < Q < K < 10 < A
                    match card.rank {
                        Rank::Seven => 0,
                        Rank::Eight => 1,
                        Rank::Nine => 2,
                        Rank::Queen => 3,
                        Rank::King => 4,
                        Rank::Ten => 5,
                        Rank::Ace => 6,
                        Rank::Jack => unreachable!(),
                    }
                }
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffectiveSuit {
    Trump,
    Plain(Suit),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jack_is_always_trump_in_suit_game() {
        let contract = Contract::new(GameType::Suit(Suit::Hearts), Modifiers::default());
        assert!(contract.is_trump(Card::new(Suit::Clubs, Rank::Jack)));
        assert!(contract.is_trump(Card::new(Suit::Hearts, Rank::Jack)));
        assert!(contract.is_trump(Card::new(Suit::Hearts, Rank::Ace)));
        assert!(!contract.is_trump(Card::new(Suit::Clubs, Rank::Ace)));
    }

    #[test]
    fn grand_only_jacks_trump() {
        let contract = Contract::new(GameType::Grand, Modifiers::default());
        assert!(contract.is_trump(Card::new(Suit::Clubs, Rank::Jack)));
        assert!(!contract.is_trump(Card::new(Suit::Clubs, Rank::Ace)));
    }

    #[test]
    fn null_no_trump() {
        let contract = Contract::new(GameType::Null, Modifiers::default());
        assert!(!contract.is_trump(Card::new(Suit::Clubs, Rank::Jack)));
        assert!(!contract.is_trump(Card::new(Suit::Clubs, Rank::Ace)));
    }

    #[test]
    fn trump_beats_plain() {
        let contract = Contract::new(GameType::Suit(Suit::Hearts), Modifiers::default());
        let trump = Card::new(Suit::Hearts, Rank::Seven);
        let plain = Card::new(Suit::Clubs, Rank::Ace);
        let lead = contract.effective_suit(plain);

        let ord = contract.trick_strength(trump, plain, lead);
        assert_eq!(ord, std::cmp::Ordering::Greater);
    }

    #[test]
    fn jack_of_clubs_highest_trump() {
        let contract = Contract::new(GameType::Suit(Suit::Hearts), Modifiers::default());
        let cj = Card::new(Suit::Clubs, Rank::Jack);
        let ha = Card::new(Suit::Hearts, Rank::Ace);
        let lead = EffectiveSuit::Trump;

        let ord = contract.trick_strength(cj, ha, lead);
        assert_eq!(ord, std::cmp::Ordering::Greater);
    }
}
