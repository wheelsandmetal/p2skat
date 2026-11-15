use super::card::{Card, Rank, Suit};
use super::contract::{Contract, GameType, Modifiers};

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

/// Calculate the signed score for a completed hand, as recorded on the score sheet.
///
/// A game is lost if the declarer misses 61 card points, misses an announced
/// Schneider or Schwarz, or overbid (game value below `bid`). An overbid game
/// counts as the lowest multiple of the base value that reaches the bid. Lost
/// games count double.
pub fn game_value(
    contract: &Contract,
    declarer_cards: &[Card], // original declarer hand plus skat, excluding won tricks
    declarer_trick_points: u32,
    declarer_trick_count: u32,
    total_tricks: u32,
    bid: u32,
) -> i32 {
    match contract.game_type {
        GameType::Null => null_game_value(contract, declarer_trick_count, bid),
        _ => normal_game_value(
            contract,
            declarer_cards,
            declarer_trick_points,
            declarer_trick_count,
            total_tricks,
            bid,
        ),
    }
}

/// Fixed value of a Null game: 23, Hand 35, Ouvert 46, Ouvert Hand 59.
pub fn null_value(modifiers: Modifiers) -> u32 {
    match (modifiers.hand, modifiers.ouvert) {
        (false, false) => 23,
        (true, false) => 35,
        (false, true) => 46,
        (true, true) => 59,
    }
}

fn null_game_value(contract: &Contract, declarer_trick_count: u32, bid: u32) -> i32 {
    let base = null_value(contract.modifiers);
    // Declaration rejects Null below the bid; treat it as a plain loss if it slips through.
    if declarer_trick_count == 0 && base >= bid {
        base as i32
    } else {
        -2 * base as i32
    }
}

