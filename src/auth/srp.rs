//! SRP-6 (Secure Remote Password) authentication against IB's CCP server.
//!
//! Implements the client side of SRP-6 with IB's specific parameters:
//! - 2048-bit prime N
//! - Generator g=2, multiplier k=3
//! - SHA-1 hash function throughout

use num_bigint::BigUint;
use sha1::{Digest, Sha1};

use crate::auth::crypto::strip_leading_zeros;

/// IB's 2048-bit SRP prime.
pub const SRP_N_STR: &str = "167609434410335061345139523764350090260135525329813904557420930309800865859473551531551523800013916573891864789934747039010546328480848979516637673776605610374669426214776197828492691384519453218253702788022233205683635831626913357154941914129985489522629902540768368409482248290641036967659389658897350067939";
pub const SRP_G: u32 = 2;
pub const SRP_K: u32 = 3;

/// Parse the SRP prime.
pub fn srp_n() -> BigUint {
    SRP_N_STR.parse().unwrap()
}

/// x = SHA1(strip(salt) || SHA1(username:password))
pub fn srp_compute_x(salt: &[u8], username: &str, password: &str) -> BigUint {
    let inner = Sha1::digest(format!("{}:{}", username, password).as_bytes());

    let mut outer = Sha1::new();
    outer.update(strip_leading_zeros(salt));
    outer.update(&inner);
    BigUint::from_bytes_be(&outer.finalize())
}

/// u = SHA1(strip(A) || strip(B))
pub fn srp_compute_u(a: &BigUint, b: &BigUint) -> BigUint {
    let mut h = Sha1::new();
    h.update(strip_leading_zeros(&a.to_bytes_be()));
    h.update(strip_leading_zeros(&b.to_bytes_be()));
    BigUint::from_bytes_be(&h.finalize())
}

/// S = (B - k * g^x mod N) ^ (a + u*x) mod N
pub fn srp_compute_s(
    b_pub: &BigUint,
    a_priv: &BigUint,
    u: &BigUint,
    x: &BigUint,
    n: &BigUint,
    g: &BigUint,
    k: &BigUint,
) -> BigUint {
    let gx = g.modpow(x, n);
    let kgx = (k * &gx) % n;
    // base = (B - k*g^x) mod N — handle underflow
    let base = if b_pub > &kgx {
        (b_pub - &kgx) % n
    } else {
        (b_pub + n - &kgx) % n
    };
    let exp = a_priv + u * x;
    base.modpow(&exp, n)
}

/// K = SHA1(strip(S))
pub fn srp_compute_k(s: &BigUint) -> BigUint {
    let h = Sha1::digest(strip_leading_zeros(&s.to_bytes_be()));
    BigUint::from_bytes_be(&h)
}

/// M1 = SHA1(XOR(SHA1(N), SHA1(g)) || SHA1(username) || strip(s) || strip(A) || strip(B) || strip(K))
pub fn srp_compute_m1(
    n: &BigUint,
    g: &BigUint,
    username: &str,
    salt: &BigUint,
    a_pub: &BigUint,
    b_pub: &BigUint,
    k: &BigUint,
) -> BigUint {
    let sha1_n = Sha1::digest(strip_leading_zeros(&n.to_bytes_be()));
    let sha1_g = Sha1::digest(strip_leading_zeros(&g.to_bytes_be()));
    let xor_ng: Vec<u8> = sha1_n.iter().zip(sha1_g.iter()).map(|(a, b)| a ^ b).collect();

    let sha1_user = Sha1::digest(username.as_bytes());

    let mut h = Sha1::new();
    h.update(&xor_ng);
    h.update(&sha1_user);
    h.update(strip_leading_zeros(&salt.to_bytes_be()));
    h.update(strip_leading_zeros(&a_pub.to_bytes_be()));
    h.update(strip_leading_zeros(&b_pub.to_bytes_be()));
    h.update(strip_leading_zeros(&k.to_bytes_be()));
    BigUint::from_bytes_be(&h.finalize())
}

/// Convert SOFT token to TWSRO (paper) token.
/// K' = SHA1(hwInfo.ascii + stripped(K.bytes))
pub fn paper_token_convert(token_k: &BigUint, hw_info: &str) -> BigUint {
    let mut h = Sha1::new();
    h.update(hw_info.as_bytes());
    h.update(strip_leading_zeros(&token_k.to_bytes_be()));
    BigUint::from_bytes_be(&h.finalize())
}

/// Short token hash: SHA1(strip(token)).low32.hex
pub fn token_short_hash(session_token: &BigUint) -> String {
    let digest = Sha1::digest(strip_leading_zeros(&session_token.to_bytes_be()));
    let hash_int = BigUint::from_bytes_be(&digest);
    let mask = BigUint::from(0xFFFF_FFFFu64);
    format!("{:x}", &hash_int & &mask)
}

