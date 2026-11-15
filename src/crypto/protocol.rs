use num_bigint::BigUint;
use rand::seq::SliceRandom;
use sha2::{Digest, Sha256};

use super::sra::SraKeyPair;
use crate::game::card::{Card, Deck};

/// Encrypt every card value and shuffle the result (Fisher-Yates).
pub fn encrypt_and_shuffle(cards: &[BigUint], key: &SraKeyPair) -> Vec<BigUint> {
    let mut encrypted: Vec<BigUint> = cards.iter().map(|c| key.encrypt(c)).collect();
    let mut rng = rand::thread_rng();
    encrypted.shuffle(&mut rng);
    encrypted
}

/// Fixed public plaintexts for the 32 cards.
///
/// SRA (`m^e mod p`) preserves the Legendre symbol and multiplicative relations
/// (`E(a)·E(b) = E(a·b)`, `E(1) = 1`). Raw indices leaked through all of them:
/// `1` stayed `1`, each card's quadratic-residue bit was visible, and relations
/// such as `E(2)² = E(4)` linked ciphertexts. Squares of small numbers would
/// fix the first two but keep the third (`E(2²)² = E(4²)`). So each card is the
/// square of a hash-derived value mod p: every encoding is a quadratic residue
/// (constant Legendre symbol), none is 0 or 1, all are distinct, and there are
/// no known multiplicative relations between them. The derivation is
/// deterministic, so all peers build the same table from the shared prime.
pub struct CardCodec {
    /// `values[i]` encodes the card with index `i + 1`.
    values: Vec<BigUint>,
}

const CODEC_DOMAIN: &[u8] = b"p2skat card encoding v1";

impl CardCodec {
    /// Build the table for prime `p`.
    pub fn new(p: &BigUint) -> Result<Self, String> {
        if p.bits() < 12 {
            return Err(format!("Prime {} is too small for card encoding", p));
        }
        // Hash 64 bits more than p so the reduction is close to uniform.
        let blocks = (p.bits() as usize + 64).div_ceil(256);
        let one = BigUint::from(1u8);
        let mut values: Vec<BigUint> = Vec::with_capacity(32);
        for index in 1..=32u8 {
            let value = (0..1000u32)
                .map(|counter| {
                    let mut bytes = Vec::with_capacity(blocks * 32);
                    for block in 0..blocks as u32 {
                        let mut hasher = Sha256::new();
                        hasher.update(CODEC_DOMAIN);
                        hasher.update([index]);
                        hasher.update(counter.to_be_bytes());
                        hasher.update(block.to_be_bytes());
                        bytes.extend_from_slice(&hasher.finalize());
                    }
                    let root = BigUint::from_bytes_be(&bytes) % p;
                    (&root * &root) % p
                })
                .find(|value| *value > one && !values.contains(value))
                .ok_or_else(|| format!("No card encoding found for prime {}", p))?;
            values.push(value);
        }
        Ok(Self { values })
    }

    pub fn encode(&self, card: Card) -> &BigUint {
        &self.values[card.to_index() as usize - 1]
    }

    /// Table lookup: anything outside the table is not a card.
    pub fn decode(&self, value: &BigUint) -> Option<Card> {
        let position = self.values.iter().position(|known| known == value)?;
        Card::from_index(position as u64 + 1)
    }

    /// Plaintext deck in `Deck::new()` order.
    pub fn deck(&self) -> Vec<BigUint> {
        Deck::new()
            .iter()
            .map(|c| self.encode(*c).clone())
            .collect()
    }
}

/// Jacobi symbol `(a/n)` for odd `n > 0`: 1, -1 (as `n - 1` would be awkward), or 0.
/// For a prime `n` this is the Legendre symbol, computed without a modular exponentiation.
pub fn jacobi(a: &BigUint, n: &BigUint) -> i8 {
    debug_assert!(n.bit(0), "the Jacobi symbol needs an odd modulus");
    let low = |value: &BigUint| value.iter_u32_digits().next().unwrap_or(0);
    let mut a = a % n;
    let mut n = n.clone();
    let mut result = 1i8;
    while a != BigUint::default() {
        let zeros = a.trailing_zeros().unwrap_or(0);
        a >>= zeros;
        if zeros % 2 == 1 && matches!(low(&n) & 7, 3 | 5) {
            result = -result;
        }
        std::mem::swap(&mut a, &mut n);
        if low(&a) & 3 == 3 && low(&n) & 3 == 3 {
            result = -result;
        }
        a %= &n;
    }
    if n == BigUint::from(1u8) {
        result
    } else {
        0
    }
}

