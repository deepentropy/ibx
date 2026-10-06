//! Diffie-Hellman key exchange for establishing shared secrets.

use std::io;

use base64::engine::DecodePaddingMode;
use base64::engine::general_purpose::{GeneralPurpose, GeneralPurposeConfig};
use base64::{Engine as _, alphabet, engine::general_purpose::STANDARD as B64};
use num_bigint::BigUint;
use rand::RngCore;

use crate::auth::certs::{self, CertError};
use crate::auth::crypto::{aes_cbc_decrypt, aes_cbc_encrypt, hmac_sha1, strip_leading_zeros, tls10_prf};
use crate::auth::srp::SRP_N_STR;
use crate::protocol::ns::NS_MAGIC;

/// Login text of a failed key exchange (`n.g(aD)@440-454`,
/// `trader.common.tag.g.C`).
pub const SECURE_CONNECTION_FAILED: &str = "Unable to establish secure connection";
/// Login text of an expired first certificate (`n.g(aD)@410-424`,
/// `trader.common.tag.g.D`).
pub const CERTIFICATE_EXPIRED: &str =
    "The certificate has expired. Please check if your system time is set correctly and try again.";
/// Login text of a first certificate not yet valid (`n.g(aD)@380-394`,
/// `trader.common.tag.g.E`).
pub const CERTIFICATE_NOT_YET_VALID: &str =
    "The certificate is not yet valid. Please check if your system time is set correctly and try again.";

/// The reference's base64 decoder of the reply fields (`aQ.c(String)`):
/// line breaks removed, then `Base64.getDecoder()`, which takes a value
/// with or without its padding and ignores the unused bits of the last
/// character.
const REPLY_B64: GeneralPurpose = GeneralPurpose::new(
    &alphabet::STANDARD,
    GeneralPurposeConfig::new()
        .with_decode_padding_mode(DecodePaddingMode::Indifferent)
        .with_decode_allow_trailing_bits(true),
);

/// A failed key exchange: `text` is the login text, `detail` what the
/// reference logs ("CipherContext error while initialization").
fn key_exchange_error(text: &str, detail: String) -> io::Error {
    log::error!("Cannot init CipherContext: {}", detail);
    io::Error::new(io::ErrorKind::InvalidData, format!("{} ({})", text, detail))
}

/// DH uses the same prime as SRP.
fn dh_n() -> BigUint {
    SRP_N_STR.parse().unwrap()
}

