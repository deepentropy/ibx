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
}