fn normal_game_value(
    contract: &Contract,
    declarer_cards: &[Card],
    declarer_trick_points: u32,
    declarer_trick_count: u32,
    total_tricks: u32,
    bid: u32,
) -> i32 {
    let modifiers = contract.modifiers;
    let (mat_count, _with) = matadors(declarer_cards, contract.game_type);

    let schneider_achieved = declarer_trick_points >= 90;
    let schwarz_achieved = declarer_trick_count == total_tricks;
    // Announcements include every lower level, whether or not it was reached.
    let schwarz = schwarz_achieved || declarer_trick_count == 0 || modifiers.schwarz_announced;
    let schneider = schwarz
        || schneider_achieved
        || declarer_trick_points < 31
        || modifiers.schneider_announced;

    // Matadors + 1 ("game"), then one level per applicable modifier.
    let multiplier = mat_count
        + 1
        + [
            modifiers.hand,
            schneider,
            modifiers.schneider_announced,
            schwarz,
            modifiers.schwarz_announced,
            modifiers.ouvert,
        ]
        .iter()
        .filter(|&&level| level)
        .count() as u32;

    let base = contract.game_type.base_value();
    let value = base * multiplier;

    let won = declarer_trick_points >= 61
        && (!modifiers.schneider_announced || schneider_achieved)
        && (!modifiers.schwarz_announced || schwarz_achieved)
        && value >= bid;
    if won {
        value as i32
    } else {
        let overbid_value = bid.div_ceil(base) * base;
        -2 * value.max(overbid_value) as i32
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

    /// Record a signed score from [`game_value`], which already applies the bid.
    pub fn record(&mut self, declarer: usize, value: i32, bid: u32) {
        self.scores[declarer] += value;
        self.history.push(HandScore {
            declarer,
            game_value: value,
            bid,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let value = game_value(&contract, &declarer_cards, 70, 5, 10, 18);
        assert_eq!(value, 72);
    }

    #[test]
    fn null_win() {
        let contract = Contract::new(GameType::Null, Modifiers::default());
        let value = game_value(&contract, &[], 0, 0, 10, 18);
        assert_eq!(value, 23);
    }

    #[test]
    fn null_loss() {
        let contract = Contract::new(GameType::Null, Modifiers::default());
        let value = game_value(&contract, &[], 0, 1, 10, 18);
        assert_eq!(value, -46);
    }

    fn jacks(suits: &[Suit]) -> Vec<Card> {
        suits.iter().map(|&suit| Card::new(suit, Rank::Jack)).collect()
    }

    #[test]
    fn lost_game_counts_double() {
        let contract = Contract::new(GameType::Grand, Modifiers::default());
        let cards = jacks(&[Suit::Clubs, Suit::Spades]);
        // With 2, game 3: 72, lost with 50 points.
        assert_eq!(game_value(&contract, &cards, 50, 4, 10, 18), -144);
        // Under 31 points adds Schneider: 4 * 24.
        assert_eq!(game_value(&contract, &cards, 30, 2, 10, 18), -192);
    }

    #[test]
    fn game_value_reaching_the_bid_wins() {
        let contract = Contract::new(GameType::Suit(Suit::Diamonds), Modifiers::default());
        // With 2, game 3: 27.
        let cards = jacks(&[Suit::Clubs, Suit::Spades]);
        assert_eq!(game_value(&contract, &cards, 70, 6, 10, 27), 27);
    }

    #[test]
    fn overbid_game_loses_lowest_multiple_reaching_the_bid() {
        let contract = Contract::new(GameType::Suit(Suit::Diamonds), Modifiers::default());
        // With 1, game 2: 18, but the bid was 30. 4 * 9 = 36 is the first multiple >= 30.
        let cards = jacks(&[Suit::Clubs]);
        assert_eq!(game_value(&contract, &cards, 70, 6, 10, 30), -72);
        // Losing on points too still counts at least the overbid value.
        assert_eq!(game_value(&contract, &cards, 40, 3, 10, 30), -72);
        // Without an overbid, a loss counts the computed value: Schneider lifts it to 27.
        assert_eq!(game_value(&contract, &cards, 20, 1, 10, 18), -54);
    }

    #[test]
    fn announced_schneider_counts_even_when_missed() {
        let contract = Contract::new(
            GameType::Grand,
            Modifiers {
                hand: true,
                schneider_announced: true,
                ..Modifiers::default()
            },
        );
        let cards = jacks(&[Suit::Clubs]);
        // With 1, game, hand, Schneider, announced: 5 * 24 = 120; missed, so lost.
        assert_eq!(game_value(&contract, &cards, 80, 7, 10, 18), -240);
        assert_eq!(game_value(&contract, &cards, 95, 8, 10, 18), 120);
    }

    #[test]
    fn announced_schwarz_includes_schneider_levels() {
        let contract = Contract::new(
            GameType::Grand,
            Modifiers {
                hand: true,
                schneider_announced: true,
                schwarz_announced: true,
                ..Modifiers::default()
            },
        );
        let cards = jacks(&[Suit::Clubs]);
        // With 1, game, hand, Schneider, announced, Schwarz, announced: 7 * 24.
        assert_eq!(game_value(&contract, &cards, 120, 10, 10, 18), 168);
        assert_eq!(game_value(&contract, &cards, 110, 9, 10, 18), -336);
    }

    #[test]
    fn null_values_and_overbid() {
        let hand = Modifiers {
            hand: true,
            ..Modifiers::default()
        };
        assert_eq!(null_value(Modifiers::default()), 23);
        assert_eq!(null_value(hand), 35);
        let null_hand = Contract::new(GameType::Null, hand);
        assert_eq!(game_value(&null_hand, &[], 0, 0, 10, 35), 35);
        let null = Contract::new(GameType::Null, Modifiers::default());
        assert_eq!(game_value(&null, &[], 0, 0, 10, 24), -46);
    }

    #[test]
    fn score_board_records_the_signed_value() {
        let mut board = ScoreBoard::new();
        board.record(1, -144, 18);
        board.record(1, 27, 27);
        assert_eq!(board.scores, [0, -117, 0]);
        assert_eq!(board.history[0].game_value, -144);
    }

    #[test]
    fn total_deck_points() {
        let deck = crate::game::card::Deck::new();
        assert_eq!(count_points(&deck), 120);
    }
}