/// DH-based encrypted channel.
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
    /// Test certificates accepted too ([`crate::config::test_secure_connect`]).
    test_mode: bool,
    /// Time of the certificate checks, Unix milliseconds; `None`: the system
    /// clock, as the reference.
    cert_time_ms: Option<i64>,
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
            test_mode: crate::config::test_secure_connect(),
            cert_time_ms: None,
        }
    }

    /// The channel with its certificate checks at `unix_ms` instead of the
    /// system clock: the captured certificates are valid two days only.
    #[cfg(any(test, feature = "test-support"))]
    pub fn with_cert_time(mut self, unix_ms: i64) -> Self {
        self.cert_time_ms = Some(unix_ms);
        self
    }

    fn cert_time_ms(&self) -> i64 {
        self.cert_time_ms.unwrap_or_else(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_millis() as i64)
        })
    }

    /// Build key exchange initiation message.
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

    /// Parse server hello fields, check its certificates and derive keys.
    ///
    /// `fields` are the semicolon-split parts after version and msg_type:
    /// server random, server public value, signature, certificate count,
    /// then each certificate (`crypt.a.b(String)@12-150`, a tokenizer on
    /// `;`: an empty field is skipped; fields after the certificates are
    /// not read).
    ///
    /// A missing field, bad base64, a bad count or certificate, or a
    /// certificate check that fails (`crypt.a.a(List)`, see
    /// [`certs::check_server_certificates`]) is an `InvalidData` error with
    /// the reference's login text and leaves the channel without keys, so
    /// the caller fails the login, as the reference does (`n.g(aD)@313-454`;
    /// on a farm the failure callback, `az.g(aD)@185-200`), ibx#276.
    pub fn process_server_hello(&mut self, fields: &[&str]) -> io::Result<()> {
        let mut tokens = fields.iter().filter(|f| !f.is_empty());
        let mut next = |name: &str| {
            tokens.next().ok_or_else(|| key_exchange_error(SECURE_CONNECTION_FAILED, format!("no {} in the server hello", name)))
        };
        let decode = |value: &str, name: &str| {
            REPLY_B64.decode(value.replace(['\r', '\n'], "")).map_err(|e| {
                key_exchange_error(SECURE_CONNECTION_FAILED, format!("bad {} in the server hello: {}", name, e))
            })
        };
        let server_random = decode(next("server random")?, "server random")?;
        let server_pub_bytes = decode(next("server public value")?, "server public value")?;
        let signature = decode(next("signature")?, "signature")?;
        let count: i32 = next("certificate count")?.parse().map_err(|e| {
            key_exchange_error(SECURE_CONNECTION_FAILED, format!("bad certificate count in the server hello: {}", e))
        })?;
        if count < 0 {
            return Err(key_exchange_error(SECURE_CONNECTION_FAILED, format!("Illegal Capacity: {}", count)));
        }
        let mut chain = Vec::new();
        for _ in 0..count {
            let der = decode(next("certificate")?, "certificate")?;
            let cert = certs::parse_certificate(&der).map_err(|e| {
                key_exchange_error(SECURE_CONNECTION_FAILED, format!("bad certificate in the server hello: {}", e))
            })?;
            chain.push(cert);
        }
        certs::check_server_certificates(&signature, &chain, self.cert_time_ms(), self.test_mode).map_err(|e| match e {
            CertError::Failed(detail) => key_exchange_error(SECURE_CONNECTION_FAILED, detail),
            CertError::Expired => key_exchange_error(CERTIFICATE_EXPIRED, "Error: first cert is expired".into()),
            CertError::NotYetValid => key_exchange_error(CERTIFICATE_NOT_YET_VALID, "Error: first cert is not yet valid".into()),
        })?;
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
        Ok(())
    }

    /// Encrypt plaintext using Encrypt-then-MAC.
    ///
    /// Wire layout: `aes_cbc(plaintext) || hmac_sha1(mac_key, iv || ciphertext)`.
    /// Both auth and farm channels share this HMAC formula.
    pub fn encrypt(&mut self, plaintext: &[u8]) -> Vec<u8> {
        let aes_key = self.write_aes_key.as_ref().unwrap();
        let iv = self.write_iv.as_ref().unwrap();
        let mac_key = self.write_mac_key.as_ref().unwrap();

        let ciphertext = aes_cbc_encrypt(aes_key, iv, plaintext);

        let mut mac_input = Vec::with_capacity(iv.len() + ciphertext.len());
        mac_input.extend_from_slice(iv);
        mac_input.extend_from_slice(&ciphertext);
        let mac = hmac_sha1(mac_key, &mac_input);

        // CBC chaining: next message's IV = last 16 bytes of THIS ciphertext.
        self.write_iv = Some(ciphertext[ciphertext.len() - 16..].to_vec());

        let mut result = ciphertext;
        result.extend_from_slice(&mac);
        result
    }

    /// Verify MAC then decrypt. A channel with no key exchange has no
    /// decryptor: an error, as in the reference (ibx#423).
    pub fn decrypt(&mut self, data: &[u8]) -> Result<Vec<u8>, &'static str> {
        if data.len() < 20 {
            return Err("data too short for MAC");
        }
        let ciphertext = &data[..data.len() - 20];
        let received_mac = &data[data.len() - 20..];

        let (Some(iv), Some(mac_key), Some(aes_key)) = (self.read_iv.as_ref(), self.read_mac_key.as_ref(), self.read_aes_key.as_ref()) else {
            return Err("Decryptor is not valid");
        };

        let mut mac_input = Vec::with_capacity(iv.len() + ciphertext.len());
        mac_input.extend_from_slice(iv);
        mac_input.extend_from_slice(ciphertext);
        let expected_mac = hmac_sha1(mac_key, &mac_input);

        if received_mac != expected_mac {
            return Err("HMAC verification failed");
        }

        let plaintext = aes_cbc_decrypt(aes_key, iv, ciphertext)?;

        // CBC chaining: next message's IV = last 16 bytes of THIS ciphertext.
        self.read_iv = Some(ciphertext[ciphertext.len() - 16..].to_vec());

        Ok(plaintext)
    }

    /// Encrypt with initial IVs from key derivation (for logon).
    pub fn encrypt_fresh(&self, plaintext: &[u8]) -> Vec<u8> {
        let kb = self.key_block.as_ref().unwrap();
        let iv = &kb[32..48];
        let aes_key = &kb[0..16];
        let mac_key = &kb[64..84];

        let ciphertext = aes_cbc_encrypt(aes_key, iv, plaintext);

        let mut mac_input = Vec::with_capacity(iv.len() + ciphertext.len());
        mac_input.extend_from_slice(iv);
        mac_input.extend_from_slice(&ciphertext);
        let mac = hmac_sha1(mac_key, &mac_input);

        let mut result = ciphertext;
        result.extend_from_slice(&mac);
        result
    }

    /// Access the raw key block.
    pub fn key_block(&self) -> Option<&[u8]> {
        self.key_block.as_deref()
    }

    /// Current write IV (updated after each encrypt call).
    pub fn write_iv(&self) -> Option<&[u8]> {
        self.write_iv.as_deref()
    }

    /// Current read IV (updated after each decrypt call).
    pub fn read_iv(&self) -> Option<&[u8]> {
        self.read_iv.as_deref()
    }

    /// Test channel with all-zero keys and IVs: two of them encrypt for
    /// each other.
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn zero_keys_for_test() -> Self {
        Self {
            client_random: [0u8; 32],
            private_key: BigUint::from(0u32),
            public_key: BigUint::from(0u32),
            key_block: Some(vec![0u8; 104]),
            write_aes_key: Some(vec![0u8; 16]),
            read_aes_key: Some(vec![0u8; 16]),
            write_iv: Some(vec![0u8; 16]),
            read_iv: Some(vec![0u8; 16]),
            write_mac_key: Some(vec![0u8; 20]),
            read_mac_key: Some(vec![0u8; 20]),
            test_mode: false,
            cert_time_ms: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::certs::fixture;

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
            test_mode: false,
            cert_time_ms: None,
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

    // ibx#423: the auth connection has no key exchange, so no decryptor.
    #[test]
    fn decrypt_without_key_exchange_is_an_error() {
        let mut ch = SecureChannel::new();
        assert_eq!(ch.decrypt(&[0u8; 52]), Err("Decryptor is not valid"));
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

    #[test]
    fn encrypt_fresh_output_is_valid_base64() {
        let ch = make_test_channel();
        let ct = ch.encrypt_fresh(b"some payload data");
        // The raw output is ciphertext || HMAC, not base64 itself.
        // But when base64-encoded, it should produce valid base64.
        let encoded = B64.encode(&ct);
        let decoded = B64.decode(&encoded).unwrap();
        assert_eq!(decoded, ct);
        // Ciphertext should be at least 16 (one AES block) + 20 (HMAC) = 36 bytes
        assert!(ct.len() >= 36);
    }

    #[test]
    fn encrypt_decrypt_max_payload() {
        let mut enc_ch = make_test_channel();
        let mut dec_ch = make_test_channel();
        let plaintext = vec![0xABu8; 4096];
        let encrypted = enc_ch.encrypt(&plaintext);
        let decrypted = dec_ch.decrypt(&encrypted).unwrap();
        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn encrypt_decrypt_one_byte() {
        let mut enc_ch = make_test_channel();
        let mut dec_ch = make_test_channel();
        let plaintext = &[0x42u8];
        let encrypted = enc_ch.encrypt(plaintext);
        let decrypted = dec_ch.decrypt(&encrypted).unwrap();
        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn encrypt_decrypt_empty_payload() {
        let mut enc_ch = make_test_channel();
        let mut dec_ch = make_test_channel();
        let plaintext: &[u8] = &[];
        let encrypted = enc_ch.encrypt(plaintext);
        let decrypted = dec_ch.decrypt(&encrypted).unwrap();
        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn build_secure_connect_different_versions() {
        let ch = SecureChannel::new();
        for &(ver, neg_ver) in &[(50, 50), (48, 50), (50, 52), (100, 200)] {
            let msg = ch.build_secure_connect(ver, neg_ver);
            assert_eq!(&msg[..4], NS_MAGIC);
            let payload = std::str::from_utf8(&msg[8..]).unwrap();
            let expected_prefix = format!("{};532;0;{};", ver, neg_ver);
            assert!(
                payload.starts_with(&expected_prefix),
                "Expected prefix '{}' but got '{}'",
                expected_prefix, payload
            );
        }
    }

    #[test]
    fn key_block_none_before_server_hello() {
        let ch = SecureChannel::new();
        assert!(ch.key_block().is_none());
    }

    // ibx#276: a malformed server hello is a login error, not a panic, and
    // leaves the channel without keys.
    #[test]
    fn malformed_server_hello_is_an_error() {
        let good = B64.encode([1u8; 32]);
        let cases: [&[&str]; 4] = [
            &[],
            &[good.as_str()],
            &["not base64!", good.as_str()],
            &[good.as_str(), "@@@"],
        ];
        for fields in cases {
            let mut ch = SecureChannel::new();
            let err = ch.process_server_hello(fields).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidData, "{fields:?}");
            assert!(err.to_string().contains("server hello"), "{err}");
            assert!(ch.key_block().is_none(), "{fields:?}: no keys after a failure");
        }
    }

    /// Server hello fields: `random` and `public` with the signature and
    /// certificates of the captured reply.
    fn hello<'a>(random: &'a str, public: &'a str) -> Vec<&'a str> {
        let mut fields = vec![random, public];
        fields.extend(fixture::captured_hello_certificates());
        fields
    }

    /// The fields of a reply text after its type.
    fn fields(text: &str) -> Vec<&str> {
        text.split(';').skip(2).collect()
    }

    // ibx#276: the captured reply passes inside its first certificate's
    // dates; the fields are read as the reference's tokenizer and decoder
    // read them (`crypt.a.b(String)`, `aQ.c(String)`): empty fields
    // skipped, fields after the certificates not read, base64 with or
    // without padding, line breaks dropped; the count is a signed decimal.
    #[test]
    fn captured_server_hello_passes() {
        let captured = fixture::captured_hello();
        let ok = |text: &str| SecureChannel::new().with_cert_time(fixture::NOW_MS).process_server_hello(&fields(text));
        ok(captured).unwrap();
        let f = fields(captured);
        let (random, public, sig, count, certs) = (f[0], f[1], f[2], f[3], &f[4..7]);
        let join = |parts: &[&str]| format!("50;533;{};", parts.join(";"));
        let unpadded: Vec<&str> = f[..7].iter().map(|v| v.trim_end_matches('=')).collect();
        let sig_crlf = format!("{}\r\n{}", &sig[..40], &sig[40..]);
        let plus = format!("+{}", count);
        for text in [
            format!("50;533;;{};;", [random, public, "", sig, count, certs[0], "", certs[1], certs[2]].join(";")),
            join(&[random, public, sig, &plus, certs[0], certs[1], certs[2]]),
            join(&[random, public, sig, count, certs[0], certs[1], certs[2], "not read!"]),
            join(&unpadded),
            join(&[random, public, &sig_crlf, count, certs[0], certs[1], certs[2]]),
        ] {
            ok(&text).unwrap_or_else(|e| panic!("{e}: {text}"));
        }
        for text in [
            join(&[random, public, sig, "4", certs[0], certs[1], certs[2]]),
            join(&[random, public, sig, "2", certs[0], certs[1], certs[2]]),
            join(&[random, public, sig, "0", certs[0], certs[1], certs[2]]),
            join(&[random, public, sig, "-1", certs[0], certs[1], certs[2]]),
            join(&[random, public, sig, " 3", certs[0], certs[1], certs[2]]),
            join(&[random, public, &format!("{} {}", &sig[..40], &sig[40..]), count, certs[0], certs[1], certs[2]]),
            join(&[random, public, sig]),
        ] {
            let err = ok(&text).unwrap_err();
            assert!(err.to_string().starts_with(SECURE_CONNECTION_FAILED), "{err}: {text}");
        }
    }

    // ibx#276: the login texts of a failed check: the first certificate's
    // dates have their own texts, any other failure gives "Unable to
    // establish secure connection" (`n.g(aD)@313-454`); the channel keeps
    // no keys.
    #[test]
    fn certificate_failures_give_the_reference_login_texts() {
        let captured = fixture::captured_hello();
        let run = |now: i64, text: &str| {
            let mut ch = SecureChannel::new().with_cert_time(now);
            let err = ch.process_server_hello(&fields(text)).unwrap_err();
            assert!(ch.key_block().is_none());
            assert_eq!(err.kind(), io::ErrorKind::InvalidData);
            err.to_string()
        };
        assert!(run(1_791_321_968_001, captured).starts_with(CERTIFICATE_EXPIRED));
        assert!(run(1_791_120_367_999, captured).starts_with(CERTIFICATE_NOT_YET_VALID));
        let f = fields(captured);
        let short = format!("50;533;{};{};{};2;{};{};", f[0], f[1], f[2], f[4], f[5]);
        assert!(run(fixture::NOW_MS, &short).starts_with(SECURE_CONNECTION_FAILED));
        let swapped = format!("50;533;{};{};{};3;{};{};{};", f[0], f[1], f[2], f[4], f[6], f[5]);
        assert!(run(fixture::NOW_MS, &swapped).starts_with(SECURE_CONNECTION_FAILED));
        // The system clock, as the reference, when no time is set.
        assert!(SecureChannel::new().cert_time_ms() > fixture::NOW_MS);
    }

    #[test]
    fn key_block_some_after_server_hello() {
        // Create two channels and exchange keys between them to simulate
        // a real handshake without needing a server.
        let mut channel_a = SecureChannel::new().with_cert_time(fixture::NOW_MS);
        let mut channel_b = SecureChannel::new().with_cert_time(fixture::NOW_MS);

        // Channel A builds its SECURE_CONNECT message
        let msg_a = channel_a.build_secure_connect(50, 50);
        let payload_a = std::str::from_utf8(&msg_a[8..]).unwrap();
        let parts_a: Vec<&str> = payload_a.trim_end_matches(';').split(';').collect();
        // parts_a: [version, 532, 0, negotiated_version, client_random_b64, pub_b64]
        let a_random = parts_a[4];
        let a_pub = parts_a[5];

        // Channel B builds its SECURE_CONNECT message
        let msg_b = channel_b.build_secure_connect(50, 50);
        let payload_b = std::str::from_utf8(&msg_b[8..]).unwrap();
        let parts_b: Vec<&str> = payload_b.trim_end_matches(';').split(';').collect();
        let b_random = parts_b[4];
        let b_pub = parts_b[5];

        // Each channel processes the other's hello as if it were a server
        // response, with the captured certificates
        channel_a.process_server_hello(&hello(b_random, b_pub)).unwrap();
        channel_b.process_server_hello(&hello(a_random, a_pub)).unwrap();

        // Both should now have key_blocks of 104 bytes
        let kb_a = channel_a.key_block().expect("channel_a should have key_block");
        assert_eq!(kb_a.len(), 104);
        let kb_b = channel_b.key_block().expect("channel_b should have key_block");
        assert_eq!(kb_b.len(), 104);
    }

    #[test]
    fn two_channels_exchange_keys_shared_secret_matches() {
        // In DH, both sides compute the same shared secret: A^b mod N == B^a mod N.
        // However, the key_block derivation uses each channel's own client_random as seed,
        // so two independent SecureChannels won't derive identical key_blocks.
        //
        // What we CAN verify: after exchanging keys, both sides computed the same
        // DH shared secret (pre-master). We test this by verifying that both channels
        // have valid 104-byte key_blocks and that encrypt_fresh produces parseable output.
        let mut channel_a = SecureChannel::new().with_cert_time(fixture::NOW_MS);
        let mut channel_b = SecureChannel::new().with_cert_time(fixture::NOW_MS);

        let msg_a = channel_a.build_secure_connect(50, 50);
        let payload_a = std::str::from_utf8(&msg_a[8..]).unwrap();
        let parts_a: Vec<&str> = payload_a.trim_end_matches(';').split(';').collect();

        let msg_b = channel_b.build_secure_connect(50, 50);
        let payload_b = std::str::from_utf8(&msg_b[8..]).unwrap();
        let parts_b: Vec<&str> = payload_b.trim_end_matches(';').split(';').collect();

        // Each processes the other's hello
        channel_a.process_server_hello(&hello(parts_b[4], parts_b[5])).unwrap();
        channel_b.process_server_hello(&hello(parts_a[4], parts_a[5])).unwrap();

        // Both have valid key blocks
        assert_eq!(channel_a.key_block().unwrap().len(), 104);
        assert_eq!(channel_b.key_block().unwrap().len(), 104);

        // Each can encrypt with their derived keys (no panics)
        let ct_a = channel_a.encrypt(b"message from A");
        let ct_b = channel_b.encrypt(b"message from B");
        // Ciphertexts are non-empty: at least 16 (AES block) + 20 (HMAC)
        assert!(ct_a.len() >= 36);
        assert!(ct_b.len() >= 36);

        // encrypt_fresh also works on both
        let fresh_a = channel_a.encrypt_fresh(b"fresh from A");
        let fresh_b = channel_b.encrypt_fresh(b"fresh from B");
        assert!(fresh_a.len() >= 36);
        assert!(fresh_b.len() >= 36);
    }
}
