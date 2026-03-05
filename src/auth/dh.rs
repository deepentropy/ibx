//! Diffie-Hellman key exchange for establishing shared secrets with IB servers.
//!
//! Implements the IB custom TLS-like protocol:
//! - DH key exchange for pre-master secret
//! - TLS 1.0 PRF for key derivation
//! - AES-128-CBC + HMAC-SHA1 (Encrypt-then-MAC)

use base64::{Engine as _, engine::general_purpose::STANDARD as B64};
use num_bigint::BigUint;
use rand::RngCore;

use crate::auth::crypto::{aes_cbc_decrypt, aes_cbc_encrypt, hmac_sha1, strip_leading_zeros, tls10_prf};
use crate::auth::srp::SRP_N_STR;
use crate::protocol::ns::NS_MAGIC;

/// DH uses the same prime as SRP.
fn dh_n() -> BigUint {
    SRP_N_STR.parse().unwrap()
}

/// DH-based encrypted channel (NS_SECURE_CONNECT / NS_SECURE_MESSAGE).
pub struct SecureChannel {
    client_random: [u8; 32],
    private_key: BigUint,
    public_key: BigUint,
    // Cipher state (set after key derivation)
    key_block: Option<Vec<u8>>,
    write_aes_key: Option<Vec<u8>>,
    read_aes_key: Option<Vec<u8>>,
    write_iv: Option<Vec<u8>>,
    read_iv: Option<Vec<u8>>,
    write_mac_key: Option<Vec<u8>>,
    read_mac_key: Option<Vec<u8>>,
}

impl SecureChannel {
    pub fn new() -> Self {
        let mut client_random = [0u8; 32];
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as u32;
        client_random[0..4].copy_from_slice(&timestamp.to_be_bytes());
        rand::rng().fill_bytes(&mut client_random[4..]);

        let mut priv_bytes = [0u8; 32];
        rand::rng().fill_bytes(&mut priv_bytes);
        let private_key = BigUint::from_bytes_be(&priv_bytes);
        let n = dh_n();
        let g = BigUint::from(2u32);
        let public_key = g.modpow(&private_key, &n);

        Self {
            client_random,
            private_key,
            public_key,
            key_block: None,
            write_aes_key: None,
            read_aes_key: None,
            write_iv: None,
            read_iv: None,
            write_mac_key: None,
            read_mac_key: None,
        }
    }

    /// Build NS_SECURE_CONNECT (532) message.
    pub fn build_secure_connect(&self, version: u32, negotiated_version: u32) -> Vec<u8> {
        let cr_b64 = B64.encode(&self.client_random);

        // Encode public key as 128-byte big-endian, zero-padded
        let pub_bytes = self.public_key.to_bytes_be();
        let mut pub_padded = vec![0u8; 128];
        if pub_bytes.len() <= 128 {
            pub_padded[128 - pub_bytes.len()..].copy_from_slice(&pub_bytes);
        } else {
            // Shouldn't happen with 2048-bit prime, but strip leading zeros
            let stripped = strip_leading_zeros(&pub_bytes);
            let start = 128usize.saturating_sub(stripped.len());
            pub_padded[start..].copy_from_slice(&stripped[..128.min(stripped.len())]);
        }
        let pub_b64 = B64.encode(&pub_padded);

        let payload = format!(
            "{};532;0;{};{};{};",
            version, negotiated_version, cr_b64, pub_b64
        );
        let payload_bytes = payload.as_bytes();
        let mut msg = Vec::with_capacity(8 + payload_bytes.len());
        msg.extend_from_slice(NS_MAGIC);
        msg.extend_from_slice(&(payload_bytes.len() as u32).to_be_bytes());
        msg.extend_from_slice(payload_bytes);
        msg
    }

