//! The OP_RETURN payload is encrypted to the mint's DID public key.
//!
//! The cipher is AIR's, not a new scheme: `air::names::secp256k1`, where
//! `PublicKey::encrypt` seals bytes to a public key and `SecretKey::decrypt`
//! opens them with the matching secret key. It does an ElligatorSwift key
//! exchange and encrypts with ChaCha20-Poly1305, and a fresh sender key is made
//! for every message.
//!
//! The wallet builds the payload as before (see `payload`), seals it with
//! [`encrypt`], and puts the result in the OP_RETURN. The mint's scanner tries
//! [`decrypt`] on each OP_RETURN. What does not open is not for this mint, so
//! [`open_payloads`] drops it without an error and the rest of the PSP never
//! sees it.
//!
//! On the wire the sealed payload is the `postcard` encoding of AIR's
//! `Encrypted`, because its fields are private and serde is the only way in or
//! out. For a payload shorter than 128 bytes that is
//! `0x40 || 64 byte sender key || 16 byte tag || length byte || ciphertext`, so
//! 82 bytes longer than the payload. The ciphertext is as long as the payload.
//!
//! The mint's DID key pair is a local file for now (`mint_did_key.json`, `0600`
//! permissions, in the data directory). Moving it into the AIR keystore is
//! deferred.

use std::error::Error;
use std::fmt;
use std::fs;
use std::io::Write;
use std::path::Path;

use air::names::secp256k1::{Encrypted, PublicKey, SecretKey};

use crate::scan::Received;

pub const KEY_FILENAME: &str = "mint_did_key.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SealError {
    /// The bytes are not a sealed payload at all: too short, the wrong framing,
    /// or bytes left over.
    Malformed,
    /// The framing is right but it does not open with this key. It was sealed
    /// to another key, or it was altered.
    Undecryptable,
    /// The payload could not be written out. Not expected.
    Encoding,
}

impl fmt::Display for SealError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SealError::Malformed => write!(f, "not a sealed payload"),
            SealError::Undecryptable => write!(f, "not sealed to this key, or altered"),
            SealError::Encoding => write!(f, "could not encode the sealed payload"),
        }
    }
}

impl Error for SealError {}

/// Seal `plaintext` to the mint's DID public key.
pub fn encrypt(mint_key: &PublicKey, plaintext: &[u8]) -> Result<Vec<u8>, SealError> {
    let sealed = mint_key.encrypt(plaintext.to_vec());
    postcard::to_allocvec(&sealed).map_err(|_| SealError::Encoding)
}

/// Open a sealed payload with the mint's DID secret key.
pub fn decrypt(secret: &SecretKey, sealed: &[u8]) -> Result<Vec<u8>, SealError> {
    let (parsed, rest) =
        postcard::take_from_bytes::<Encrypted>(sealed).map_err(|_| SealError::Malformed)?;
    if !rest.is_empty() {
        return Err(SealError::Malformed);
    }
    secret.decrypt(parsed).map_err(|_| SealError::Undecryptable)
}

/// Open the payload of every payment that has one.
///
/// - A payment whose data opens gets the plaintext as its payload.
/// - A payment whose data does not open is not for this mint. It is dropped, not
///   rejected: no record is made, no price is fetched and nothing is reported as
///   an error. Cleartext payloads from before encryption are dropped the same way.
/// - A payment with no payload (no data output, or more than one) is kept as it
///   is, so that it is rejected as before.
///
/// Returns the payments to carry on with and how many were dropped.
pub fn open_payloads(secret: &SecretKey, found: Vec<Received>) -> (Vec<Received>, usize) {
    let mut kept = Vec::with_capacity(found.len());
    let mut dropped = 0;
    for mut received in found {
        match received.payload.take() {
            None => kept.push(received),
            Some(sealed) => match decrypt(secret, &sealed) {
                Ok(plaintext) => {
                    received.payload = Some(plaintext);
                    kept.push(received);
                }
                Err(_) => dropped += 1,
            },
        }
    }
    (kept, dropped)
}

/// Read the mint's DID key from `path`, or make one if there is no file. A file
/// that is there but damaged is an error, never a reason to make a new key,
/// because every payment sealed to the old key would be lost.
pub fn load_or_generate(path: &Path) -> Result<SecretKey, Box<dyn Error>> {
    if path.exists() {
        let text = fs::read_to_string(path)?;
        return serde_json::from_str::<SecretKey>(text.trim()).map_err(|e| {
            Box::<dyn Error>::from(format!("{}: not a DID key file: {e}", path.display()))
        });
    }
    let secret = SecretKey::new();
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(serde_json::to_string(&secret)?.as_bytes())?;
    file.write_all(b"\n")?;
    Ok(secret)
}

/// The key a wallet seals to: 33 bytes, compressed, as hex.
pub fn public_key_hex(secret: &SecretKey) -> String {
    secret.public_key().to_string()
}

