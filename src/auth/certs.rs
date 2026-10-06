//! Certificate checks of the key-exchange reply (533), as the reference
//! makes them in `twslaunch.jauthentication.crypt.a.a(List)` (ibx#276).
//!
//! The reference leaves part of the work to its runtime (JRE 17): the
//! certificate parse (`CertificateFactory`), the RSA signature verify, the
//! certificate dates and the PKIX path check (`PKIXCertPathValidator`,
//! revocation off). This module does what those calls do for the checks the
//! reference makes, so a reply passes or fails here as it does there. The
//! outcomes of the cases in `tests/fixtures/key_exchange/certificates.txt`
//! are the reference's own check run on that runtime.

use digest::Digest;
use num_bigint::{BigInt, BigUint};
use x509_cert::certificate::{CertificateInner, Raw, Version};
use x509_cert::der::asn1::{Any, BitString, OctetString, Uint};
use x509_cert::der::{Decode, Encode, Tag, Tagged};
use x509_cert::ext::pkix::name::GeneralName;
use x509_cert::ext::pkix::{AuthorityKeyIdentifier, CertificatePolicies, NameConstraints, PolicyConstraints, PolicyMappings};
use x509_cert::name::Name;
use x509_cert::spki::ObjectIdentifier as Oid;

/// Issuer prefix of the first certificate and subject prefix of the second
/// (`crypt.a.q`, `crypt.a.r`), as the reference's `Principal.getName()`
/// writes the name: the last RDN of the encoding first, `", "` between RDNs.
const PROD_CN: &str = "prod.ckg.ibllc.com";
const TEST_CN: &str = "test.ckg.ibllc.com";

/// A failed check: the reference's `crypt.e` results. Only the first
/// certificate's dates have their own outcome (`e.a(SslCertificateValidity)`,
/// with their own login texts); every other failure is `e.c`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CertError {
    /// `crypt.e.c`, with the reason the reference logs.
    Failed(String),
    /// `SslCertificateValidity.NOT_YET_VALID`.
    NotYetValid,
    /// `SslCertificateValidity.EXPIRED`.
    Expired,
}

fn failed(reason: impl Into<String>) -> CertError {
    CertError::Failed(reason.into())
}

// ── Object identifiers ──

const OID_RSA: Oid = Oid::new_unwrap("1.2.840.113549.1.1.1");
const OID_CN: Oid = Oid::new_unwrap("2.5.4.3");
const OID_EMAIL: Oid = Oid::new_unwrap("1.2.840.113549.1.9.1");
const EXT_SKI: Oid = Oid::new_unwrap("2.5.29.14");
const EXT_KU: Oid = Oid::new_unwrap("2.5.29.15");
const EXT_SAN: Oid = Oid::new_unwrap("2.5.29.17");
const EXT_BC: Oid = Oid::new_unwrap("2.5.29.19");
const EXT_NC: Oid = Oid::new_unwrap("2.5.29.30");
const EXT_CP: Oid = Oid::new_unwrap("2.5.29.32");
const EXT_PM: Oid = Oid::new_unwrap("2.5.29.33");
const EXT_AKI: Oid = Oid::new_unwrap("2.5.29.35");
const EXT_PC: Oid = Oid::new_unwrap("2.5.29.36");
const EXT_EKU: Oid = Oid::new_unwrap("2.5.29.37");
const EXT_IAP: Oid = Oid::new_unwrap("2.5.29.54");
const ANY_POLICY: Oid = Oid::new_unwrap("2.5.29.32.0");

/// Critical extensions the path checkers process (`KeyChecker`,
/// `ConstraintsChecker`, `PolicyChecker`); any other critical extension of a
/// path certificate fails the path.
const PATH_EXTENSIONS: [Oid; 9] = [EXT_KU, EXT_EKU, EXT_SAN, EXT_BC, EXT_NC, EXT_CP, EXT_PM, EXT_PC, EXT_IAP];

/// Keywords of the runtime's RFC 2253 canonical names (`AVAKeyword`); other
/// attributes are written with their dotted OID and a hex value.
const RFC2253_KEYWORDS: [(&str, &str); 9] = [
    ("2.5.4.3", "CN"),
    ("2.5.4.6", "C"),
    ("2.5.4.7", "L"),
    ("2.5.4.8", "ST"),
    ("2.5.4.10", "O"),
    ("2.5.4.11", "OU"),
    ("2.5.4.9", "STREET"),
    ("0.9.2342.19200300.100.1.25", "DC"),
    ("0.9.2342.19200300.100.1.1", "UID"),
];

// ── Signatures ──

/// Digest of an RSA PKCS#1 v1.5 signature algorithm the runtime verifies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Hash {
    Md5,
    Sha1,
    Sha224,
    Sha256,
    Sha384,
    Sha512,
    Sha512_224,
    Sha512_256,
}

impl Hash {
    fn of_signature_oid(oid: &Oid) -> Option<(Self, &'static str)> {
        Some(match oid.to_string().as_str() {
            "1.2.840.113549.1.1.4" => (Self::Md5, "MD5withRSA"),
            "1.2.840.113549.1.1.5" | "1.3.14.3.2.29" => (Self::Sha1, "SHA1withRSA"),
            "1.2.840.113549.1.1.14" => (Self::Sha224, "SHA224withRSA"),
            "1.2.840.113549.1.1.11" => (Self::Sha256, "SHA256withRSA"),
            "1.2.840.113549.1.1.12" => (Self::Sha384, "SHA384withRSA"),
            "1.2.840.113549.1.1.13" => (Self::Sha512, "SHA512withRSA"),
            "1.2.840.113549.1.1.15" => (Self::Sha512_224, "SHA512/224withRSA"),
            "1.2.840.113549.1.1.16" => (Self::Sha512_256, "SHA512/256withRSA"),
            _ => return None,
        })
    }

    fn oid(self) -> Oid {
        Oid::new_unwrap(match self {
            Self::Md5 => "1.2.840.113549.2.5",
            Self::Sha1 => "1.3.14.3.2.26",
            Self::Sha224 => "2.16.840.1.101.3.4.2.4",
            Self::Sha256 => "2.16.840.1.101.3.4.2.1",
            Self::Sha384 => "2.16.840.1.101.3.4.2.2",
            Self::Sha512 => "2.16.840.1.101.3.4.2.3",
            Self::Sha512_224 => "2.16.840.1.101.3.4.2.5",
            Self::Sha512_256 => "2.16.840.1.101.3.4.2.6",
        })
    }

    fn digest(self, data: &[u8]) -> Vec<u8> {
        match self {
            Self::Md5 => md5::Md5::digest(data).to_vec(),
            Self::Sha1 => sha1::Sha1::digest(data).to_vec(),
            Self::Sha224 => sha2::Sha224::digest(data).to_vec(),
            Self::Sha256 => sha2::Sha256::digest(data).to_vec(),
            Self::Sha384 => sha2::Sha384::digest(data).to_vec(),
            Self::Sha512 => sha2::Sha512::digest(data).to_vec(),
            Self::Sha512_224 => sha2::Sha512_224::digest(data).to_vec(),
            Self::Sha512_256 => sha2::Sha512_256::digest(data).to_vec(),
        }
    }
}

