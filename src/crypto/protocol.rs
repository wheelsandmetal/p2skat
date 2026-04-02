use num_bigint::BigUint;
use rand::seq::SliceRandom;

use super::sra::SraKeyPair;
use crate::game::card::{Card, Deck};

/// Encrypt every card value and shuffle the result (Fisher-Yates).
pub fn encrypt_and_shuffle(cards: &[BigUint], key: &SraKeyPair) -> Vec<BigUint> {
    let mut encrypted: Vec<BigUint> = cards.iter().map(|c| key.encrypt(c)).collect();
    let mut rng = rand::thread_rng();
    encrypted.shuffle(&mut rng);
    encrypted
}

/// Initial card values: each card mapped to its index as a BigUint.
pub fn card_values() -> Vec<BigUint> {
    Deck::new()
        .iter()
        .map(|c| BigUint::from(c.to_index()))
        .collect()
}

/// Verify that a final set of decrypted values matches the expected 32 card indices.
/// Returns the decoded cards if valid, or an error description.
pub fn verify_deck(decrypted_values: &[BigUint]) -> Result<Vec<Card>, String> {
    if decrypted_values.len() != 32 {
        return Err(format!(
            "Expected 32 cards, got {}",
            decrypted_values.len()
        ));
    }

    let mut cards = Vec::with_capacity(32);
    let mut seen = std::collections::HashSet::new();

    for (i, val) in decrypted_values.iter().enumerate() {
        let idx = val
            .try_into()
            .map_err(|_| format!("Card value too large at position {}: {}", i, val))?;
        let card = Card::from_index(idx)
            .ok_or_else(|| format!("Invalid card index {} at position {}", idx, i))?;
        if !seen.insert(idx) {
            return Err(format!("Duplicate card index {} at position {}", idx, i));
        }
        cards.push(card);
    }

    Ok(cards)
}

/// End-of-hand verification: given the final encrypted deck and all three players' keys,
/// decrypt everything and verify the result is a valid permutation of 1..=32.
pub fn verify_hand(
    final_encrypted_deck: &[BigUint],
    keys: [&SraKeyPair; 3],
) -> Result<Vec<Card>, String> {
    // Decrypt in order: key[0], key[1], key[2]
    let mut values = final_encrypted_deck.to_vec();
    for key in &keys {
        values = values.iter().map(|v| key.decrypt(v)).collect();
    }
    verify_deck(&values)
}

/// Deal positions for a 3-player Skat game.
/// cards[0..10]  -> Player 0 (Forehand)
/// cards[10..20] -> Player 1 (Middlehand)
/// cards[20..30] -> Player 2 (Rearhand)
/// cards[30..32] -> Skat
pub struct DealPositions;

impl DealPositions {
    pub const HAND_SIZE: usize = 10;

    pub fn player_range(player: usize) -> std::ops::Range<usize> {
        let start = player * Self::HAND_SIZE;
        start..start + Self::HAND_SIZE
    }

    pub fn skat_range() -> std::ops::Range<usize> {
        30..32
    }

    pub fn player_indices(player: usize) -> Vec<usize> {
        Self::player_range(player).collect()
    }

    pub fn skat_indices() -> Vec<usize> {
        Self::skat_range().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::sra::SraKeyPair;

    #[test]
    fn full_deal_simulation() {
        // Use a small prime for speed.
        let p = BigUint::from(7919u64);

        let key1 = SraKeyPair::generate(&p);
        let key2 = SraKeyPair::generate(&p);
        let key3 = SraKeyPair::generate(&p);

        // Step 1: Start with plaintext card values.
        let values = card_values();
        assert_eq!(values.len(), 32);

        // Step 2: P1 encrypts and shuffles.
        let after_p1 = encrypt_and_shuffle(&values, &key1);

        // Step 3: P2 encrypts and shuffles.
        let after_p2 = encrypt_and_shuffle(&after_p1, &key2);

        // Step 4: P3 encrypts and shuffles.
        let final_deck = encrypt_and_shuffle(&after_p2, &key3);
        assert_eq!(final_deck.len(), 32);

        // Step 5: Verify with all keys.
        let result = verify_hand(&final_deck, [&key1, &key2, &key3]);
        assert!(result.is_ok(), "Verification failed: {:?}", result.err());

        let cards = result.unwrap();
        assert_eq!(cards.len(), 32);

        // Check all 32 unique cards present.
        let mut indices: Vec<u64> = cards.iter().map(|c| c.to_index()).collect();
        indices.sort();
        assert_eq!(indices, (1..=32).collect::<Vec<u64>>());
    }

    #[test]
    fn deal_positions() {
        assert_eq!(DealPositions::player_indices(0), (0..10).collect::<Vec<_>>());
        assert_eq!(DealPositions::player_indices(1), (10..20).collect::<Vec<_>>());
        assert_eq!(DealPositions::player_indices(2), (20..30).collect::<Vec<_>>());
        assert_eq!(DealPositions::skat_indices(), vec![30, 31]);
    }

    #[test]
    fn decrypt_hand_for_player() {
        let p = BigUint::from(7919u64);
        let key1 = SraKeyPair::generate(&p);
        let key2 = SraKeyPair::generate(&p);
        let key3 = SraKeyPair::generate(&p);

        let values = card_values();
        let deck = encrypt_and_shuffle(
            &encrypt_and_shuffle(&encrypt_and_shuffle(&values, &key1), &key2),
            &key3,
        );

        // Decrypt P1's hand (positions 0..10):
        // Other players remove their layers, then P1 removes theirs.
        let p1_encrypted: Vec<BigUint> = deck[0..10].to_vec();
        let step1: Vec<BigUint> = p1_encrypted.iter().map(|c| key2.decrypt(c)).collect();
        let step2: Vec<BigUint> = step1.iter().map(|c| key3.decrypt(c)).collect();
        let p1_hand: Vec<BigUint> = step2.iter().map(|c| key1.decrypt(c)).collect();

        // Each decrypted value should be a valid card index.
        for val in &p1_hand {
            let idx: u64 = val.try_into().unwrap();
            assert!(idx >= 1 && idx <= 32, "Invalid card index: {}", idx);
            assert!(Card::from_index(idx).is_some());
        }

        // All 10 cards should be unique.
        let mut indices: Vec<u64> = p1_hand.iter().map(|v| v.try_into().unwrap()).collect();
        indices.sort();
        indices.dedup();
        assert_eq!(indices.len(), 10);
    }
}
