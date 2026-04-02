use num_bigint::BigUint;
use rand::seq::SliceRandom;
use rand::Rng;

use crate::crypto::protocol::{card_values, encrypt_and_shuffle, verify_hand, DealPositions};
use crate::crypto::sra::SraKeyPair;
use crate::game::bidding::{BidAction, BiddingState, Seat};
use crate::game::card::{Card, Suit};
use crate::game::contract::{Contract, GameType, Modifiers};
use crate::game::scoring::{game_value, ScoreBoard};
use crate::game::trick::{legal_plays, Trick};

pub struct SimConfig {
    pub rounds: u32,
    pub use_large_prime: bool,
    pub verbose: bool,
}

pub fn run(config: &SimConfig) -> Result<(), String> {
    let prime = if config.use_large_prime {
        crate::crypto::sra::shared_prime()
    } else {
        BigUint::from(7919u64)
    };

    let mut scoreboard = ScoreBoard::new();
    let mut rng = rand::thread_rng();

    println!("=== P2Skat Simulation ===");
    println!(
        "Rounds: {}, Prime: {}",
        config.rounds,
        if config.use_large_prime {
            "2048-bit (RFC 3526)"
        } else {
            "7919 (small)"
        }
    );
    println!();

    for round in 1..=config.rounds {
        println!("--- Round {} ---", round);

        // 1. Shuffle and deal
        let keys: [SraKeyPair; 3] = [
            SraKeyPair::generate(&prime),
            SraKeyPair::generate(&prime),
            SraKeyPair::generate(&prime),
        ];

        let values = card_values();
        let deck = encrypt_and_shuffle(
            &encrypt_and_shuffle(&encrypt_and_shuffle(&values, &keys[0]), &keys[1]),
            &keys[2],
        );

        let mut hands: [Vec<Card>; 3] = [
            decrypt_to_cards(&deck[DealPositions::player_range(0)], &keys),
            decrypt_to_cards(&deck[DealPositions::player_range(1)], &keys),
            decrypt_to_cards(&deck[DealPositions::player_range(2)], &keys),
        ];
        let skat = decrypt_to_cards(&deck[DealPositions::skat_range()], &keys);

        if config.verbose {
            for i in 0..3 {
                println!(
                    "  {}: {}",
                    Seat::from_index(i).name(),
                    format_hand(&hands[i])
                );
            }
            println!("  Skat: {}", format_hand(&skat));
        }

        // 2. Bidding
        let (declarer_seat, bid_value) = simulate_bidding(&mut rng, config.verbose);
        println!("  Declarer: {} (bid {})", declarer_seat.name(), bid_value);

        // 3. Declare game
        let declarer = declarer_seat.index();
        let (contract, skat_cards) =
            declare_game(&mut hands[declarer], &skat, &mut rng, config.verbose);
        println!(
            "  Contract: {:?}{}",
            contract.game_type,
            if contract.modifiers.hand { " Hand" } else { "" }
        );

        // All cards attributable to declarer (hand + skat) for matador counting
        let mut declarer_all_cards = hands[declarer].clone();
        declarer_all_cards.extend_from_slice(&skat_cards);

        // 4. Play tricks
        let (declarer_trick_points, declarer_trick_count) =
            play_tricks(&mut hands, declarer, &contract, &mut rng, config.verbose);

        // Skat card points always count for declarer
        let skat_points: u32 = skat_cards.iter().map(|c| c.points()).sum();
        let total_declarer_points = declarer_trick_points + skat_points;

        // 5. Score
        let value = game_value(
            &contract,
            &declarer_all_cards,
            total_declarer_points,
            declarer_trick_count,
            10,
        );
        scoreboard.record(declarer, value, bid_value);

        println!(
            "  Result: {} ({} pts, value {})",
            if value > 0 { "Won" } else { "Lost" },
            total_declarer_points,
            value
        );

        // 6. Verify crypto
        verify_hand(&deck, [&keys[0], &keys[1], &keys[2]])
            .map_err(|e| format!("Round {}: crypto verification failed: {}", round, e))?;

        if config.verbose {
            println!("  Crypto: OK");
        }
        println!();
    }

    println!("=== Final Scores ===");
    for i in 0..3 {
        println!(
            "  {}: {}",
            Seat::from_index(i).name(),
            scoreboard.scores[i]
        );
    }

    Ok(())
}

/// Decrypt a slice of triply-encrypted BigUint values using all 3 keys.
/// Order of decryption doesn't matter (SRA commutativity).
fn decrypt_to_cards(encrypted: &[BigUint], keys: &[SraKeyPair; 3]) -> Vec<Card> {
    let step1: Vec<BigUint> = encrypted.iter().map(|c| keys[0].decrypt(c)).collect();
    let step2: Vec<BigUint> = step1.iter().map(|c| keys[1].decrypt(c)).collect();
    let plaintext: Vec<BigUint> = step2.iter().map(|c| keys[2].decrypt(c)).collect();

    plaintext
        .iter()
        .map(|v| {
            let idx: u64 = v.try_into().expect("card value fits u64");
            Card::from_index(idx).expect("valid card index after decryption")
        })
        .collect()
}