fn der_tlv(tag: u8, body: &[u8]) -> Vec<u8> {
    let mut out = vec![tag];
    let len = body.len();
    if len < 0x80 {
        out.push(len as u8);
    } else {
        let bytes = len.to_be_bytes();
        let skip = bytes.iter().take_while(|b| **b == 0).count();
        out.push(0x80 | (bytes.len() - skip) as u8);
        out.extend_from_slice(&bytes[skip..]);
    }
    out.extend_from_slice(body);
    out
}

/// Header length of the DER element at the start of `b` (`b` decoded).
fn header_len(b: &[u8]) -> usize {
    if b[1] < 0x80 { 2 } else { 2 + (b[1] & 0x7f) as usize }
}

/// Length of the DER element at the start of `b` (header included), for a
/// definite length.
fn element_len(b: &[u8]) -> Option<usize> {
    let first = *b.get(1)? as usize;
    if first < 0x80 {
        return Some(2 + first);
    }
    let n = first & 0x7f;
    if n == 0 || n > 4 || b.len() < 2 + n {
        return None;
    }
    let len = b[2..2 + n].iter().fold(0usize, |acc, x| (acc << 8) | *x as usize);
    Some(2 + n + len)
}

/// An RSA public key as the runtime accepts it (`RSAKeyFactory`): 512 to
/// 16384 bits, an exponent of at most 64 bits above 3072 bits.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RsaKey {
    n: BigUint,
    e: BigUint,
}

impl RsaKey {
    fn of(spki: &x509_cert::spki::SubjectPublicKeyInfoOwned) -> Option<Self> {
        if spki.algorithm.oid != OID_RSA || spki.algorithm.parameters.as_ref().is_some_and(|p| !p.is_null()) {
            return None;
        }
        let ints = Vec::<Uint>::from_der(spki.subject_public_key.as_bytes()?).ok()?;
        let [n, e] = ints.as_slice() else { return None };
        let key = Self { n: BigUint::from_bytes_be(n.as_bytes()), e: BigUint::from_bytes_be(e.as_bytes()) };
        let bits = key.n.bits();
        if !(512..=16384).contains(&bits) || (bits > 3072 && key.e.bits() > 64) {
            return None;
        }
        Some(key)
    }

    fn bits(&self) -> u64 {
        self.n.bits()
    }

    /// Byte length of the modulus (`RSACore.getByteLength`).
    fn len(&self) -> usize {
        self.bits().div_ceil(8) as usize
    }
}

/// The runtime's `Signature.verify` of a PKCS#1 v1.5 RSA signature
/// (`RSASignature.engineVerify`): a signature whose length is not the
/// modulus length throws; a value at or above the modulus is `false`; the
/// encoded digest is accepted with or without the NULL parameters of its
/// algorithm.
fn rsa_verify(key: &RsaKey, hash: Hash, data: &[u8], sig: &[u8]) -> Result<bool, String> {
    let k = key.len();
    if sig.len() != k {
        return Err(format!("Bad signature length: got {} but was expecting {}", sig.len(), k));
    }
    let s = BigUint::from_bytes_be(sig);
    if s >= key.n {
        return Ok(false);
    }
    let m = s.modpow(&key.e, &key.n).to_bytes_be();
    let mut decrypted = vec![0u8; k - m.len()];
    decrypted.extend_from_slice(&m);
    let digest = hash.digest(data);
    let oid = der_tlv(0x06, hash.oid().as_bytes());
    for with_null in [true, false] {
        let mut alg = oid.clone();
        if with_null {
            alg.extend_from_slice(&[0x05, 0x00]);
        }
        let mut info = der_tlv(0x30, &alg);
        info.extend_from_slice(&der_tlv(0x04, &digest));
        let info = der_tlv(0x30, &info);
        if info.len() + 11 > k {
            continue;
        }
        let mut padded = vec![0x00, 0x01];
        padded.resize(k - info.len() - 1, 0xff);
        padded.push(0x00);
        padded.extend_from_slice(&info);
        if padded == decrypted {
            return Ok(true);
        }
    }
    Ok(false)
}

// ── Names ──

/// A distinguished name with the runtime's RFC 2253 canonical form
/// (`X500Name.getRFC2253CanonicalName`), RDN by RDN in the order of the
/// encoding, each RDN's values sorted: two names are equal when their
/// canonical forms are (`X500Principal.equals`).
#[derive(Debug, Clone)]
struct DName {
    name: Name,
    canonical: Vec<Vec<String>>,
}

impl PartialEq for DName {
    fn eq(&self, other: &Self) -> bool {
        self.canonical == other.canonical
    }
}

impl DName {
    fn new(name: &Name) -> Self {
        let canonical = name
            .iter_rdn()
            .map(|rdn| {
                let mut avas: Vec<String> = rdn.iter().map(|a| canonical_ava(&a.oid, &a.value)).collect();
                avas.sort();
                avas
            })
            .collect();
        Self { name: name.clone(), canonical }
    }

    /// No attribute at all (`X500Name.isEmpty`).
    fn is_empty(&self) -> bool {
        self.canonical.iter().all(Vec::is_empty)
    }

    /// The name as `Principal.getName()` writes it (`X500Name.toString`)
    /// starts with `CN=<cn>, `: two RDNs or more, the last one of the
    /// encoding a single common name whose value reads `cn`.
    fn starts_with_cn(&self, cn: &str) -> bool {
        let rdns: Vec<_> = self.name.iter_rdn().collect();
        if rdns.len() < 2 {
            return false;
        }
        let avas: Vec<_> = rdns[rdns.len() - 1].iter().collect();
        matches!(avas.as_slice(), [a] if a.oid == OID_CN && java_string(&a.value).as_deref() == Some(cn))
    }

    /// The common name of the last RDN that has one
    /// (`X500Name.findMostSpecificAttribute`).
    fn most_specific_cn(&self) -> Option<String> {
        let rdns: Vec<_> = self.name.iter_rdn().collect();
        rdns.iter().rev().find_map(|rdn| rdn.iter().find(|a| a.oid == OID_CN)).and_then(|a| java_string(&a.value))
    }

    /// `X500Name.isWithinSubtree`: `self` starts with the RDNs of `base`.
    fn is_within(&self, base: &DName) -> bool {
        base.canonical.len() <= self.canonical.len() && self.canonical[..base.canonical.len()] == base.canonical[..]
    }
}

/// The runtime's string of an attribute value (`DerValue.getAsString`), with
/// the character set of each string type; `None` for other types.
fn java_string(value: &Any) -> Option<String> {
    let b = value.value();
    let ascii = || b.iter().map(|c| if *c < 0x80 { *c as char } else { '\u{fffd}' }).collect();
    Some(match value.tag() {
        Tag::Utf8String => String::from_utf8_lossy(b).into_owned(),
        Tag::PrintableString | Tag::Ia5String | Tag::GeneralString => ascii(),
        Tag::TeletexString => b.iter().map(|c| *c as char).collect(),
        Tag::BmpString => {
            let units: Vec<u16> = b.chunks(2).map(|c| if c.len() == 2 { u16::from_be_bytes([c[0], c[1]]) } else { 0xfffd }).collect();
            String::from_utf16_lossy(&units)
        }
        _ => return None,
    })
}

