use std::fmt;

use mint_types::{Amount, BlindNonce, MintOutput};
use secp256k1::PublicKey;

pub const MAX_REPLY_LEN: usize = 64;
const POINT_LEN: usize = 33;

#[derive(Debug, PartialEq, Eq)]
pub struct Payload {
    pub reply: Vec<u8>,
    pub outputs: Vec<MintOutput>,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum DecodeError {
    Truncated,
    TrailingBytes,
    ReplyTooLong,
    NoOutputs,
    ZeroAmount,
    InvalidBlindNonce,
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DecodeError::Truncated => write!(f, "payload ends early"),
            DecodeError::TrailingBytes => write!(f, "payload has bytes after the last output"),
            DecodeError::ReplyTooLong => write!(f, "reply is longer than {MAX_REPLY_LEN} bytes"),
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
    if reply_len > MAX_REPLY_LEN {
        return Err(DecodeError::ReplyTooLong);
    }
    let reply = take(&mut rest, reply_len)?.to_vec();

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
    Ok(Payload { reply, outputs })
}

#[cfg(test)]
mod tests {
    use super::*;

    const KAT_PLAINTEXT_HEX: &str = "0a7265706c792d7465737402000000000000000402f9308a019258c31049344f85f89d5229b531c845836f99b08601f113bce036f90000000000000001022f8bde4d1a07209355b4a7250a5c5128e88b84bddc619ab7cba8d569b240efe4";
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

    fn sample() -> Payload {
        Payload {
            reply: b"reply-test".to_vec(),
            outputs: vec![output(4, POINT_3G), output(1, POINT_5G)],
        }
    }

    fn encode(payload: &Payload) -> Vec<u8> {
        let mut out = vec![payload.reply.len() as u8];
        out.extend_from_slice(&payload.reply);
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
    fn accepts_empty_and_maximum_replies() {
        for len in [0usize, MAX_REPLY_LEN] {
            let payload = Payload {
                reply: vec![7u8; len],
                outputs: vec![output(1, POINT_3G)],
            };
            assert_eq!(decode(&encode(&payload)), Ok(payload));
        }
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
            reply: Vec::new(),
            outputs: vec![output(0, POINT_3G)],
        };
        assert_eq!(decode(&encode(&payload)), Err(DecodeError::ZeroAmount));
    }

    #[test]
    fn rejects_no_outputs() {
        assert_eq!(decode(&[0, 0]), Err(DecodeError::NoOutputs));
    }

    #[test]
    fn rejects_overlong_reply() {
        let mut bytes = vec![(MAX_REPLY_LEN + 1) as u8];
        bytes.extend_from_slice(&[0u8; 100]);
        assert_eq!(decode(&bytes), Err(DecodeError::ReplyTooLong));
    }

    #[test]
    fn rejects_invalid_blind_nonce() {
        let mut bytes = vec![0u8, 1];
        bytes.extend_from_slice(&1u64.to_be_bytes());
        bytes.extend_from_slice(&[0u8; POINT_LEN]);
        assert_eq!(decode(&bytes), Err(DecodeError::InvalidBlindNonce));
    }
}