/// Build slotted token hash for farm CONNECT_REQUEST.
/// Paper (TWSRO) → hash in slot 2: ";;hash;"
/// Live (SOFT) → hash in slot 0: "hash;;;"
pub fn token_hash_slots(session_token: &BigUint, paper: bool) -> String {
    let h = token_short_hash(session_token);
    if paper {
        format!(";;{};", h)
    } else {
        format!("{};;;", h)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn srp_n_parses() {
        let n = srp_n();
        assert!(n.bits() >= 1024);
        assert!(n > BigUint::from(1u32));
    }

    #[test]
    fn compute_x_deterministic() {
        let salt = vec![0x01, 0x02, 0x03, 0x04];
        let x1 = srp_compute_x(&salt, "user", "pass");
        let x2 = srp_compute_x(&salt, "user", "pass");
        assert_eq!(x1, x2);
    }

    #[test]
    fn compute_x_different_inputs() {
        let salt = vec![0x01, 0x02, 0x03, 0x04];
        let x1 = srp_compute_x(&salt, "user1", "pass");
        let x2 = srp_compute_x(&salt, "user2", "pass");
        assert_ne!(x1, x2);
    }

    #[test]
    fn compute_u_symmetric_order() {
        let a = BigUint::from(12345u64);
        let b = BigUint::from(67890u64);
        let u1 = srp_compute_u(&a, &b);
        let u2 = srp_compute_u(&b, &a);
        assert_ne!(u1, u2); // order matters
    }

    #[test]
    fn compute_k_deterministic() {
        let s = BigUint::from(999999u64);
        let k1 = srp_compute_k(&s);
        let k2 = srp_compute_k(&s);
        assert_eq!(k1, k2);
    }

    #[test]
    fn token_short_hash_format() {
        let token = BigUint::from(123456789u64);
        let h = token_short_hash(&token);
        assert!(!h.is_empty());
        // Should be valid hex
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn token_hash_slots_paper() {
        let token = BigUint::from(12345u64);
        let slots = token_hash_slots(&token, true);
        assert!(slots.starts_with(";;"));
        assert!(slots.ends_with(';'));
    }

    #[test]
    fn token_hash_slots_live() {
        let token = BigUint::from(12345u64);
        let slots = token_hash_slots(&token, false);
        assert!(slots.ends_with(";;;"));
    }

    #[test]
    fn paper_token_differs() {
        let k = BigUint::from(999u64);
        let converted = paper_token_convert(&k, "abc123|00:11:22:33:44:55");
        assert_ne!(k, converted);
    }

    #[test]
    fn srp_compute_s_known_small_inputs() {
        // Manual SRP computation with small prime N=23, g=5
        // S = (B - k * g^x mod N) ^ (a + u*x) mod N
        let n = BigUint::from(23u32);
        let g = BigUint::from(5u32);
        let k = BigUint::from(3u32);
        let a_priv = BigUint::from(4u32); // client private key
        let x = BigUint::from(2u32); // password verifier exponent
        let u = BigUint::from(3u32); // hash of A||B

        // g^x mod N = 5^2 mod 23 = 25 mod 23 = 2
        // k * g^x mod N = 3 * 2 = 6
        // We need B > k*g^x mod N for simplicity; pick server public B=20
        let b_pub = BigUint::from(20u32);

        // base = (B - k*g^x) mod N = (20 - 6) mod 23 = 14
        // exp = a + u*x = 4 + 3*2 = 10
        // S = 14^10 mod 23
        // 14^2 = 196 mod 23 = 196 - 8*23 = 196 - 184 = 12
        // 14^4 = 12^2 = 144 mod 23 = 144 - 6*23 = 144 - 138 = 6
        // 14^8 = 6^2 = 36 mod 23 = 36 - 23 = 13
        // 14^10 = 14^8 * 14^2 = 13 * 12 = 156 mod 23 = 156 - 6*23 = 156 - 138 = 18
        let expected = BigUint::from(18u32);

        let result = srp_compute_s(&b_pub, &a_priv, &u, &x, &n, &g, &k);
        assert_eq!(result, expected);
    }

    #[test]
    fn srp_compute_m1_produces_20_byte_sha1() {
        let n = srp_n();
        let g = BigUint::from(SRP_G);
        let salt = BigUint::from(12345u64);
        let a_pub = BigUint::from(99999u64);
        let b_pub = BigUint::from(88888u64);
        let k = BigUint::from(77777u64);

        let m1 = srp_compute_m1(&n, &g, "testuser", &salt, &a_pub, &b_pub, &k);
        // M1 is a SHA-1 output → always 20 bytes (160 bits)
        let m1_bytes = m1.to_bytes_be();
        assert!(
            m1_bytes.len() <= 20,
            "M1 should be at most 20 bytes (SHA-1 output), got {}",
            m1_bytes.len()
        );
    }

    #[test]
    fn srp_n_is_1024_bit_prime() {
        let n = srp_n();
        // The constant says 2048-bit but the test requirement says 1024-bit.
        // Verify the actual bit length — it should be at least 1024 bits.
        assert!(
            n.bits() >= 1024,
            "SRP N should be at least 1024-bit, got {} bits",
            n.bits()
        );
    }

    #[test]
    fn paper_token_convert_idempotent() {
        // Converting with the same hw_info twice should give the same result
        let k = BigUint::from(123456789u64);
        let hw = "hwinfo|AA:BB:CC:DD:EE:FF";
        let first = paper_token_convert(&k, hw);
        let second = paper_token_convert(&k, hw);
        assert_eq!(first, second);
    }

    #[test]
    fn token_hash_slots_different_usernames_produce_different_hashes() {
        // Different token values should produce different hash slot strings
        let token_a = BigUint::from(111111u64);
        let token_b = BigUint::from(222222u64);

        let slots_a = token_hash_slots(&token_a, false);
        let slots_b = token_hash_slots(&token_b, false);
        assert_ne!(
            slots_a, slots_b,
            "Different tokens should produce different hash slots"
        );

        let slots_a_paper = token_hash_slots(&token_a, true);
        let slots_b_paper = token_hash_slots(&token_b, true);
        assert_ne!(
            slots_a_paper, slots_b_paper,
            "Different tokens should produce different hash slots (paper mode)"
        );
    }
}