/// `AVA.toRFC2253CanonicalString`: keyword or dotted OID, then the value:
/// a PrintableString or UTF8String as text (escaped, inner spaces
/// collapsed, trimmed), any other value as `#` and the hex of its encoding;
/// the whole upper- then lower-cased.
fn canonical_ava(oid: &Oid, value: &Any) -> String {
    let dotted = oid.to_string();
    let keyword = RFC2253_KEYWORDS.iter().find(|(o, _)| *o == dotted).map(|(_, k)| *k);
    let text = matches!(value.tag(), Tag::PrintableString | Tag::Utf8String);
    let mut out = format!("{}=", keyword.unwrap_or(&dotted));
    match keyword {
        Some(_) if text => {
            let s = String::from_utf8_lossy(value.value());
            let mut v = String::new();
            let mut previous_white = false;
            for (i, c) in s.chars().enumerate() {
                let escapee = ",+<>;\"\\".contains(c);
                if is_printable_char(c) || escapee || (i == 0 && c == '#') {
                    if escapee || (i == 0 && c == '#') {
                        v.push('\\');
                    }
                    if c != ' ' {
                        previous_white = false;
                        v.push(c);
                    } else if !previous_white {
                        previous_white = true;
                        v.push(c);
                    }
                } else {
                    previous_white = false;
                    v.push(c);
                }
            }
            out.push_str(v.trim_matches(|c: char| c <= ' '));
        }
        _ => {
            out.push('#');
            out.push_str(&hex::encode(value.to_der().unwrap_or_default()));
        }
    }
    out.to_uppercase().to_lowercase()
}

/// `DerValue.isPrintableStringChar`.
fn is_printable_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || " '()+,-./:=?".contains(c)
}

// ── Certificates ──

/// A certificate of the reply, parsed as the runtime's `CertificateFactory`
/// does, with the extensions the checks read.
#[derive(Debug, Clone)]
pub struct Certificate {
    tbs: Vec<u8>,
    signature_algorithm: Oid,
    signature: Vec<u8>,
    version: u8,
    serial: BigInt,
    issuer: DName,
    subject: DName,
    not_before: i64,
    not_after: i64,
    key: Option<RsaKey>,
    critical: Vec<Oid>,
    /// `getBasicConstraints()`: -1 when not a CA, else the path length.
    basic_constraints: i64,
    key_usage: Option<Vec<bool>>,
    aki_key_id: Option<Vec<u8>>,
    aki_serial: Option<BigInt>,
    ski: Option<Vec<u8>>,
    /// Certificate policies: critical flag, then each policy and whether it
    /// has qualifiers.
    policies: Option<(bool, Vec<(Oid, bool)>)>,
    /// Policy constraints: require explicit policy, inhibit policy mapping,
    /// -1 when absent.
    policy_constraints: Option<(i64, i64)>,
    policy_mappings: Option<Vec<(Oid, Oid)>>,
    inhibit_any_policy: Option<i64>,
    name_constraints: Option<NameConstraints>,
    alt_names: Option<Vec<GeneralName>>,
}

/// An algorithm identifier as the runtime compares them (`AlgorithmId`): a
/// NULL parameter is no parameter.
fn same_algorithm(a: &x509_cert::spki::AlgorithmIdentifierOwned, b: &x509_cert::spki::AlgorithmIdentifierOwned) -> bool {
    let params = |p: &Option<Any>| p.clone().filter(|p| !p.is_null());
    a.oid == b.oid && params(&a.parameters) == params(&b.parameters)
}

/// `BasicConstraintsExtension`: a CA flag first, then an optional path
/// length (no path length: no limit); a CA flag missing or not first is
/// not a CA.
fn basic_constraints(value: &[u8]) -> Result<i64, String> {
    let items = Vec::<Any>::from_der(value).map_err(|e| e.to_string())?;
    let ca = match items.first() {
        Some(first) if first.tag() == Tag::Boolean => first.value().first().is_some_and(|b| *b != 0),
        _ => return Ok(-1),
    };
    let path_len = match items.get(1) {
        None => i32::MAX as i64,
        Some(len) if len.tag() == Tag::Integer => {
            let v = BigInt::from_signed_bytes_be(len.value());
            i64::try_from(v).ok().filter(|v| i32::try_from(*v).is_ok()).ok_or("path length out of range")?
        }
        Some(_) => return Err("Invalid encoding of BasicConstraints".into()),
    };
    Ok(if ca { path_len } else { -1 })
}

/// Parse one certificate of the reply as `CertificateFactory
/// .generateCertificate` does: one DER element, the bytes after it are not
/// read; the inner and outer signature algorithms must match; an extension
/// twice or a known critical extension that does not decode fails the
/// certificate, a known non-critical one that does not decode is ignored.
pub fn parse_certificate(der: &[u8]) -> Result<Certificate, String> {
    if der.first() != Some(&0x30) {
        return Err("not a DER certificate".into());
    }
    let len = element_len(der).filter(|l| *l <= der.len()).ok_or("truncated certificate")?;
    let der = &der[..len];
    let cert = CertificateInner::<Raw>::from_der(der).map_err(|e| e.to_string())?;
    // The signed part as received (`X509CertInfo.getEncodedInfo`).
    let body = &der[header_len(der)..];
    let tbs = &body[..element_len(body).ok_or("bad certificate body")?];
    let info = cert.tbs_certificate();
    if !same_algorithm(cert.signature_algorithm(), info.signature()) {
        return Err("Signature algorithm mismatch".into());
    }

    let mut c = Certificate {
        tbs: tbs.to_vec(),
        signature_algorithm: cert.signature_algorithm().oid,
        signature: cert.signature().raw_bytes().to_vec(),
        version: match info.version() {
            Version::V1 => 1,
            Version::V2 => 2,
            Version::V3 => 3,
        },
        serial: BigInt::from_signed_bytes_be(info.serial_number().as_bytes()),
        issuer: DName::new(info.issuer()),
        subject: DName::new(info.subject()),
        not_before: info.validity().not_before.to_unix_duration().as_millis() as i64,
        not_after: info.validity().not_after.to_unix_duration().as_millis() as i64,
        key: RsaKey::of(info.subject_public_key_info()),
        critical: Vec::new(),
        basic_constraints: -1,
        key_usage: None,
        aki_key_id: None,
        aki_serial: None,
        ski: None,
        policies: None,
        policy_constraints: None,
        policy_mappings: None,
        inhibit_any_policy: None,
        name_constraints: None,
        alt_names: None,
    };
    let exts = info.extensions().map(Vec::as_slice).unwrap_or_default();
    for (i, ext) in exts.iter().enumerate() {
        if exts[..i].iter().any(|e| e.extn_id == ext.extn_id) {
            return Err("Duplicate extensions not allowed".into());
        }
        if ext.critical {
            c.critical.push(ext.extn_id);
        }
        let v = ext.extn_value.as_bytes();
        let read = match ext.extn_id {
            id if id == EXT_KU => BitString::from_der(v).map(|b| {
                c.key_usage = Some((0..b.bit_len().max(9)).map(|i| i < b.bit_len() && b.raw_bytes()[i / 8] & (0x80 >> (i % 8)) != 0).collect());
            }).map_err(|e| e.to_string()),
            id if id == EXT_BC => basic_constraints(v).map(|bc| c.basic_constraints = bc),
            id if id == EXT_AKI => AuthorityKeyIdentifier::from_der(v).map(|aki| {
                c.aki_key_id = aki.key_identifier.map(|k| k.as_bytes().to_vec());
                c.aki_serial = aki.authority_cert_serial_number.map(|s| BigInt::from_signed_bytes_be(s.as_bytes()));
            }).map_err(|e| e.to_string()),
            id if id == EXT_SKI => OctetString::from_der(v).map(|k| c.ski = Some(k.as_bytes().to_vec())).map_err(|e| e.to_string()),
            id if id == EXT_CP => CertificatePolicies::from_der(v).map(|cp| {
                let items = cp.0.iter().map(|p| (p.policy_identifier, p.policy_qualifiers.as_ref().is_some_and(|q| !q.is_empty()))).collect();
                c.policies = Some((ext.critical, items));
            }).map_err(|e| e.to_string()),
            id if id == EXT_PC => PolicyConstraints::from_der(v).map(|pc| {
                let n = |v: Option<u32>| v.map_or(-1, i64::from);
                c.policy_constraints = Some((n(pc.require_explicit_policy), n(pc.inhibit_policy_mapping)));
            }).map_err(|e| e.to_string()),
            id if id == EXT_PM => PolicyMappings::from_der(v).map(|pm| {
                c.policy_mappings = Some(pm.0.iter().map(|m| (m.issuer_domain_policy, m.subject_domain_policy)).collect());
            }).map_err(|e| e.to_string()),
            id if id == EXT_IAP => u32::from_der(v).map(|n| c.inhibit_any_policy = Some(n.into())).map_err(|e| e.to_string()),
            id if id == EXT_NC => NameConstraints::from_der(v).map(|nc| c.name_constraints = Some(nc)).map_err(|e| e.to_string()),
            id if id == EXT_SAN => Vec::<GeneralName>::from_der(v).map(|names| c.alt_names = Some(names)).map_err(|e| e.to_string()),
            id if id == EXT_EKU => Vec::<Oid>::from_der(v).map(|_| ()).map_err(|e| e.to_string()),
            _ => Ok(()),
        };
        if let Err(e) = read
            && ext.critical
        {
            return Err(format!("extension {}: {}", ext.extn_id, e));
        }
    }
    // `X509CertInfo.verifyCert`: an empty subject needs a critical,
    // non-empty subject alternative name.
    if c.subject.is_empty() && !(c.critical.contains(&EXT_SAN) && c.alt_names.as_ref().is_some_and(|n| !n.is_empty())) {
        return Err("X.509 Certificate is incomplete: subject field is empty".into());
    }
    Ok(c)
}