/// True if `value` can be an honest encrypted or partially decrypted card.
///
/// Every card encoding is a quadratic residue other than 1, and SRA keys are odd
/// exponents, so every layer added or removed keeps a value inside the residues
/// and away from 1. A value outside them (for example `p - c`, a sign flip) would
/// let its sender recognise that card through every later shuffle, so honest
/// peers refuse it at once instead of at the key reveal. For the safe game prime
/// the residues are the prime-order subgroup, which has no smaller subgroups to
/// tag with.
pub fn in_card_group(value: &BigUint, p: &BigUint) -> bool {
    *value > BigUint::from(1u8) && value < p && jacobi(value, p) == 1
}

/// Verify that a final set of decrypted values encodes all 32 cards exactly once.
/// Returns the decoded cards if valid, or an error description.
pub fn verify_deck(decrypted_values: &[BigUint], codec: &CardCodec) -> Result<Vec<Card>, String> {
    if decrypted_values.len() != 32 {
        return Err(format!("Expected 32 cards, got {}", decrypted_values.len()));
    }

    let mut cards = Vec::with_capacity(32);
    for (i, val) in decrypted_values.iter().enumerate() {
        let card = codec
            .decode(val)
            .ok_or_else(|| format!("Invalid card encoding at position {}", i))?;
        if cards.contains(&card) {
            return Err(format!("Duplicate card {} at position {}", card, i));
        }
        cards.push(card);
    }

    Ok(cards)
}

