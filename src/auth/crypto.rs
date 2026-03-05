//! Cryptographic primitives for IB protocol.
//!
//! - TLS 1.0 PRF (RFC 2246): key derivation for DH secure channel
//! - AES-128-CBC + HMAC-SHA1: encrypt-then-MAC for NS secure messages
//! - Helpers: PKCS7 pad/unpad, strip leading zeros

use aes::Aes128;
use cbc::cipher::{block_padding::Pkcs7, BlockDecryptMut, BlockEncryptMut, KeyIvInit};
use hmac::{Hmac, Mac};
use md5::Md5;
use sha1::Sha1;

type Aes128CbcEnc = cbc::Encryptor<Aes128>;
type Aes128CbcDec = cbc::Decryptor<Aes128>;
type HmacMd5 = Hmac<Md5>;
type HmacSha1 = Hmac<Sha1>;

/// Strip leading zero bytes (Java BigInteger.toByteArray() compat).
pub fn strip_leading_zeros(b: &[u8]) -> &[u8] {
    let mut i = 0;
    while i < b.len().saturating_sub(1) && b[i] == 0 {
        i += 1;
    }
    &b[i..]
}

/// HMAC-SHA1(key, data) → 20-byte digest.
pub fn hmac_sha1(key: &[u8], data: &[u8]) -> [u8; 20] {
    let mut mac = <HmacSha1 as Mac>::new_from_slice(key).unwrap();
    mac.update(data);
    let result = mac.finalize().into_bytes();
    let mut out = [0u8; 20];
    out.copy_from_slice(&result);
    out
}

/// AES-128-CBC encrypt with PKCS7 padding.
pub fn aes_cbc_encrypt(key: &[u8], iv: &[u8], plaintext: &[u8]) -> Vec<u8> {
    let block_size = 16;
    let padded_len = plaintext.len() + (block_size - plaintext.len() % block_size);
    let mut buf = vec![0u8; padded_len];
    buf[..plaintext.len()].copy_from_slice(plaintext);
    let enc = Aes128CbcEnc::new_from_slices(key, iv).unwrap();
    let ct = enc.encrypt_padded_mut::<Pkcs7>(&mut buf, plaintext.len()).unwrap();
    ct.to_vec()
}

/// AES-128-CBC decrypt with PKCS7 unpadding.
pub fn aes_cbc_decrypt(key: &[u8], iv: &[u8], ciphertext: &[u8]) -> Result<Vec<u8>, &'static str> {
    let mut buf = ciphertext.to_vec();
    let dec = Aes128CbcDec::new_from_slices(key, iv).unwrap();
    let pt = dec
        .decrypt_padded_mut::<Pkcs7>(&mut buf)
        .map_err(|_| "AES-CBC decrypt/unpad failed")?;
    Ok(pt.to_vec())
}

/// P_MD5 expansion for TLS 1.0 PRF.
fn p_md5(secret: &[u8], seed: &[u8], length: usize) -> Vec<u8> {
    let mut result = Vec::with_capacity(length);
    let mut a = {
        let mut mac = <HmacMd5 as Mac>::new_from_slice(secret).unwrap();
        mac.update(seed);
        mac.finalize().into_bytes().to_vec()
    };
    while result.len() < length {
        let mut mac = <HmacMd5 as Mac>::new_from_slice(secret).unwrap();
        mac.update(&a);
        mac.update(seed);
        result.extend_from_slice(&mac.finalize().into_bytes());
        let mut mac_a = <HmacMd5 as Mac>::new_from_slice(secret).unwrap();
        mac_a.update(&a);
        a = mac_a.finalize().into_bytes().to_vec();
    }
    result.truncate(length);
    result
}