impl Certificate {
    /// `X509Certificate.checkValidity(date)`: not yet valid first, then
    /// expired, to the millisecond.
    fn validity(&self, now_ms: i64) -> Result<(), CertError> {
        if self.not_before > now_ms {
            Err(CertError::NotYetValid)
        } else if self.not_after < now_ms {
            Err(CertError::Expired)
        } else {
            Ok(())
        }
    }

    /// `X509Certificate.verify(key)`: the certificate's own algorithm over its
    /// signed part; a signature that does not match throws.
    fn verify_by(&self, key: Option<&RsaKey>) -> Result<(), String> {
        let (hash, _) = Hash::of_signature_oid(&self.signature_algorithm)
            .ok_or_else(|| format!("{} Signature not available", self.signature_algorithm))?;
        let key = key.ok_or("not an RSA key")?;
        match rsa_verify(key, hash, &self.tbs, &self.signature)? {
            true => Ok(()),
            false => Err("Signature does not match.".into()),
        }
    }

    /// `X509CertImpl.isSelfIssued`.
    fn self_issued(&self) -> bool {
        self.subject == self.issuer
    }
}

/// The issuer or subject prefix test of `crypt.a.c(String)`: the production
/// prefix, or in test mode (`[Communication] TestSecureConnect`, read once
/// by `crypt.d.a(int, boolean)`) the test or the production prefix.
fn prefix_ok(name: &DName, test_mode: bool) -> bool {
    name.starts_with_cn(PROD_CN) || (test_mode && name.starts_with_cn(TEST_CN))
}

/// `crypt.a.d(String)`: a test name in production mode, which the reference
/// logs as "test and production mixed".
fn mixed(name: &DName, test_mode: bool) -> bool {
    !test_mode && name.starts_with_cn(TEST_CN)
}

/// The checks of `crypt.a.a(List)` on the certificates of a reply, in the
/// reference's order, with `now_ms` as the current time (the reference
/// reads the system clock).
pub fn check_server_certificates(signature: &[u8], certs: &[Certificate], now_ms: i64, test_mode: bool) -> Result<(), CertError> {
    // @11-29: at least one certificate.
    let Some(first) = certs.first() else {
        return Err(failed("no certificate"));
    };
    // @132-194: `Signature("SHA1withRSA")`, `initVerify(cert[0] key)`,
    // `verify(signature)` with no `update()` before it; the result is
    // dropped (`pop` @170): only a throw fails, a wrong key or a signature
    // whose length is not the key's.
    let Some(key) = first.key.as_ref() else {
        return Err(failed("signature verification failed: not an RSA key"));
    };
    rsa_verify(key, Hash::Sha1, &[], signature).map_err(|e| failed(format!("signature verification failed: {}", e)))?;
    // @195-213.
    if certs.len() < 3 {
        return Err(failed("Error: too short cert chain"));
    }
    // @218-343: the first certificate's dates
    // (`SslCertificateValidity.evaluateValidity`).
    first.validity(now_ms)?;
    // @344-453: issuer of the first certificate (`getIssuerDN().getName()`).
    if !prefix_ok(&first.issuer, test_mode) {
        let mixed = if mixed(&first.issuer, test_mode) { ", test and production mixed." } else { "" };
        return Err(failed(format!("Error: the first cert is invalid{}", mixed)));
    }
    // @454-503: the first certificate signed by the second's key.
    first.verify_by(certs[1].key.as_ref()).map_err(|e| failed(format!("Error: first was not signed with second: {}", e)))?;
    // @504-613: subject of the second certificate.
    if !prefix_ok(&certs[1].subject, test_mode) {
        let mixed = if mixed(&certs[1].subject, test_mode) { ", test and production mixed" } else { "" };
        return Err(failed(format!("Error: the second cert is invaid{}", mixed)));
    }
    // @614-751: PKIX check of the path `cert[n-2] .. cert[1]` (built from
    // the end of `subList(1, n)`), trust anchor the last certificate,
    // revocation off. The validator reads a path from its end, so cert[1]
    // must be issued by the anchor, cert[2] by cert[1], and so on; cert[n-2]
    // is the path's target.
    let path: Vec<&Certificate> = certs[1..certs.len() - 1].iter().collect();
    pkix_validate(&path, &certs[certs.len() - 1], now_ms).map_err(|e| failed(format!("Error: chain verification failed: {}", e)))
}

// ── PKIX path check (`PKIXCertPathValidator`, revocation off) ──

