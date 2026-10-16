use std::fmt;

use mint_types::{Amount, BlindNonce, MintOutput};
use secp256k1::PublicKey;

/// Length of the reply field: the payer's AIR DID, a 33-byte value. The
/// mint treats it as opaque bytes and does not check that it is a curve point.
pub const DID_LEN: usize = 33;
const POINT_LEN: usize = 33;
/// A keyset ID on the wire: 8 bytes.
pub const KEYSET_LEN: usize = 8;

#[derive(Debug, PartialEq, Eq)]
pub struct Payload {
    pub did: [u8; DID_LEN],
    pub outputs: Vec<MintOutput>,
    /// The keyset the outputs were blinded for (`GET /keysets`), as 16 hex
    /// characters; 8 bytes after the outputs on the wire. Optional, so older
    /// payloads still decode. The mint signs a claim with its current keyset
    /// and prints that keyset's ID with the signatures.
    pub keyset: Option<String>,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum DecodeError {
    Truncated,
    TrailingBytes,
    WrongReplyLength,
    NoOutputs,
    ZeroAmount,
    InvalidBlindNonce,
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DecodeError::Truncated => write!(f, "payload ends early"),
            DecodeError::TrailingBytes => write!(f, "payload has bytes after the last output"),
            DecodeError::WrongReplyLength => {
                write!(f, "reply is not exactly {DID_LEN} bytes (the payer DID)")
            }
            DecodeError::NoOutputs => write!(f, "payload has no outputs"),
            DecodeError::ZeroAmount => write!(f, "an output has amount zero"),
            DecodeError::InvalidBlindNonce => write!(f, "a blind nonce is not a valid curve point"),
        }
    }
}

impl std::error::Error for DecodeError {}

fn take<'a>(input: &mut &'a [u8], len: usize) -> Result<&'a [u8], DecodeError> {
    if input.len() < len {
        return Err(DecodeError::Truncated);
    }
    let (head, tail) = input.split_at(len);
    *input = tail;
    Ok(head)
}

pub fn decode(bytes: &[u8]) -> Result<Payload, DecodeError> {
    let mut rest = bytes;

    let reply_len = usize::from(take(&mut rest, 1)?[0]);
    if reply_len != DID_LEN {
        return Err(DecodeError::WrongReplyLength);
    }
    let did: [u8; DID_LEN] = take(&mut rest, DID_LEN)?
        .try_into()
        .expect("length checked");

    let count = take(&mut rest, 1)?[0];
    if count == 0 {
        return Err(DecodeError::NoOutputs);
    }

    let mut outputs = Vec::with_capacity(usize::from(count));
    for _ in 0..count {
        let amount_bytes: [u8; 8] = take(&mut rest, 8)?.try_into().expect("length checked");
        let units = u64::from_be_bytes(amount_bytes);
        if units == 0 {
            return Err(DecodeError::ZeroAmount);
        }
        let point = PublicKey::from_slice(take(&mut rest, POINT_LEN)?)
            .map_err(|_| DecodeError::InvalidBlindNonce)?;
        outputs.push(MintOutput {
            amount: Amount::from_units(units),
            blind_nonce: BlindNonce(point),
        });
    }

    let keyset = match rest.len() {
        0 => None,
        KEYSET_LEN => Some(hex::encode(rest)),
        _ => return Err(DecodeError::TrailingBytes),
    };
    Ok(Payload {
        did,
        outputs,
        keyset,
    })
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum EncodeError {
    NoOutputs,
    /// The output count is one byte.
    TooManyOutputs,
    ZeroAmount,
    /// The keyset is not 16 hex characters.
    BadKeyset,
}

impl fmt::Display for EncodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EncodeError::NoOutputs => write!(f, "payload has no outputs"),
            EncodeError::TooManyOutputs => write!(f, "payload has more than 255 outputs"),
            EncodeError::ZeroAmount => write!(f, "an output has amount zero"),
            EncodeError::BadKeyset => write!(f, "the keyset is not 16 hex characters"),
        }
    }
}

impl std::error::Error for EncodeError {}

