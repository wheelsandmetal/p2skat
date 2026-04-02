use super::card::{Card, Rank, Suit};
use super::contract::{Contract, GameType};

/// Count card points in a collection of cards.
pub fn count_points(cards: &[Card]) -> u32 {
    cards.iter().map(|c| c.points()).sum()
}

/// Determine the matador count (consecutive Jacks from the top, "with" or "without").
/// Returns (count, with) where `with` means declarer has the Jack of Clubs.
pub fn matadors(declarer_cards: &[Card], game_type: GameType) -> (u32, bool) {
    let trump_sequence = match game_type {
        GameType::Null => return (0, false), // No matadors in Null
        GameType::Grand => {
            // Only Jacks: CJ, SJ, HJ, DJ
            vec![
                Card::new(Suit::Clubs, Rank::Jack),
                Card::new(Suit::Spades, Rank::Jack),
                Card::new(Suit::Hearts, Rank::Jack),
                Card::new(Suit::Diamonds, Rank::Jack),
            ]
        }
        GameType::Suit(trump_suit) => {
            // Jacks (C, S, H, D) then trump suit A, 10, K, Q, 9, 8, 7
            let mut seq = vec![
                Card::new(Suit::Clubs, Rank::Jack),
                Card::new(Suit::Spades, Rank::Jack),
                Card::new(Suit::Hearts, Rank::Jack),
                Card::new(Suit::Diamonds, Rank::Jack),
            ];
            for &rank in &[
                Rank::Ace,
                Rank::Ten,
                Rank::King,
                Rank::Queen,
                Rank::Nine,
                Rank::Eight,
                Rank::Seven,
            ] {
                seq.push(Card::new(trump_suit, rank));
            }
            seq
        }
    };

    let has_top = declarer_cards.contains(&trump_sequence[0]);
    let mut count = 0u32;

    for card in &trump_sequence {
        let has = declarer_cards.contains(card);
        if has == has_top {
            count += 1;
        } else {
            break;
        }
    }

    (count, has_top)
}

/// Calculate the game value for a completed hand.
pub fn game_value(
    contract: &Contract,
    declarer_cards: &[Card], // all cards held by declarer (hand + skat + tricks)
    declarer_trick_points: u32,
    declarer_trick_count: u32,
    total_tricks: u32,
) -> i32 {
    match contract.game_type {
        GameType::Null => null_game_value(contract, declarer_trick_count),
        _ => normal_game_value(
            contract,
            declarer_cards,
            declarer_trick_points,
            declarer_trick_count,
            total_tricks,
        ),
    }
}

fn null_game_value(contract: &Contract, declarer_trick_count: u32) -> i32 {
    let base = match (contract.modifiers.hand, contract.modifiers.ouvert) {
        (false, false) => 23,
        (true, false) => 35,
        (false, true) => 46,
        (true, true) => 59,
    };

    if declarer_trick_count == 0 {
        base
    } else {
        -2 * base // lost
    }
}

fn normal_game_value(
    contract: &Contract,
    declarer_cards: &[Card],
    declarer_trick_points: u32,
    declarer_trick_count: u32,
    total_tricks: u32,
) -> i32 {
    let (mat_count, _with) = matadors(declarer_cards, contract.game_type);

    // Multiplier starts at matadors + 1 ("game")
    let mut multiplier = mat_count + 1;

    if contract.modifiers.hand {
        multiplier += 1;
    }

    // Schneider (opponent has < 31 points) / declarer loses with < 31
    let schneider_achieved = declarer_trick_points >= 90;
    let schwarz_achieved = declarer_trick_count == total_tricks;
    let declarer_won_basic = declarer_trick_points >= 61;

    if schneider_achieved || declarer_trick_points < 31 {
        multiplier += 1; // schneider
    }
    if contract.modifiers.schneider_announced {
        multiplier += 1;
    }
    if schwarz_achieved || declarer_trick_count == 0 {
        multiplier += 1; // schwarz
    }
    if contract.modifiers.schwarz_announced {
        multiplier += 1;
    }
    if contract.modifiers.ouvert {
        multiplier += 1;
    }

    let base = contract.game_type.base_value();
    let value = (base * multiplier) as i32;

    if declarer_won_basic
        && (!contract.modifiers.schneider_announced || schneider_achieved)
        && (!contract.modifiers.schwarz_announced || schwarz_achieved)
    {
        value
    } else {
        // Declarer lost. Value is negative, and at least the bid value.
        -value
    }
}

