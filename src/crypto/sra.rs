use num_bigint::BigUint;
use num_integer::Integer;
use num_traits::{One, Zero};
use rand::Rng;

/// A 2048-bit safe prime p (p = 2q + 1 where q is also prime).
/// Generated offline. All players use the same prime.
pub fn shared_prime() -> BigUint {
    // This is a known 2048-bit safe prime from RFC 3526 (MODP Group 14).
    // p = 2^2048 - 2^1984 - 1 + 2^64 * { [2^1918 pi] + 124476 }
    BigUint::parse_bytes(
        b"FFFFFFFFFFFFFFFFC90FDAA22168C234C4C6628B80DC1CD1\
          29024E088A67CC74020BBEA63B139B22514A08798E3404DD\
          EF9519B3CD3A431B302B0A6DF25F14374FE1356D6D51C245\
          E485B576625E7EC6F44C42E9A637ED6B0BFF5CB6F406B7ED\
          EE386BFB5A899FA5AE9F24117C4B1FE649286651ECE45B3D\
          C2007CB8A163BF0598DA48361C55D39A69163FA8FD24CF5F\
          83655D23DCA3AD961C62F356208552BB9ED529077096966D\
          670C354E4ABC9804F1746C08CA18217C32905E462E36CE3B\
          E39E772C180E86039B2783A2EC07A28FB5C55DF06F4C52C9\
          DE2BCBF6955817183995497CEA956AE515D2261898FA0510\
          15728E5A8AACAA68FFFFFFFFFFFFFFFF",
        16,
    )
    .unwrap()
}

/// p - 1, used as the modulus for key generation.
pub fn phi(p: &BigUint) -> BigUint {
    p - BigUint::one()
}

/// Extended GCD: returns (gcd, x, y) such that a*x + b*y = gcd.
/// x and y are signed (represented as BigInt internally).
fn extended_gcd(a: &BigUint, b: &BigUint) -> (BigUint, BigUint, bool, BigUint, bool) {
    if b.is_zero() {
        return (a.clone(), BigUint::one(), false, BigUint::zero(), false);
    }

    let (g, x1, x1_neg, y1, y1_neg) = extended_gcd(b, &(a % b));
    let q = a / b;
    // x = y1
    // y = x1 - (a/b)*y1
    let qy1 = &q * &y1;

    // y = x1 - q*y1 (with signs)
    let (y_val, y_neg) = if x1_neg == y1_neg {
        // same sign: x1 (sign) - q*y1 (sign) depends on magnitude
        if x1_neg {
            // both negative: -x1 - (-q*y1) = -(x1 - q*y1)
            if x1 >= qy1 {
                ((&x1 - &qy1), true)
            } else {
                ((&qy1 - &x1), false)
            }
        } else {
            // both positive: x1 - q*y1
            if x1 >= qy1 {
                ((&x1 - &qy1), false)
            } else {
                ((&qy1 - &x1), true)
            }
        }
    } else {
        // different signs
        if x1_neg {
            // -x1 - q*y1
            ((&x1 + &qy1), true)
        } else {
            // x1 + q*y1
            ((&x1 + &qy1), false)
        }
    };

    (g, y1, y1_neg, y_val, y_neg)
}

/// Compute modular inverse: a^(-1) mod m.
/// Returns None if gcd(a, m) != 1.
pub fn mod_inverse(a: &BigUint, m: &BigUint) -> Option<BigUint> {
    let (g, x, x_neg, _, _) = extended_gcd(a, m);
    if g != BigUint::one() {
        return None;
    }
    if x_neg {
        // x is negative, so result = m - |x| % m
        Some(m - (&x % m))
    } else {
        Some(&x % m)
    }
}

/// An SRA key pair for mental poker.
#[derive(Debug, Clone)]
pub struct SraKeyPair {
    pub e: BigUint,
    pub d: BigUint,
    pub p: BigUint,
}