/// End-of-hand verification: given the final encrypted deck and all three players' keys,
/// decrypt everything and verify the result encodes each of the 32 cards once.
pub fn verify_hand(
    final_encrypted_deck: &[BigUint],
    keys: [&SraKeyPair; 3],
) -> Result<Vec<Card>, String> {
    let codec = CardCodec::new(&keys[0].p)?;
    // Decrypt in order: key[0], key[1], key[2]
    let mut values = final_encrypted_deck.to_vec();
    for key in &keys {
        values = values.iter().map(|v| key.decrypt(v)).collect();
    }
    verify_deck(&values, &codec)
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
    use crate::crypto::sra::{shared_prime, SraKeyPair};

    fn small_prime() -> BigUint {
        BigUint::from(7919u64)
    }

    #[test]
    fn card_encodings_are_distinct_nontrivial_quadratic_residues() {
        for p in [
            shared_prime(),
            small_prime(),
            (BigUint::from(1u8) << 127usize) - BigUint::from(1u8),
        ] {
            let codec = CardCodec::new(&p).unwrap();
            let values = codec.deck();
            let one = BigUint::from(1u8);
            let legendre_exponent = (&p - &one) >> 1usize;
            for (i, value) in values.iter().enumerate() {
                assert!(value > &one && value < &p, "card {i} encoding out of range");
                assert_eq!(
                    value.modpow(&legendre_exponent, &p),
                    one,
                    "card {i} encoding is not a quadratic residue"
                );
                assert!(!values[..i].contains(value), "card {i} encoding repeats");
            }
            for card in Deck::new().iter() {
                assert_eq!(codec.decode(codec.encode(*card)), Some(*card));
            }
            assert_eq!(codec.decode(&one), None);
        }
    }

    #[test]
    fn jacobi_matches_euler_and_card_group_survives_every_layer() {
        let one = BigUint::from(1u8);
        let mersenne = (BigUint::from(1u8) << 127usize) - &one;
        let safe = (BigUint::from(1u8) << 128usize) + BigUint::from(0x30a3u32);
        let mut rng = rand::thread_rng();
        // The 2048-bit prime is slow in debug builds: few samples, no keys.
        for (p, count) in [
            (small_prime(), 50),
            (BigUint::from(23u8), 50),
            (mersenne, 50),
            (safe, 50),
            (shared_prime(), 4),
        ] {
            let exponent = (&p - &one) >> 1usize;
            let samples = (1u32..=count * 4).map(BigUint::from).chain(
                (0..count)
                    .map(|_| BigUint::from_bytes_be(&rand::Rng::gen::<[u8; 32]>(&mut rng)) % &p),
            );
            for value in samples {
                let euler = value.modpow(&exponent, &p);
                let expected = if value.clone() % &p == BigUint::default() {
                    0
                } else if euler == one {
                    1
                } else {
                    -1
                };
                assert_eq!(jacobi(&value, &p), expected, "({value}/{p})");
            }
            // Honest values stay in the group through encryption and decryption; a
            // sign flip (p ≡ 3 mod 4 makes -1 a non-residue) and 1 never are.
            let Ok(codec) = CardCodec::new(&p) else {
                continue;
            };
            if count < 50 {
                assert!(codec.deck().iter().all(|value| in_card_group(value, &p)));
                continue;
            }
            let key = SraKeyPair::generate(&p);
            for value in codec.deck().iter().take(8) {
                let encrypted = key.encrypt(value);
                assert!(in_card_group(value, &p) && in_card_group(&encrypted, &p));
                assert!(in_card_group(&key.decrypt(value), &p));
                if p.bit(1) {
                    assert!(!in_card_group(&(&p - &encrypted), &p));
                }
            }
            assert!(!in_card_group(&one, &p));
            assert!(!in_card_group(&BigUint::default(), &p));
            assert!(!in_card_group(&p, &p));
        }
    }

    #[test]
    fn card_encodings_are_deterministic_and_have_no_small_square_relations() {
        let p = shared_prime();
        let first = CardCodec::new(&p).unwrap().deck();
        assert_eq!(first, CardCodec::new(&p).unwrap().deck());
        // E(x)² = E(x²): with structured encodings this links ciphertext pairs.
        for a in &first {
            for b in &first {
                assert_ne!(&((a * a) % &p), b, "square relation between encodings");
                for c in &first {
                    assert_ne!(&((a * b) % &p), c, "product relation between encodings");
                }
            }
        }
    }

    #[test]
    fn full_deal_simulation() {
        // Use a small prime for speed.
        let p = small_prime();

        let key1 = SraKeyPair::generate(&p);
        let key2 = SraKeyPair::generate(&p);
        let key3 = SraKeyPair::generate(&p);

        // Step 1: Start with plaintext card values.
        let values = CardCodec::new(&p).unwrap().deck();
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
        assert_eq!(
            DealPositions::player_indices(0),
            (0..10).collect::<Vec<_>>()
        );
        assert_eq!(
            DealPositions::player_indices(1),
            (10..20).collect::<Vec<_>>()
        );
        assert_eq!(
            DealPositions::player_indices(2),
            (20..30).collect::<Vec<_>>()
        );
        assert_eq!(DealPositions::skat_indices(), vec![30, 31]);
    }

    #[test]
    fn decrypt_hand_for_player() {
        let p = small_prime();
        let codec = CardCodec::new(&p).unwrap();
        let key1 = SraKeyPair::generate(&p);
        let key2 = SraKeyPair::generate(&p);
        let key3 = SraKeyPair::generate(&p);

        let deck = encrypt_and_shuffle(
            &encrypt_and_shuffle(&encrypt_and_shuffle(&codec.deck(), &key1), &key2),
            &key3,
        );

        // Decrypt P1's hand (positions 0..10):
        // Other players remove their layers, then P1 removes theirs.
        let p1_encrypted: Vec<BigUint> = deck[0..10].to_vec();
        let step1: Vec<BigUint> = p1_encrypted.iter().map(|c| key2.decrypt(c)).collect();
        let step2: Vec<BigUint> = step1.iter().map(|c| key3.decrypt(c)).collect();
        let p1_hand: Vec<Card> = step2
            .iter()
            .map(|c| codec.decode(&key1.decrypt(c)).expect("valid card"))
            .collect();

        // All 10 cards should be unique.
        let mut indices: Vec<u64> = p1_hand.iter().map(|c| c.to_index()).collect();
        indices.sort();
        indices.dedup();
        assert_eq!(indices.len(), 10);
    }

    #[test]
    fn verify_deck_rejects_raw_indices_and_duplicates() {
        let codec = CardCodec::new(&small_prime()).unwrap();
        let raw: Vec<BigUint> = (1..=32u32).map(BigUint::from).collect();
        assert!(verify_deck(&raw, &codec).is_err());
        let mut duplicated = codec.deck();
        duplicated[1] = duplicated[0].clone();
        assert!(verify_deck(&duplicated, &codec).is_err());
    }
}