/// Score entry for one hand.
#[derive(Debug, Clone)]
pub struct HandScore {
    pub declarer: usize, // seat index
    pub game_value: i32,
    pub bid: u32,
}

/// Running score tracker across multiple rounds.
#[derive(Debug, Clone, Default)]
pub struct ScoreBoard {
    pub scores: [i32; 3],
    pub history: Vec<HandScore>,
}

impl ScoreBoard {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record(&mut self, declarer: usize, value: i32, bid: u32) {
        // If declarer lost and the computed |value| < bid, use -2*bid instead.
        let final_value = if value < 0 {
            let min_loss = -(2 * bid as i32);
            value.min(min_loss)
        } else {
            value
        };

        self.scores[declarer] += final_value;
        self.history.push(HandScore {
            declarer,
            game_value: final_value,
            bid,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::game::contract::Modifiers;

    #[test]
    fn matadors_with_two() {
        // Declarer has CJ, SJ but not HJ
        let cards = vec![
            Card::new(Suit::Clubs, Rank::Jack),
            Card::new(Suit::Spades, Rank::Jack),
            Card::new(Suit::Hearts, Rank::Ace),
        ];
        let (count, with) = matadors(&cards, GameType::Grand);
        assert_eq!(count, 2);
        assert!(with);
    }

    #[test]
    fn matadors_without_one() {
        // Declarer doesn't have CJ but has SJ
        let cards = vec![
            Card::new(Suit::Spades, Rank::Jack),
            Card::new(Suit::Hearts, Rank::Ace),
        ];
        let (count, with) = matadors(&cards, GameType::Grand);
        assert_eq!(count, 1);
        assert!(!with);
    }

    #[test]
    fn matadors_suit_game() {
        // Hearts game. Declarer has CJ, SJ, HJ, DJ, HA, H10
        let cards = vec![
            Card::new(Suit::Clubs, Rank::Jack),
            Card::new(Suit::Spades, Rank::Jack),
            Card::new(Suit::Hearts, Rank::Jack),
            Card::new(Suit::Diamonds, Rank::Jack),
            Card::new(Suit::Hearts, Rank::Ace),
            Card::new(Suit::Hearts, Rank::Ten),
        ];
        let (count, with) = matadors(&cards, GameType::Suit(Suit::Hearts));
        assert_eq!(count, 6);
        assert!(with);
    }

    #[test]
    fn grand_win_with_2() {
        let contract = Contract::new(GameType::Grand, Modifiers::default());
        let declarer_cards = vec![
            Card::new(Suit::Clubs, Rank::Jack),
            Card::new(Suit::Spades, Rank::Jack),
        ];
        // with 2, game = multiplier 3 * base 24 = 72
        let value = game_value(&contract, &declarer_cards, 70, 5, 10);
        assert_eq!(value, 72);
    }

    #[test]
    fn null_win() {
        let contract = Contract::new(GameType::Null, Modifiers::default());
        let value = game_value(&contract, &[], 0, 0, 10);
        assert_eq!(value, 23);
    }

    #[test]
    fn null_loss() {
        let contract = Contract::new(GameType::Null, Modifiers::default());
        let value = game_value(&contract, &[], 0, 1, 10);
        assert_eq!(value, -46);
    }

    #[test]
    fn total_deck_points() {
        let deck = crate::game::card::Deck::new();
        assert_eq!(count_points(&deck), 120);
    }
}