/// Drive the bidding state machine with random actions.
/// Caller bids 65% / passes 35%. Responder holds 65% / passes 35%.
fn simulate_bidding(rng: &mut impl Rng, verbose: bool) -> (Seat, u32) {
    let mut state = BiddingState::new();

    while !state.is_done() {
        let seat = state.turn;

        // Probe what actions are valid via clone-and-test
        let can_bid = state.next_bid_value().is_some() && {
            let mut test = state.clone();
            test.process(seat, BidAction::Bid(state.next_bid_value().unwrap()))
                .is_ok()
        };
        let can_hold = state.current_bid().is_some() && {
            let mut test = state.clone();
            test.process(seat, BidAction::Hold).is_ok()
        };

        let action = if can_bid {
            if rng.gen_bool(0.35) {
                BidAction::Pass
            } else {
                BidAction::Bid(state.next_bid_value().unwrap())
            }
        } else if can_hold {
            if rng.gen_bool(0.35) {
                BidAction::Pass
            } else {
                BidAction::Hold
            }
        } else {
            BidAction::Pass
        };

        if verbose {
            println!("  Bidding: {} {:?}", seat.name(), action);
        }

        state
            .process(seat, action)
            .expect("simulation produced invalid bid action");
    }

    (state.winner.unwrap(), state.final_bid.unwrap())
}

/// Pick up skat (70%) or play Hand, choose a random game type.
/// Returns (contract, skat_cards) where skat_cards are the final 2 skat cards
/// (original if Hand, discarded cards if picked up).
fn declare_game(
    hand: &mut Vec<Card>,
    skat: &[Card],
    rng: &mut impl Rng,
    verbose: bool,
) -> (Contract, Vec<Card>) {
    let pickup = rng.gen_bool(0.7);

    let skat_cards = if pickup {
        hand.extend_from_slice(skat);
        hand.shuffle(rng);
        let discarded: Vec<Card> = hand.drain(10..).collect();
        if verbose {
            println!(
                "  Skat pickup, discarded: {}",
                format_hand(&discarded)
            );
        }
        discarded
    } else {
        if verbose {
            println!("  Playing Hand (no skat pickup)");
        }
        skat.to_vec()
    };

    let game_type = match rng.gen_range(0u32..6) {
        0 => GameType::Suit(Suit::Diamonds),
        1 => GameType::Suit(Suit::Hearts),
        2 => GameType::Suit(Suit::Spades),
        3 => GameType::Suit(Suit::Clubs),
        4 => GameType::Grand,
        _ => GameType::Null,
    };

    let modifiers = Modifiers {
        hand: !pickup,
        ..Modifiers::default()
    };

    (Contract::new(game_type, modifiers), skat_cards)
}

/// Play 10 tricks with random legal moves.
/// Returns (declarer_trick_points, declarer_trick_count).
fn play_tricks(
    hands: &mut [Vec<Card>; 3],
    declarer: usize,
    contract: &Contract,
    rng: &mut impl Rng,
    verbose: bool,
) -> (u32, u32) {
    let mut declarer_trick_points = 0u32;
    let mut declarer_trick_count = 0u32;
    let mut leader = 0usize; // Forehand leads first trick

    for trick_num in 0..10 {
        let mut trick = Trick::new();
        let mut current = leader;

        for _ in 0..3 {
            let plays = legal_plays(&hands[current], &trick, contract);
            assert!(
                !plays.is_empty(),
                "no legal plays for seat {} in trick {}",
                current,
                trick_num + 1
            );
            let card = *plays.choose(rng).unwrap();
            trick.play(current, card, contract);
            hands[current].retain(|&c| c != card);
            current = (current + 1) % 3;
        }

        let winner = trick.winner(contract);
        let points = trick.points();

        if verbose {
            let plays: String = trick
                .cards
                .iter()
                .map(|(s, c)| format!("{}:{}", Seat::from_index(*s).name(), c))
                .collect::<Vec<_>>()
                .join("  ");
            println!(
                "  Trick {:2}: {}  -> {} ({} pts)",
                trick_num + 1,
                plays,
                Seat::from_index(winner).name(),
                points
            );
        }

        if winner == declarer {
            declarer_trick_points += points;
            declarer_trick_count += 1;
        }

        leader = winner;
    }

    (declarer_trick_points, declarer_trick_count)
}

fn format_hand(cards: &[Card]) -> String {
    let mut sorted = cards.to_vec();
    sorted.sort();
    sorted
        .iter()
        .map(|c| c.to_string())
        .collect::<Vec<_>>()
        .join(" ")
}