    /// Parse NS_SECURE_CONNECTION_START (533) fields and derive keys.
    ///
    /// `fields` are the semicolon-split parts after version and msg_type:
    /// `[server_random_b64, server_pub_b64, ...]`
    pub fn process_server_hello(&mut self, fields: &[&str]) {
        let server_random = B64.decode(fields[0]).unwrap();
        let server_pub_bytes = B64.decode(fields[1]).unwrap();
        let server_pub = BigUint::from_bytes_be(&server_pub_bytes);

        let n = dh_n();

        // Pre-master secret = server_pub ^ client_private mod N
        let shared = server_pub.modpow(&self.private_key, &n);
        let shared_bytes = shared.to_bytes_be();
        let pre_master = strip_leading_zeros(&shared_bytes);

        // Master secret = PRF(pre_master, "master secret", client_random || server_random)
        let mut seed = Vec::with_capacity(64);
        seed.extend_from_slice(&self.client_random);
        seed.extend_from_slice(&server_random);
        let master_secret = tls10_prf(pre_master, "master secret", &seed, 48);

        // Key block = PRF(master_secret, "key expansion", client_random || server_random)
        let key_block = tls10_prf(&master_secret, "key expansion", &seed, 104);

        // Parse key block (104 bytes):
        // [0:16]   = client→server AES key
        // [16:32]  = server→client AES key
        // [32:48]  = client→server IV
        // [48:64]  = server→client IV
        // [64:84]  = client→server HMAC key
        // [84:104] = server→client HMAC key
        self.write_aes_key = Some(key_block[0..16].to_vec());
        self.read_aes_key = Some(key_block[16..32].to_vec());
        self.write_iv = Some(key_block[32..48].to_vec());
        self.read_iv = Some(key_block[48..64].to_vec());
        self.write_mac_key = Some(key_block[64..84].to_vec());
        self.read_mac_key = Some(key_block[84..104].to_vec());
        self.key_block = Some(key_block);
    }

    /// Encrypt plaintext using Encrypt-then-MAC (AES-CBC + HMAC-SHA1).
    ///
    /// Wire format: ciphertext || 20-byte HMAC (MAC is NOT encrypted).
    pub fn encrypt(&mut self, plaintext: &[u8]) -> Vec<u8> {
        let aes_key = self.write_aes_key.as_ref().unwrap();
        let iv = self.write_iv.as_ref().unwrap();
        let mac_key = self.write_mac_key.as_ref().unwrap();

        // AES-CBC encrypt with PKCS7
        let ciphertext = aes_cbc_encrypt(aes_key, iv, plaintext);

        // MAC over IV + ciphertext
        let mut mac_input = Vec::with_capacity(iv.len() + ciphertext.len());
        mac_input.extend_from_slice(iv);
        mac_input.extend_from_slice(&ciphertext);
        let mac = hmac_sha1(mac_key, &mac_input);

        // Update IV to last ciphertext block
        self.write_iv = Some(ciphertext[ciphertext.len() - 16..].to_vec());

        let mut result = ciphertext;
        result.extend_from_slice(&mac);
        result
    }

    /// Verify MAC then decrypt (Encrypt-then-MAC).
    ///
    /// Wire format: ciphertext || 20-byte HMAC.
    pub fn decrypt(&mut self, data: &[u8]) -> Result<Vec<u8>, &'static str> {
        if data.len() < 20 {
            return Err("data too short for MAC");
        }
        let ciphertext = &data[..data.len() - 20];
        let received_mac = &data[data.len() - 20..];

        let iv = self.read_iv.as_ref().unwrap();
        let mac_key = self.read_mac_key.as_ref().unwrap();

        // Verify MAC over IV + ciphertext
        let mut mac_input = Vec::with_capacity(iv.len() + ciphertext.len());
        mac_input.extend_from_slice(iv);
        mac_input.extend_from_slice(ciphertext);
        let expected_mac = hmac_sha1(mac_key, &mac_input);

        if received_mac != expected_mac {
            return Err("HMAC verification failed");
        }

        let aes_key = self.read_aes_key.as_ref().unwrap();
        let plaintext = aes_cbc_decrypt(aes_key, iv, ciphertext)?;

        // Update IV to last ciphertext block
        self.read_iv = Some(ciphertext[ciphertext.len() - 16..].to_vec());