/// Parse a published DID public key (hex).
pub fn parse_public_key(text: &str) -> Result<PublicKey, Box<dyn Error>> {
    text.trim()
        .parse::<PublicKey>()
        .map_err(|e| Box::<dyn Error>::from(format!("not a public key: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::payload;
    use std::path::PathBuf;

    // DID 0279be66..1798, then outputs 4 -> 3G and 1 -> 5G (5 units in all)
    const PAYLOAD_HEX: &str = "210279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f8179802000000000000000402f9308a019258c31049344f85f89d5229b531c845836f99b08601f113bce036f90000000000000001022f8bde4d1a07209355b4a7250a5c5128e88b84bddc619ab7cba8d569b240efe4";

    fn plaintext() -> Vec<u8> {
        hex::decode(PAYLOAD_HEX).unwrap()
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("psp-mint-seal-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn payment(txid: &str, payload: Option<Vec<u8>>) -> Received {
        Received {
            txid: txid.to_string(),
            confirmations: 1,
            value_sats: 5_000,
            payload,
        }
    }

    #[test]
    fn a_payload_sealed_to_the_mint_opens_with_its_key() {
        let mint = SecretKey::new();
        let sealed = encrypt(&mint.public_key(), &plaintext()).unwrap();
        assert_eq!(decrypt(&mint, &sealed).unwrap(), plaintext());
        // and the opened bytes are still a valid payload
        assert!(payload::decode(&decrypt(&mint, &sealed).unwrap()).is_ok());
    }

    #[test]
    fn the_sealed_payload_is_82_bytes_longer_and_hides_the_did() {
        let mint = SecretKey::new();
        let sealed = encrypt(&mint.public_key(), &plaintext()).unwrap();
        assert_eq!(plaintext().len(), 117);
        assert_eq!(sealed.len(), 117 + 82);
        // the payer's DID bytes do not appear on chain
        let did = &plaintext()[1..34];
        assert!(!sealed.windows(did.len()).any(|w| w == did));
        // a payload of 128 bytes or more takes a two byte length
        let longer = encrypt(&mint.public_key(), &vec![7u8; 158]).unwrap();
        assert_eq!(longer.len(), 158 + 83);
    }

    #[test]
    fn sealing_twice_gives_different_bytes() {
        let mint = SecretKey::new();
        let a = encrypt(&mint.public_key(), &plaintext()).unwrap();
        let b = encrypt(&mint.public_key(), &plaintext()).unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn a_payload_sealed_to_another_key_does_not_open() {
        let mint = SecretKey::new();
        let other = SecretKey::new();
        let sealed = encrypt(&other.public_key(), &plaintext()).unwrap();
        assert_eq!(decrypt(&mint, &sealed), Err(SealError::Undecryptable));
    }

    #[test]
    fn a_changed_byte_anywhere_is_rejected() {
        let mint = SecretKey::new();
        let sealed = encrypt(&mint.public_key(), &plaintext()).unwrap();
        for i in 0..sealed.len() {
            let mut bad = sealed.clone();
            bad[i] ^= 0x01;
            assert!(decrypt(&mint, &bad).is_err(), "byte {i} changed but it opened");
        }
    }

    #[test]
    fn truncated_extended_and_junk_input_is_rejected() {
        let mint = SecretKey::new();
        let sealed = encrypt(&mint.public_key(), &plaintext()).unwrap();
        assert!(decrypt(&mint, &[]).is_err());
        assert!(decrypt(&mint, &sealed[..sealed.len() - 1]).is_err());
        assert!(decrypt(&mint, &sealed[..40]).is_err());
        let mut extended = sealed.clone();
        extended.push(0);
        assert_eq!(decrypt(&mint, &extended), Err(SealError::Malformed));
        assert!(decrypt(&mint, &[0xff; 300]).is_err());
        // the old cleartext payload is not a sealed payload
        assert!(decrypt(&mint, &plaintext()).is_err());
    }

    #[test]
    fn open_payloads_keeps_what_opens_and_drops_the_rest_without_error() {
        let mint = SecretKey::new();
        let other = SecretKey::new();
        let ours = encrypt(&mint.public_key(), &plaintext()).unwrap();
        let theirs = encrypt(&other.public_key(), &plaintext()).unwrap();
        let mut tampered = ours.clone();
        let last = tampered.len() - 1;
        tampered[last] ^= 0x80;

        let (kept, dropped) = open_payloads(
            &mint,
            vec![
                payment("ours", Some(ours)),
                payment("theirs", Some(theirs)),
                payment("tampered", Some(tampered)),
                payment("cleartext", Some(plaintext())),
                payment("nodata", None),
            ],
        );
        assert_eq!(dropped, 3);
        let ids: Vec<&str> = kept.iter().map(|r| r.txid.as_str()).collect();
        assert_eq!(ids, vec!["ours", "nodata"]);
        assert_eq!(kept[0].payload.as_deref(), Some(plaintext().as_slice()));
        assert_eq!(kept[1].payload, None);
    }

    #[test]
    fn the_key_file_is_made_once_reloaded_and_private() {
        let dir = scratch("keyfile");
        let path = dir.join(KEY_FILENAME);
        let first = load_or_generate(&path).unwrap();
        let again = load_or_generate(&path).unwrap();
        assert_eq!(first.public_key(), again.public_key());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
        // a reloaded key opens what was sealed to the first
        let sealed = encrypt(&first.public_key(), b"hello").unwrap();
        assert_eq!(decrypt(&again, &sealed).unwrap(), b"hello");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_damaged_key_file_is_an_error_and_is_left_alone() {
        let dir = scratch("damaged");
        let path = dir.join(KEY_FILENAME);
        fs::write(&path, "not a key").unwrap();
        assert!(load_or_generate(&path).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "not a key");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_published_key_is_33_bytes_of_hex_and_parses_back() {
        let mint = SecretKey::new();
        let text = public_key_hex(&mint);
        assert_eq!(text.len(), 66);
        assert!(text.starts_with("02") || text.starts_with("03"));
        let parsed = parse_public_key(&text).unwrap();
        assert_eq!(parsed, mint.public_key());
        assert!(parse_public_key("zz").is_err());
    }
}