/// The path check with default parameters: `path` in processing order
/// (issued by the anchor first). The checkers of the runtime, per
/// certificate: algorithm constraints (`jdk.certpath.disabledAlgorithms`:
/// MD2, MD5, RSA keys under 1024 bits), key usage of CA certificates, basic
/// and name constraints, policies, then signature, dates and name chaining;
/// a critical extension none of them processes fails the path.
fn pkix_validate(path: &[&Certificate], anchor: &Certificate, now_ms: i64) -> Result<(), String> {
    let n = path.len();
    let Some(first) = path.first() else { return Ok(()) };
    // The anchor must match the first certificate's issuer and authority
    // key identifier (`AdaptableX509CertSelector`).
    let anchor_matches = anchor.subject == first.issuer
        && match (&first.aki_key_id, &anchor.ski) {
            (Some(aki), Some(ski)) => aki == ski,
            _ => true,
        }
        && match &first.aki_serial {
            Some(serial) if anchor.version > 2 => *serial == anchor.serial,
            _ => true,
        };
    if !anchor_matches {
        return Err("Path does not chain with any of the trust anchors".into());
    }

    let mut prev_key = anchor.key.clone();
    let mut prev_subject = anchor.subject.clone();
    let mut max_path_len = n as i64;
    let mut name_constraints: Vec<&NameConstraints> = Vec::new();
    let mut policy = PolicyState::new(n);
    for (idx, cert) in path.iter().enumerate() {
        let i = idx + 1;
        let last = i == n;
        let mut unresolved = cert.critical.clone();

        // AlgorithmChecker.
        let (_, alg_name) = Hash::of_signature_oid(&cert.signature_algorithm).unwrap_or((Hash::Sha256, "unknown"));
        if alg_name == "MD5withRSA" {
            return Err(format!("Algorithm constraints check failed on signature algorithm: {}", alg_name));
        }
        if cert.key.as_ref().is_some_and(|k| k.bits() < 1024) {
            return Err("Algorithm constraints check failed on keysize limits".into());
        }
        if prev_key.as_ref().is_some_and(|k| k.bits() < 1024) {
            return Err(format!("Algorithm constraints check failed on signature algorithm: {}", alg_name));
        }

        // KeyChecker: a CA certificate (every one but the target) with key
        // usage must allow certificate signing.
        if !last && cert.key_usage.as_ref().is_some_and(|ku| !ku[5]) {
            return Err("CA key usage check failed: keyCertSign bit is not set".into());
        }

        // ConstraintsChecker.
        if !last {
            let path_len = if cert.version < 3 {
                if i == 1 && cert.self_issued() { i32::MAX as i64 } else { -1 }
            } else {
                cert.basic_constraints
            };
            if path_len == -1 {
                return Err("basic constraints check failed: this is not a CA certificate".into());
            }
            if !cert.self_issued() {
                if max_path_len <= 0 {
                    return Err("basic constraints check failed: pathLenConstraint violated - this cert must be the last cert in the certification path".into());
                }
                max_path_len -= 1;
            }
            max_path_len = max_path_len.min(path_len);
        }
        if !name_constraints.is_empty() && (last || !cert.self_issued()) {
            for nc in &name_constraints {
                if !name_constraints_allow(nc, cert)? {
                    return Err("name constraints check failed".into());
                }
            }
        }
        if let Some(nc) = &cert.name_constraints {
            name_constraints.push(nc);
        }

        // PolicyChecker.
        policy.check(cert, i, last)?;

        // BasicChecker.
        cert.validity(now_ms).map_err(|_| "validity check failed")?;
        if cert.issuer.is_empty() {
            return Err("subject/issuer name chaining check failed: empty/null issuer DN in certificate is invalid".into());
        }
        if cert.issuer != prev_subject {
            return Err("subject/issuer name chaining check failed".into());
        }
        cert.verify_by(prev_key.as_ref()).map_err(|_| "signature check failed")?;
        prev_key = cert.key.clone();
        prev_subject = cert.subject.clone();

        unresolved.retain(|oid| !PATH_EXTENSIONS.contains(oid));
        if !unresolved.is_empty() {
            return Err("unrecognized critical extension(s)".into());
        }
    }
    Ok(())
}

// ── Policies (`PolicyChecker`, default parameters) ──

/// A node of the valid policy tree (`PolicyNodeImpl`).
#[derive(Debug)]
struct PolicyNode {
    parent: Option<usize>,
    depth: usize,
    valid: Oid,
    expected: Vec<Oid>,
    /// The expected set is the original one: a mapping replaces it first.
    original: bool,
    children: Vec<usize>,
}

/// The valid policy tree: node 0 is the root.
#[derive(Debug)]
struct PolicyTree {
    nodes: Vec<PolicyNode>,
}

impl PolicyTree {
    fn new() -> Self {
        let root = PolicyNode { parent: None, depth: 0, valid: ANY_POLICY, expected: vec![ANY_POLICY], original: true, children: Vec::new() };
        Self { nodes: vec![root] }
    }

    fn add(&mut self, parent: usize, valid: Oid, expected: Oid, by_mapping: bool) {
        let id = self.nodes.len();
        let depth = self.nodes[parent].depth + 1;
        self.nodes.push(PolicyNode { parent: Some(parent), depth, valid, expected: vec![expected], original: !by_mapping, children: Vec::new() });
        self.nodes[parent].children.push(id);
    }

    fn at_depth(&self, depth: usize) -> Vec<usize> {
        let mut out = Vec::new();
        let mut stack = vec![0];
        while let Some(id) = stack.pop() {
            if self.nodes[id].depth == depth {
                out.push(id);
            } else if self.nodes[id].depth < depth {
                stack.extend(self.nodes[id].children.iter().copied());
            }
        }
        out
    }

    /// `PolicyNodeImpl.prune(depth)`: a childless node above `depth` goes.
    fn prune(&mut self, id: usize, depth: usize) {
        for child in self.nodes[id].children.clone() {
            self.prune(child, depth);
            if self.nodes[child].children.is_empty() && depth > self.nodes[id].depth + 1 {
                self.nodes[id].children.retain(|c| *c != child);
            }
        }
    }

    fn is_empty(&self) -> bool {
        self.nodes[0].children.is_empty()
    }

    /// `processParents`: children for `policy` under the nodes of depth
    /// `i - 1` that expect it (with `match_any`: under the anyPolicy nodes;
    /// for anyPolicy: under every node, one per expected policy not yet a
    /// child).
    fn process_parents(&mut self, i: usize, policy: Oid, match_any: bool) -> bool {
        let parents: Vec<usize> = self
            .at_depth(i - 1)
            .into_iter()
            .filter(|p| {
                let node = &self.nodes[*p];
                policy == ANY_POLICY || if match_any { node.valid == ANY_POLICY } else { node.expected.contains(&policy) }
            })
            .collect();
        for p in &parents {
            if policy == ANY_POLICY {
                for expected in self.nodes[*p].expected.clone() {
                    if !self.nodes[*p].children.iter().any(|c| self.nodes[*c].valid == expected) {
                        self.add(*p, expected, expected, false);
                    }
                }
            } else {
                self.add(*p, policy, policy, false);
            }
        }
        !parents.is_empty()
    }
}

/// The policy state of the path (`PolicyChecker`): user initial policy set
/// anyPolicy, no explicit policy required, mapping and anyPolicy allowed,
/// qualifiers of critical policies rejected (the `PKIXParameters`
/// defaults).
struct PolicyState {
    tree: Option<PolicyTree>,
    explicit: i64,
    mapping: i64,
    inhibit_any: i64,
}

