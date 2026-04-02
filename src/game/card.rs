use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Suit {
    Diamonds,
    Hearts,
    Spades,
    Clubs,
}

impl Suit {
    pub const ALL: [Suit; 4] = [Suit::Diamonds, Suit::Hearts, Suit::Spades, Suit::Clubs];

    /// Base value of this suit when used as trump in a Suit game.
    pub fn base_value(self) -> u32 {
        match self {
            Suit::Diamonds => 9,
            Suit::Hearts => 10,
            Suit::Spades => 11,
            Suit::Clubs => 12,
        }
    }

    pub fn symbol(self) -> &'static str {
        match self {
            Suit::Diamonds => "\u{2666}",
            Suit::Hearts => "\u{2665}",
            Suit::Spades => "\u{2660}",
            Suit::Clubs => "\u{2663}",
        }
    }
}

impl fmt::Display for Suit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.symbol())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Rank {
    Seven,
    Eight,
    Nine,
    Queen,
    King,
    Ten,
    Ace,
    Jack,
}

impl Rank {
    pub const ALL: [Rank; 8] = [
        Rank::Seven,
        Rank::Eight,
        Rank::Nine,
        Rank::Ten,
        Rank::Jack,
        Rank::Queen,
        Rank::King,
        Rank::Ace,
    ];

    /// Card point value for scoring.
    pub fn points(self) -> u32 {
        match self {
            Rank::Seven | Rank::Eight | Rank::Nine => 0,
            Rank::Jack => 2,
            Rank::Queen => 3,
            Rank::King => 4,
            Rank::Ten => 10,
            Rank::Ace => 11,
        }
    }

    pub fn short_name(self) -> &'static str {
        match self {
            Rank::Seven => "7",
            Rank::Eight => "8",
            Rank::Nine => "9",
            Rank::Ten => "10",
            Rank::Jack => "J",
            Rank::Queen => "Q",
            Rank::King => "K",
            Rank::Ace => "A",
        }
    }
}

impl fmt::Display for Rank {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.short_name())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Card {
    pub suit: Suit,
    pub rank: Rank,
}

impl Card {
    pub fn new(suit: Suit, rank: Rank) -> Self {
        Self { suit, rank }
    }

    /// Map card to a unique index in 1..=32 for encryption.
    /// Layout: suits in order (Diamonds, Hearts, Spades, Clubs),
    /// ranks in order (Seven..Ace) within each suit.
    pub fn to_index(self) -> u64 {
        let suit_offset = match self.suit {
            Suit::Diamonds => 0,
            Suit::Hearts => 8,
            Suit::Spades => 16,
            Suit::Clubs => 24,
        };
        let rank_offset = match self.rank {
            Rank::Seven => 0,
            Rank::Eight => 1,
            Rank::Nine => 2,
            Rank::Ten => 3,
            Rank::Jack => 4,
            Rank::Queen => 5,
            Rank::King => 6,
            Rank::Ace => 7,
        };
        (suit_offset + rank_offset + 1) as u64
    }

    /// Reconstruct a card from its index (1..=32).
    pub fn from_index(index: u64) -> Option<Self> {
        if index < 1 || index > 32 {
            return None;
        }
        let i = (index - 1) as usize;
        let suit = Suit::ALL[i / 8];
        let rank_idx = i % 8;
        let rank = [
            Rank::Seven,
            Rank::Eight,
            Rank::Nine,
            Rank::Ten,
            Rank::Jack,
            Rank::Queen,
            Rank::King,
            Rank::Ace,
        ][rank_idx];
        Some(Card { suit, rank })
    }

    pub fn points(self) -> u32 {
        self.rank.points()
    }
}

impl fmt::Display for Card {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}{}", self.rank, self.suit)
    }
}

impl PartialOrd for Card {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Card {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.to_index().cmp(&other.to_index())
    }
}

pub struct Deck;

impl Deck {
    /// Create the standard 32-card Skat deck.
    pub fn new() -> Vec<Card> {
        let mut cards = Vec::with_capacity(32);
        for &suit in &Suit::ALL {
            for &rank in &Rank::ALL {
                cards.push(Card::new(suit, rank));
            }
        }
        cards
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deck_has_32_cards() {
        let deck = Deck::new();
        assert_eq!(deck.len(), 32);
    }

    #[test]
    fn index_roundtrip() {
        let deck = Deck::new();
        for card in &deck {
            let idx = card.to_index();
            assert!(idx >= 1 && idx <= 32);
            let back = Card::from_index(idx).unwrap();
            assert_eq!(*card, back);
        }
    }

    #[test]
    fn unique_indices() {
        let deck = Deck::new();
        let indices: Vec<u64> = deck.iter().map(|c| c.to_index()).collect();
        let mut sorted = indices.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), 32);
    }

    #[test]
    fn total_points_120() {
        let deck = Deck::new();
        let total: u32 = deck.iter().map(|c| c.points()).sum();
        assert_eq!(total, 120);
    }
}
