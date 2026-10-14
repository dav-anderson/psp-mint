use std::fmt;

use mint_types::{Amount, BlindNonce, MintOutput};
use secp256k1::PublicKey;

/// Length of the reply field: the payer's JAMJAR DID, a 33-byte value. The
/// mint treats it as opaque bytes and does not check that it is a curve point.
pub const DID_LEN: usize = 33;
const POINT_LEN: usize = 33;

#[derive(Debug, PartialEq, Eq)]
pub struct Payload {
    pub did: [u8; DID_LEN],
    pub outputs: Vec<MintOutput>,
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
    let did: [u8; DID_LEN] = take(&mut rest, DID_LEN)?.try_into().expect("length checked");

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

    if !rest.is_empty() {
        return Err(DecodeError::TrailingBytes);
    }
    Ok(Payload { did, outputs })
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

    fn sample() -> Payload {
        Payload {
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