impl PolicyState {
    fn new(n: usize) -> Self {
        let start = n as i64 + 1;
        Self { tree: Some(PolicyTree::new()), explicit: start, mapping: start, inhibit_any: start }
    }

    fn check(&mut self, cert: &Certificate, i: usize, last: bool) -> Result<(), String> {
        const QUALIFIERS: &str = "critical policy qualifiers present in certificate";
        let mut tree = self.tree.take();
        match (&cert.policies, tree.as_mut()) {
            (Some((critical, items)), Some(t)) => {
                let mut any_qualifiers = None;
                for (policy, qualifiers) in items {
                    if *policy == ANY_POLICY {
                        any_qualifiers = Some(*qualifiers);
                    } else {
                        if *qualifiers && *critical {
                            return Err(QUALIFIERS.into());
                        }
                        if !t.process_parents(i, *policy, false) {
                            t.process_parents(i, *policy, true);
                        }
                    }
                }
                if let Some(qualifiers) = any_qualifiers
                    && (self.inhibit_any > 0 || (!last && cert.self_issued()))
                {
                    if qualifiers && *critical {
                        return Err(QUALIFIERS.into());
                    }
                    t.process_parents(i, ANY_POLICY, true);
                }
                t.prune(0, i);
                if t.is_empty() {
                    tree = None;
                }
            }
            (None, _) => tree = None,
            (Some(_), None) => {}
        }
        if !last && let Some(t) = tree.take() {
            tree = self.process_mappings(t, cert, i)?;
        }
        let explicit = if last { merge_explicit(self.explicit, cert, true) } else { self.explicit };
        if explicit == 0 && tree.is_none() {
            return Err("non-null policy tree required and policy tree is null".into());
        }
        self.tree = tree;
        if !last {
            self.explicit = merge_explicit(self.explicit, cert, false);
            self.mapping = decrement(self.mapping, cert);
            if let Some((_, inhibit)) = cert.policy_constraints
                && inhibit != -1
                && (self.mapping == -1 || inhibit < self.mapping)
            {
                self.mapping = inhibit;
            }
            self.inhibit_any = decrement(self.inhibit_any, cert);
            if let Some(skip) = cert.inhibit_any_policy
                && skip != -1
                && skip < self.inhibit_any
            {
                self.inhibit_any = skip;
            }
        }
        Ok(())
    }

    /// `processPolicyMappings`.
    fn process_mappings(&self, mut t: PolicyTree, cert: &Certificate, i: usize) -> Result<Option<PolicyTree>, String> {
        let Some(maps) = &cert.policy_mappings else { return Ok(Some(t)) };
        let mut deleted = false;
        for (issuer, subject) in maps {
            if *issuer == ANY_POLICY {
                return Err("encountered an issuerDomainPolicy of ANY_POLICY".into());
            }
            if *subject == ANY_POLICY {
                return Err("encountered a subjectDomainPolicy of ANY_POLICY".into());
            }
            let nodes = t.at_depth(i);
            let valid: Vec<usize> = nodes.iter().copied().filter(|n| t.nodes[*n].valid == *issuer).collect();
            if !valid.is_empty() {
                for id in valid {
                    if self.mapping > 0 || self.mapping == -1 {
                        let node = &mut t.nodes[id];
                        if node.original {
                            node.expected.clear();
                            node.original = false;
                        }
                        node.expected.push(*subject);
                    } else if self.mapping == 0 {
                        if let Some(parent) = t.nodes[id].parent {
                            t.nodes[parent].children.retain(|c| *c != id);
                        }
                        deleted = true;
                    }
                }
            } else if self.mapping > 0 || self.mapping == -1 {
                let any_parents: Vec<usize> = nodes.iter().filter(|n| t.nodes[**n].valid == ANY_POLICY).filter_map(|n| t.nodes[*n].parent).collect();
                for parent in any_parents {
                    t.add(parent, *issuer, *subject, true);
                }
            }
        }
        if deleted {
            t.prune(0, i);
            if t.is_empty() {
                return Ok(None);
            }
        }
        Ok(Some(t))
    }
}

/// A policy counter after a certificate that is not self-issued.
fn decrement(counter: i64, cert: &Certificate) -> i64 {
    if counter > 0 && !cert.self_issued() { counter - 1 } else { counter }
}

/// `mergeExplicitPolicy`.
fn merge_explicit(explicit: i64, cert: &Certificate, last: bool) -> i64 {
    let explicit = decrement(explicit, cert);
    match cert.policy_constraints {
        Some((require, _)) if !last && require != -1 && (explicit == -1 || require < explicit) => require,
        Some((0, _)) if last => 0,
        _ => explicit,
    }
}

// ── Name constraints (`NameConstraintsExtension.verify`) ──

/// How a constraint name relates to a certificate name
/// (`GeneralNameInterface.constrains`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Relation {
    DiffType,
    Match,
    Narrows,
    Widens,
    SameType,
}

/// A certificate name checked against name constraints.
enum CertName {
    Directory(DName),
    Dns(String),
    Email(String),
    Ip(Vec<u8>),
    /// A name form the checks here do not compare: its tag.
    Other(u8),
}

fn general_name_tag(name: &GeneralName) -> u8 {
    match name {
        GeneralName::OtherName(_) => 0,
        GeneralName::Rfc822Name(_) => 1,
        GeneralName::DnsName(_) => 2,
        GeneralName::DirectoryName(_) => 4,
        GeneralName::EdiPartyName(_) => 5,
        GeneralName::UniformResourceIdentifier(_) => 6,
        GeneralName::IpAddress(_) => 7,
        GeneralName::RegisteredId(_) => 8,
    }
}

impl CertName {
    fn of(name: &GeneralName) -> Self {
        match name {
            GeneralName::DirectoryName(n) => Self::Directory(DName::new(n)),
            GeneralName::DnsName(s) => Self::Dns(s.to_string()),
            GeneralName::Rfc822Name(s) => Self::Email(s.to_string()),
            GeneralName::IpAddress(a) => Self::Ip(a.as_bytes().to_vec()),
            other => Self::Other(general_name_tag(other)),
        }
    }

    fn tag(&self) -> u8 {
        match self {
            Self::Email(_) => 1,
            Self::Dns(_) => 2,
            Self::Directory(_) => 4,
            Self::Ip(_) => 7,
            Self::Other(t) => *t,
        }
    }
}

/// `constraint.constrains(name)` for the name forms compared here; another
/// form of the same type is an error (not compared).
fn constrains(constraint: &GeneralName, name: &CertName) -> Result<Relation, String> {
    if general_name_tag(constraint) != name.tag() {
        return Ok(Relation::DiffType);
    }
    Ok(match (constraint, name) {
        (GeneralName::DirectoryName(c), CertName::Directory(n)) => {
            let c = DName::new(c);
            if *n == c {
                Relation::Match
            } else if n.canonical.is_empty() {
                Relation::Widens
            } else if c.canonical.is_empty() || n.is_within(&c) {
                Relation::Narrows
            } else if c.is_within(n) {
                Relation::Widens
            } else {
                Relation::SameType
            }
        }
        (GeneralName::DnsName(c), CertName::Dns(n)) => suffix_relation(c.as_str(), n, '.'),
        (GeneralName::Rfc822Name(c), CertName::Email(n)) => email_relation(c.as_str(), n),
        (GeneralName::IpAddress(c), CertName::Ip(n)) => ip_relation(c.as_bytes(), n),
        _ => return Err(format!("name form {} not compared", name.tag())),
    })
}