        Ok(plaintext)
    }

    /// Encrypt with INITIAL IVs from key derivation (for FIX-level logon).
    ///
    /// Java creates a fresh cipher with initial key_block IVs for the FIX
    /// logon, separate from the NS channel's chained IVs.
    pub fn encrypt_fresh(&self, plaintext: &[u8]) -> Vec<u8> {
        let kb = self.key_block.as_ref().unwrap();
        let iv = &kb[32..48];
        let aes_key = &kb[0..16];
        let mac_key = &kb[64..84];

        let ciphertext = aes_cbc_encrypt(aes_key, iv, plaintext);

        let mut mac_input = Vec::with_capacity(16 + ciphertext.len());
        mac_input.extend_from_slice(iv);
        mac_input.extend_from_slice(&ciphertext);
        let mac = hmac_sha1(mac_key, &mac_input);

        let mut result = ciphertext;
        result.extend_from_slice(&mac);
        result
    }

    /// Access the raw key block (for FIX-level encryption setup).
    pub fn key_block(&self) -> Option<&[u8]> {
        self.key_block.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_test_channel() -> SecureChannel {
        // Create a channel with deterministic keys for testing
        let mut ch = SecureChannel {
            client_random: [0u8; 32],
            private_key: BigUint::from(0u32),
            public_key: BigUint::from(0u32),
            key_block: None,
            write_aes_key: Some(vec![0u8; 16]),
            read_aes_key: Some(vec![0u8; 16]),
            write_iv: Some(vec![0u8; 16]),
            read_iv: Some(vec![0u8; 16]),
            write_mac_key: Some(vec![0u8; 20]),
            read_mac_key: Some(vec![0u8; 20]),
        };
        // Set key_block for encrypt_fresh
        let mut kb = vec![0u8; 104];
        kb[0..16].copy_from_slice(&[0u8; 16]); // write AES
        kb[16..32].copy_from_slice(&[0u8; 16]); // read AES
        kb[32..48].copy_from_slice(&[0u8; 16]); // write IV
        kb[48..64].copy_from_slice(&[0u8; 16]); // read IV
        kb[64..84].copy_from_slice(&[0u8; 20]); // write MAC
        kb[84..104].copy_from_slice(&[0u8; 20]); // read MAC
        ch.key_block = Some(kb);
        ch
    }

    #[test]
    fn encrypt_decrypt_roundtrip() {
        let mut enc_ch = make_test_channel();
        let mut dec_ch = make_test_channel();

        let plaintext = b"hello secure channel";
        let encrypted = enc_ch.encrypt(plaintext);
        let decrypted = dec_ch.decrypt(&encrypted).unwrap();
        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn encrypt_decrypt_multiple() {
        let mut enc_ch = make_test_channel();
        let mut dec_ch = make_test_channel();

        for i in 0..5 {
            let msg = format!("message {}", i);
            let encrypted = enc_ch.encrypt(msg.as_bytes());
            let decrypted = dec_ch.decrypt(&encrypted).unwrap();
            assert_eq!(decrypted, msg.as_bytes());
        }
    }

    #[test]
    fn decrypt_bad_mac() {
        let mut enc_ch = make_test_channel();
        let mut dec_ch = make_test_channel();

        let encrypted = enc_ch.encrypt(b"test");
        let mut corrupted = encrypted.clone();
        let last = corrupted.len() - 1;
        corrupted[last] ^= 0xFF;
        assert!(dec_ch.decrypt(&corrupted).is_err());
    }

    #[test]
    fn encrypt_fresh_deterministic() {
        let ch = make_test_channel();
        let ct1 = ch.encrypt_fresh(b"logon message");
        let ct2 = ch.encrypt_fresh(b"logon message");
        // Same initial IVs → same ciphertext
        assert_eq!(ct1, ct2);
    }

    #[test]
    fn iv_chains_across_messages() {
        let mut ch = make_test_channel();
        let ct1 = ch.encrypt(b"first");
        let ct2 = ch.encrypt(b"second");
        // Different ciphertexts due to IV chaining
        assert_ne!(ct1, ct2);
    }

    #[test]
    fn build_secure_connect_format() {
        let ch = SecureChannel::new();
        let msg = ch.build_secure_connect(50, 50);
        assert_eq!(&msg[..4], NS_MAGIC);
        let payload = &msg[8..];
        let text = std::str::from_utf8(payload).unwrap();
        assert!(text.starts_with("50;532;0;50;"));
        assert!(text.ends_with(';'));
    }
}