impl SraKeyPair {
    /// Generate a random SRA key pair for the given prime p.
    pub fn generate(p: &BigUint) -> Self {
        let phi_p = phi(p);
        let mut rng = rand::thread_rng();

        // Pick random e coprime to phi(p).
        // e must be in [2, phi_p - 1] and gcd(e, phi_p) == 1.
        let e = loop {
            // Generate a random BigUint with same byte length as phi_p.
            let byte_len = phi_p.to_bytes_be().len();
            let mut bytes = vec![0u8; byte_len];
            rng.fill(&mut bytes[..]);
            let candidate = BigUint::from_bytes_be(&bytes) % &phi_p;
            if candidate > BigUint::one() && candidate.gcd(&phi_p) == BigUint::one() {
                break candidate;
            }
        };

        let d = mod_inverse(&e, &phi_p).expect("e is coprime to phi(p)");

        SraKeyPair {
            e,
            d,
            p: p.clone(),
        }
    }

    /// Encrypt a plaintext value: m^e mod p.
    pub fn encrypt(&self, m: &BigUint) -> BigUint {
        m.modpow(&self.e, &self.p)
    }

    /// Decrypt a ciphertext: c^d mod p.
    pub fn decrypt(&self, c: &BigUint) -> BigUint {
        c.modpow(&self.d, &self.p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn small_prime() -> BigUint {
        // A small safe prime for fast tests: 7879 (7879 = 2*3939 + 1... let's use a known one)
        // 23 is a safe prime (23 = 2*11 + 1), but too small.
        // Use 1223 (not safe but works for basic testing) — actually let's use the real prime
        // but with a smaller one: 7919 is prime, (7919-1)/2 = 3959 is prime. Safe prime!
        BigUint::from(7919u64)
    }

    #[test]
    fn encrypt_decrypt_roundtrip() {
        let p = small_prime();
        let kp = SraKeyPair::generate(&p);
        let m = BigUint::from(42u64);
        let c = kp.encrypt(&m);
        let recovered = kp.decrypt(&c);
        assert_eq!(m, recovered);
    }

    #[test]
    fn commutativity() {
        let p = small_prime();
        let kp_a = SraKeyPair::generate(&p);
        let kp_b = SraKeyPair::generate(&p);

        let m = BigUint::from(17u64);

        // Encrypt with A then B
        let c_ab = kp_b.encrypt(&kp_a.encrypt(&m));
        // Decrypt with A then B (opposite order of encrypt)
        let recovered = kp_b.decrypt(&kp_a.decrypt(&c_ab));
        assert_eq!(m, recovered);

        // Decrypt in same order as encrypt (should also work due to commutativity)
        let recovered2 = kp_a.decrypt(&kp_b.decrypt(&c_ab));
        assert_eq!(m, recovered2);
    }

    #[test]
    fn all_card_indices_roundtrip() {
        let p = small_prime();
        let kp = SraKeyPair::generate(&p);
        for i in 1u64..=32 {
            let m = BigUint::from(i);
            let c = kp.encrypt(&m);
            let recovered = kp.decrypt(&c);
            assert_eq!(m, recovered, "Failed roundtrip for card index {}", i);
        }
    }

    #[test]
    fn mod_inverse_basic() {
        let a = BigUint::from(3u64);
        let m = BigUint::from(26u64);
        let inv = mod_inverse(&a, &m).unwrap();
        assert_eq!((&a * &inv) % &m, BigUint::one());
    }

    #[test]
    fn three_player_commutativity() {
        let p = small_prime();
        let kp_a = SraKeyPair::generate(&p);
        let kp_b = SraKeyPair::generate(&p);
        let kp_c = SraKeyPair::generate(&p);

        let m = BigUint::from(7u64);

        // Encrypt: A -> B -> C
        let encrypted = kp_c.encrypt(&kp_b.encrypt(&kp_a.encrypt(&m)));

        // Decrypt: B removes layer, then A, then C (arbitrary order)
        let step1 = kp_b.decrypt(&encrypted);
        let step2 = kp_a.decrypt(&step1);
        let recovered = kp_c.decrypt(&step2);
        assert_eq!(m, recovered);
    }
}