/// `DNSName.constrains`: `this` the constraint, `input` the name.
fn suffix_relation(this: &str, input: &str, sep: char) -> Relation {
    let (this, input) = (this.to_lowercase(), input.to_lowercase());
    if input == this {
        Relation::Match
    } else if let Some(head) = this.strip_suffix(input.as_str()) {
        if head.ends_with(sep) { Relation::Widens } else { Relation::SameType }
    } else if let Some(head) = input.strip_suffix(this.as_str()) {
        if head.ends_with(sep) { Relation::Narrows } else { Relation::SameType }
    } else {
        Relation::SameType
    }
}

/// `RFC822Name.constrains`.
fn email_relation(this: &str, input: &str) -> Relation {
    let (this, input) = (this.to_lowercase(), input.to_lowercase());
    if input == this {
        Relation::Match
    } else if let Some(head) = this.strip_suffix(input.as_str()) {
        if input.contains('@') {
            Relation::SameType
        } else if input.starts_with('.') || head.ends_with('@') {
            Relation::Widens
        } else {
            Relation::SameType
        }
    } else if let Some(head) = input.strip_suffix(this.as_str()) {
        if this.contains('@') {
            Relation::SameType
        } else if this.starts_with('.') || head.ends_with('@') {
            Relation::Narrows
        } else {
            Relation::SameType
        }
    } else {
        Relation::SameType
    }
}

/// `IPAddressName.constrains` for a host or a subnet against a host or a
/// subnet.
fn ip_relation(this: &[u8], other: &[u8]) -> Relation {
    if this == other {
        return Relation::Match;
    }
    let in_subnet = |subnet: &[u8], host: &[u8]| {
        let half = subnet.len() / 2;
        host.len() >= half && (0..half).all(|i| host[i] & subnet[i + half] == subnet[i])
    };
    match (this.len(), other.len()) {
        (4, 4) => Relation::SameType,
        (a, b) if a == b && (a == 8 || a == 32) => {
            let half = a / 2;
            let this_empty = (0..half).any(|i| this[i] & this[i + half] != this[i]);
            let other_empty = (0..half).any(|i| other[i] & other[i + half] != other[i]);
            let other_in_this = (0..half).all(|i| this[i + half] & other[i + half] == this[i + half] && this[i] & this[i + half] == other[i] & this[i + half]);
            let this_in_other = (0..half).all(|i| other[i + half] & this[i + half] == other[i + half] && other[i] & other[i + half] == this[i] & other[i + half]);
            match (this_empty, other_empty) {
                (true, true) => Relation::Match,
                (true, false) => Relation::Widens,
                (false, true) => Relation::Narrows,
                _ if other_in_this => Relation::Narrows,
                _ if this_in_other => Relation::Widens,
                _ => Relation::SameType,
            }
        }
        (_, 8 | 32) => if in_subnet(other, this) { Relation::Widens } else { Relation::SameType },
        (8 | 32, _) => if in_subnet(this, other) { Relation::Narrows } else { Relation::SameType },
        _ => Relation::SameType,
    }
}

/// `DNSName(String)`: labels of letters, digits and hyphens, each starting
/// with a letter or a digit.
fn valid_dns_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('.')
        && !name.ends_with('.')
        && name.split('.').all(|label| {
            label.chars().next().is_some_and(|c| c.is_ascii_alphanumeric()) && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        })
}

/// One name against the constraints (`verify(GeneralNameInterface)`): no
/// excluded subtree matches or contains it, and when a permitted subtree has
/// its type, one of them does.
fn name_allowed(nc: &NameConstraints, name: &CertName) -> Result<bool, String> {
    for subtree in nc.excluded_subtrees.iter().flatten() {
        if matches!(constrains(&subtree.base, name)?, Relation::Match | Relation::Narrows) {
            return Ok(false);
        }
    }
    let mut same_type = false;
    for subtree in nc.permitted_subtrees.iter().flatten() {
        match constrains(&subtree.base, name)? {
            Relation::DiffType => {}
            Relation::Widens | Relation::SameType => same_type = true,
            Relation::Match | Relation::Narrows => return Ok(true),
        }
    }
    Ok(!same_type)
}