/// Write a payload in the wire format (the plaintext, before encryption). The
/// wallet uses this, and `decode` reads it back.
pub fn encode(payload: &Payload) -> Result<Vec<u8>, EncodeError> {
    if payload.outputs.is_empty() {
        return Err(EncodeError::NoOutputs);
    }
    let count = u8::try_from(payload.outputs.len()).map_err(|_| EncodeError::TooManyOutputs)?;
    let mut out = Vec::with_capacity(2 + DID_LEN + payload.outputs.len() * (8 + POINT_LEN));
    out.push(DID_LEN as u8);
    out.extend_from_slice(&payload.did);
    out.push(count);
    for output in &payload.outputs {
        if output.amount.units == 0 {
            return Err(EncodeError::ZeroAmount);
        }
        out.extend_from_slice(&output.amount.units.to_be_bytes());
        out.extend_from_slice(&output.blind_nonce.0.serialize());
    }
    if let Some(k) = &payload.keyset {
        match hex::decode(k) {
            Ok(b) if b.len() == KEYSET_LEN => out.extend_from_slice(&b),
            _ => return Err(EncodeError::BadKeyset),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const KAT_PLAINTEXT_HEX: &str = "210279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f8179802000000000000000402f9308a019258c31049344f85f89d5229b531c845836f99b08601f113bce036f90000000000000001022f8bde4d1a07209355b4a7250a5c5128e88b84bddc619ab7cba8d569b240efe4";
    const POINT_3G: &str = "02f9308a019258c31049344f85f89d5229b531c845836f99b08601f113bce036f9";
    const POINT_5G: &str = "022f8bde4d1a07209355b4a7250a5c5128e88b84bddc619ab7cba8d569b240efe4";

    fn point(hex_str: &str) -> PublicKey {
        PublicKey::from_slice(&hex::decode(hex_str).expect("valid hex")).expect("valid point")
    }

    fn output(units: u64, hex_point: &str) -> MintOutput {
        MintOutput {
            amount: Amount::from_units(units),
            blind_nonce: BlindNonce(point(hex_point)),
        }
    }

    const DID_HEX: &str = "0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798";

    fn did(byte: u8) -> [u8; DID_LEN] {
        [byte; DID_LEN]
    }

    #[test]
    fn a_keyset_after_the_outputs_round_trips_and_other_trailing_bytes_do_not() {
        let mut p = sample();
        p.keyset = Some("0123456789abcdef".into());
        let bytes = super::encode(&p).unwrap();
        assert_eq!(decode(&bytes).unwrap(), p);
        assert_eq!(
            decode(&bytes[..bytes.len() - 1]),
            Err(DecodeError::TrailingBytes)
        );
        p.keyset = Some("zz".into());
        assert_eq!(super::encode(&p), Err(EncodeError::BadKeyset));
    }

    fn sample() -> Payload {
        Payload {
            keyset: None,
            did: hex::decode(DID_HEX).unwrap().try_into().unwrap(),
            outputs: vec![output(4, POINT_3G), output(1, POINT_5G)],
        }
    }

    fn encode(payload: &Payload) -> Vec<u8> {
        let mut out = vec![DID_LEN as u8];
        out.extend_from_slice(&payload.did);
        out.push(payload.outputs.len() as u8);
        for output in &payload.outputs {
            out.extend_from_slice(&output.amount.units.to_be_bytes());
            out.extend_from_slice(&output.blind_nonce.0.serialize());
        }
        out
    }

    #[test]
    fn the_public_encode_reproduces_the_known_answer_and_round_trips() {
        let bytes = super::encode(&sample()).unwrap();
        assert_eq!(hex::encode(&bytes), KAT_PLAINTEXT_HEX);
        assert_eq!(decode(&bytes), Ok(sample()));
    }

    #[test]
    fn the_public_encode_refuses_what_decode_would_refuse() {
        let none = Payload {
            keyset: None,
            did: did(7),
            outputs: vec![],
        };
        assert_eq!(super::encode(&none), Err(EncodeError::NoOutputs));
        let zero = Payload {
            keyset: None,
            did: did(7),
            outputs: vec![output(0, POINT_3G)],
        };
        assert_eq!(super::encode(&zero), Err(EncodeError::ZeroAmount));
        let many = Payload {
            keyset: None,
            did: did(7),
            outputs: (0..256).map(|_| output(1, POINT_3G)).collect(),
        };
        assert_eq!(super::encode(&many), Err(EncodeError::TooManyOutputs));
        let max = Payload {
            keyset: None,
            did: did(7),
            outputs: (0..255).map(|_| output(1, POINT_3G)).collect(),
        };
        assert_eq!(super::encode(&max).unwrap().len(), 35 + 41 * 255);
    }

    #[test]
    fn decodes_known_answer_plaintext() {
        let bytes = hex::decode(KAT_PLAINTEXT_HEX).expect("valid hex");
        assert_eq!(decode(&bytes), Ok(sample()));
    }

    #[test]
    fn test_encode_reproduces_known_answer_plaintext() {
        assert_eq!(hex::encode(encode(&sample())), KAT_PLAINTEXT_HEX);
    }

    #[test]
    fn the_did_is_carried_through_unchecked() {
        let payload = Payload {
            keyset: None,
            did: did(7),
            outputs: vec![output(1, POINT_3G)],
        };
        assert_eq!(decode(&encode(&payload)), Ok(payload));
    }

    #[test]
    fn any_truncation_is_rejected() {
        let bytes = encode(&sample());
        for len in 0..bytes.len() {
            assert!(decode(&bytes[..len]).is_err(), "length {len} was accepted");
        }
    }

    #[test]
    fn rejects_trailing_bytes() {
        let mut bytes = encode(&sample());
        bytes.push(0);
        assert_eq!(decode(&bytes), Err(DecodeError::TrailingBytes));
    }

    #[test]
    fn rejects_zero_amount() {
        let payload = Payload {
            keyset: None,
            did: did(7),
            outputs: vec![output(0, POINT_3G)],
        };
        assert_eq!(decode(&encode(&payload)), Err(DecodeError::ZeroAmount));
    }

    #[test]
    fn rejects_no_outputs() {
        let mut bytes = vec![DID_LEN as u8];
        bytes.extend_from_slice(&did(7));
        bytes.push(0);
        assert_eq!(decode(&bytes), Err(DecodeError::NoOutputs));
    }

    #[test]
    fn rejects_any_reply_length_other_than_a_did() {
        for len in [0usize, 10, DID_LEN - 1, DID_LEN + 1, 64] {
            let mut bytes = vec![len as u8];
            bytes.extend_from_slice(&vec![7u8; len]);
            bytes.push(1);
            bytes.extend_from_slice(&1u64.to_be_bytes());
            bytes.extend_from_slice(&hex::decode(POINT_3G).unwrap());
            assert_eq!(
                decode(&bytes),
                Err(DecodeError::WrongReplyLength),
                "reply length {len}"
            );
        }
    }

    #[test]
    fn rejects_invalid_blind_nonce() {
        let mut bytes = vec![DID_LEN as u8];
        bytes.extend_from_slice(&did(7));
        bytes.push(1);
        bytes.extend_from_slice(&1u64.to_be_bytes());
        bytes.extend_from_slice(&[0u8; POINT_LEN]);
        assert_eq!(decode(&bytes), Err(DecodeError::InvalidBlindNonce));
    }
}