/// P_SHA-1 expansion for TLS 1.0 PRF.
fn p_sha1(secret: &[u8], seed: &[u8], length: usize) -> Vec<u8> {
    let mut result = Vec::with_capacity(length);
    let mut a = {
        let mut mac = <HmacSha1 as Mac>::new_from_slice(secret).unwrap();
        mac.update(seed);
        mac.finalize().into_bytes().to_vec()
    };
    while result.len() < length {
        let mut mac = <HmacSha1 as Mac>::new_from_slice(secret).unwrap();
        mac.update(&a);
        mac.update(seed);
        result.extend_from_slice(&mac.finalize().into_bytes());
        let mut mac_a = <HmacSha1 as Mac>::new_from_slice(secret).unwrap();
        mac_a.update(&a);
        a = mac_a.finalize().into_bytes().to_vec();
    }
    result.truncate(length);
    result
}

/// TLS 1.0 PRF: P_MD5(S1, label+seed) XOR P_SHA-1(S2, label+seed).
pub fn tls10_prf(secret: &[u8], label: &str, seed: &[u8], length: usize) -> Vec<u8> {
    let mut label_seed = label.as_bytes().to_vec();
    label_seed.extend_from_slice(seed);

    let n = secret.len();
    let half = n / 2;
    let s1 = &secret[..half + (n & 1)];
    let s2 = &secret[half..];

    let md5_out = p_md5(s1, &label_seed, length);
    let sha1_out = p_sha1(s2, &label_seed, length);

    md5_out
        .iter()
        .zip(sha1_out.iter())
        .map(|(a, b)| a ^ b)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_zeros_basic() {
        assert_eq!(strip_leading_zeros(&[0, 0, 1, 2]), &[1, 2]);
        assert_eq!(strip_leading_zeros(&[1, 2, 3]), &[1, 2, 3]);
        assert_eq!(strip_leading_zeros(&[0]), &[0]);
    }

    #[test]
    fn hmac_sha1_known() {
        // RFC 2202 test case 1
        let key = [0x0b; 20];
        let data = b"Hi There";
        let expected = [
            0xb6, 0x17, 0x31, 0x86, 0x55, 0x05, 0x72, 0x64, 0xe2, 0x8b, 0xc0, 0xb6, 0xfb, 0x37,
            0x8c, 0x8e, 0xf1, 0x46, 0xbe, 0x00,
        ];
        assert_eq!(hmac_sha1(&key, data), expected);
    }

    #[test]
    fn aes_cbc_roundtrip() {
        let key = [0u8; 16];
        let iv = [0u8; 16];
        let plaintext = b"hello world 1234"; // exactly 16 bytes
        let ct = aes_cbc_encrypt(&key, &iv, plaintext);
        let pt = aes_cbc_decrypt(&key, &iv, &ct).unwrap();
        assert_eq!(pt, plaintext);
    }

    #[test]
    fn aes_cbc_padding() {
        let key = [1u8; 16];
        let iv = [2u8; 16];
        let plaintext = b"short";
        let ct = aes_cbc_encrypt(&key, &iv, plaintext);
        assert!(ct.len() == 16); // padded to one block
        let pt = aes_cbc_decrypt(&key, &iv, &ct).unwrap();
        assert_eq!(pt, plaintext);
    }

    #[test]
    fn tls10_prf_deterministic() {
        let secret = b"test secret";
        let seed = b"test seed";
        let out1 = tls10_prf(secret, "test label", seed, 32);
        let out2 = tls10_prf(secret, "test label", seed, 32);
        assert_eq!(out1.len(), 32);
        assert_eq!(out1, out2);
    }

    #[test]
    fn tls10_prf_different_labels() {
        let secret = b"secret";
        let seed = b"seed";
        let a = tls10_prf(secret, "label A", seed, 48);
        let b = tls10_prf(secret, "label B", seed, 48);
        assert_ne!(a, b);
    }

    #[test]
    fn tls10_prf_key_expansion() {
        // Verify we can produce 104-byte key block (as used in SecureChannel)
        let secret = vec![0xABu8; 48];
        let seed = vec![0xCDu8; 64];
        let key_block = tls10_prf(&secret, "key expansion", &seed, 104);
        assert_eq!(key_block.len(), 104);
    }
}