/// `NameConstraintsExtension.verify(X509Certificate)`: the subject, the
/// alternative names (without them, the subject's e-mail addresses), and
/// the subject's most specific common name as an address or a host name
/// when no alternative name of that form is there.
fn name_constraints_allow(nc: &NameConstraints, cert: &Certificate) -> Result<bool, String> {
    let subtrees = || nc.permitted_subtrees.iter().flatten().chain(nc.excluded_subtrees.iter().flatten());
    if subtrees().any(|s| s.minimum != 0) {
        return Err("Non-zero minimum BaseDistance in name constraints not supported".into());
    }
    if subtrees().any(|s| s.maximum.is_some()) {
        return Err("Maximum BaseDistance in name constraints not supported".into());
    }
    if !cert.subject.is_empty() && !name_allowed(nc, &CertName::Directory(cert.subject.clone()))? {
        return Ok(false);
    }
    let mut names: Vec<CertName> = match &cert.alt_names {
        Some(alt) => alt.iter().map(CertName::of).collect(),
        None => cert
            .subject
            .name
            .iter()
            .filter(|a| a.oid == OID_EMAIL)
            .filter_map(|a| java_string(&a.value))
            .filter(|e| !e.is_empty() && !e.ends_with('.'))
            .map(CertName::Email)
            .collect(),
    };
    if let Some(cn) = cert.subject.most_specific_cn() {
        match cn.parse::<std::net::IpAddr>() {
            Ok(ip) => {
                if !names.iter().any(|n| matches!(n, CertName::Ip(_))) {
                    names.push(CertName::Ip(match ip {
                        std::net::IpAddr::V4(v4) => v4.octets().to_vec(),
                        std::net::IpAddr::V6(v6) => v6.octets().to_vec(),
                    }));
                }
            }
            Err(_) => {
                if !names.iter().any(|n| matches!(n, CertName::Dns(_))) && valid_dns_name(&cn) {
                    names.push(CertName::Dns(cn));
                }
            }
        }
    }
    for name in &names {
        if !name_allowed(nc, name)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// The certificate cases of `tests/fixtures/key_exchange/certificates.txt`.
#[cfg(test)]
pub(crate) mod fixture {
    use base64::{Engine as _, engine::general_purpose::STANDARD as B64};
    use std::collections::HashMap;

    const FIXTURE: &str = include_str!("../../tests/fixtures/key_exchange/certificates.txt");

    /// 06/10/2026 12:00 UTC: inside every validity the fixture's cases use.
    pub(crate) const NOW_MS: i64 = 1_791_288_000_000;

    pub(crate) struct Case {
        pub(crate) name: String,
        pub(crate) test_mode: bool,
        pub(crate) outcome: String,
        pub(crate) signature: Vec<u8>,
        pub(crate) certs: Vec<Vec<u8>>,
    }

    pub(crate) fn cases() -> Vec<Case> {
        let mut table: HashMap<&str, Vec<u8>> = HashMap::new();
        let mut out = Vec::new();
        for line in FIXTURE.lines() {
            let f: Vec<&str> = line.split(' ').collect();
            match f[0] {
                "sig" | "cert" => {
                    table.insert(f[1], B64.decode(f.get(2).copied().unwrap_or("")).unwrap());
                }
                "case" => out.push(Case {
                    name: f[1].into(),
                    test_mode: f[2] == "test",
                    outcome: f[3].into(),
                    signature: table[f[4]].clone(),
                    certs: f[5..].iter().map(|id| table[id].clone()).collect(),
                }),
                _ => {}
            }
        }
        out
    }

    pub(crate) fn case(name: &str) -> Case {
        cases().into_iter().find(|c| c.name == name).unwrap_or_else(|| panic!("no case {name}"))
    }

    /// The captured key-exchange reply, the text of its frame.
    pub(crate) fn captured_hello() -> &'static str {
        FIXTURE.lines().find_map(|l| l.strip_prefix("hello ")).unwrap()
    }

    /// The fields of the captured reply after the server random and public
    /// value: signature, certificate count, certificates.
    pub(crate) fn captured_hello_certificates() -> Vec<&'static str> {
        captured_hello().split(';').skip(4).filter(|f| !f.is_empty()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::fixture::{NOW_MS as FIXTURE_NOW_MS, case, cases};
    use super::*;

    fn run(signature: &[u8], certs: &[Vec<u8>], now_ms: i64, test_mode: bool) -> Result<(), CertError> {
        let parsed: Vec<Certificate> = certs.iter().map(|c| parse_certificate(c).unwrap()).collect();
        check_server_certificates(signature, &parsed, now_ms, test_mode)
    }

    fn outcome(r: &Result<(), CertError>) -> &'static str {
        match r {
            Ok(()) => "PASS",
            Err(CertError::Failed(_)) => "FAIL",
            Err(CertError::Expired) => "EXPIRED",
            Err(CertError::NotYetValid) => "NOT_YET_VALID",
        }
    }

    // ibx#276: every case of the fixture gives the reference's outcome.
    #[test]
    fn fixture_cases_give_the_reference_outcome() {
        let cases = cases();
        assert!(cases.len() >= 90, "{} cases", cases.len());
        let mut wrong = Vec::new();
        for c in &cases {
            let r = run(&c.signature, &c.certs, FIXTURE_NOW_MS, c.test_mode);
            if outcome(&r) != c.outcome {
                wrong.push(format!("{}: expected {}, got {:?}", c.name, c.outcome, r));
            }
        }
        assert!(wrong.is_empty(), "{}", wrong.join("\n"));
    }

    // ibx#276: the captured chain at the edges of its first certificate's
    // dates (04/10/2026 13:26:08 to 06/10/2026 21:26:08 UTC), to the
    // millisecond as the runtime compares them.
    #[test]
    fn captured_chain_dates() {
        let c = case("captured_chain");
        let not_before = 1_791_120_368_000;
        let not_after = 1_791_321_968_000;
        assert_eq!(run(&c.signature, &c.certs, not_before, false), Ok(()));
        assert_eq!(run(&c.signature, &c.certs, not_after, false), Ok(()));
        assert_eq!(run(&c.signature, &c.certs, not_before - 1, false), Err(CertError::NotYetValid));
        assert_eq!(run(&c.signature, &c.certs, not_after + 1, false), Err(CertError::Expired));
    }

    // ibx#276: the reference's order: a short chain or a bad signature
    // length fails before the dates are read; the dates come before the
    // issuer.
    #[test]
    fn check_order() {
        let c = case("captured_chain");
        let late = 1_791_321_968_001;
        assert!(matches!(run(&c.signature, &c.certs[..2], late, false), Err(CertError::Failed(_))));
        assert!(matches!(run(&c.signature[..255], &c.certs, late, false), Err(CertError::Failed(_))));
        let c = case("leaf_expired_wrong_issuer");
        assert_eq!(run(&c.signature, &c.certs, FIXTURE_NOW_MS, false), Err(CertError::Expired));
    }

    // ibx#276: the signature over nothing is verified and its result
    // dropped: any value of the key's length passes, another length fails.
    #[test]
    fn reply_signature_only_its_length_counts() {
        let c = case("captured_chain");
        for sig in [vec![0u8; 256], vec![0xff; 256], vec![0x5a; 256]] {
            assert_eq!(run(&sig, &c.certs, FIXTURE_NOW_MS, false), Ok(()));
        }
        for len in [0, 128, 255, 257, 512] {
            let r = run(&vec![1u8; len], &c.certs, FIXTURE_NOW_MS, false);
            assert!(matches!(&r, Err(CertError::Failed(m)) if m.contains("Bad signature length")), "{len}: {r:?}");
        }
    }

    // ibx#276: the issuer string as `Principal.getName()` writes it.
    #[test]
    fn issuer_prefix_reads_the_name_from_its_last_rdn() {
        let c = case("captured_chain");
        let first = parse_certificate(&c.certs[0]).unwrap();
        assert!(first.issuer.starts_with_cn(PROD_CN));
        assert!(!first.issuer.starts_with_cn(TEST_CN));
        assert!(!first.subject.starts_with_cn(PROD_CN));
        let second = parse_certificate(&c.certs[1]).unwrap();
        assert!(second.subject.starts_with_cn(PROD_CN));
        assert!(first.issuer == second.subject);
    }

    #[test]
    fn malformed_certificates_do_not_parse() {
        let c = case("captured_chain");
        let der = &c.certs[0];
        assert!(parse_certificate(&[]).is_err());
        assert!(parse_certificate(b"-----BEGIN CERTIFICATE-----").is_err());
        assert!(parse_certificate(&der[..der.len() - 1]).is_err());
        let mut bad = der.clone();
        bad[20] ^= 0xff;
        let _ = parse_certificate(&bad);
        let mut longer = der.clone();
        longer.extend_from_slice(b"after");
        assert!(parse_certificate(&longer).is_ok(), "bytes after the certificate are not read");
    }

    #[test]
    fn canonical_names_as_the_runtime() {
        let ava = |tag: Tag, v: &[u8]| Any::new(tag, v.to_vec()).unwrap();
        let cn = Oid::new_unwrap("2.5.4.3");
        assert_eq!(canonical_ava(&cn, &ava(Tag::PrintableString, b"  Test   Anchor CA ")), "cn=test anchor ca");
        assert_eq!(canonical_ava(&cn, &ava(Tag::Utf8String, b"a,b")), "cn=a\\,b");
        assert_eq!(canonical_ava(&cn, &ava(Tag::Ia5String, b"x")), "cn=#160178");
        assert_eq!(canonical_ava(&Oid::new_unwrap("1.2.3"), &ava(Tag::Utf8String, b"X")), "1.2.3=#0c0158");
        assert_eq!(java_string(&ava(Tag::BmpString, &[0, b'a', 0, b'b'])).as_deref(), Some("ab"));
        assert_eq!(java_string(&ava(Tag::VisibleString, b"ab")), None);
    }
}
