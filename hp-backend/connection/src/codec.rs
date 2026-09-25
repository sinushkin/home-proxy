use prost::Message as _;
use uuid::Uuid;

use crate::proto::PeerMessage;
use crate::xor::xor_in_place;
pub use crate::xor::{KEY_LEN, XorKey};

/// Новый случайный вектор. Берём v4 UUID: 122 бита из CSPRNG ОС — для
/// маскировки хватает, и лишней зависимости не нужно.
pub fn random_key() -> XorKey {
    Uuid::new_v4().into_bytes()
}

/// Временный симметричный «шифр» для UDP-канала между пирами — XOR
/// повторяющимся 16-байтным вектором. Достаточно, чтобы на проводе не было
/// чистого protobuf и у каждой дыры была своя «подпись»; потом заменить на
/// настоящий. К `Rendezvous` не применяется: он ходит по MQTT (и внутри `Lite`
/// по дыре, где кодируется как всё остальное).
pub fn encode(message: &PeerMessage, key: &XorKey) -> Vec<u8> {
    let mut buf = message.encode_to_vec();
    xor_in_place(&mut buf, key);
    buf
}

pub fn decode(mut data: Vec<u8>, key: &XorKey) -> Result<PeerMessage, prost::DecodeError> {
    xor_in_place(&mut data, key);
    PeerMessage::decode(data.as_slice())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::{init_message, peer_message, InitMessage, Punch};

    fn sample() -> PeerMessage {
        PeerMessage {
            body: Some(peer_message::Body::Init(InitMessage {
                session_id: "session-guid".to_string(),
                from_peer_id: "from-guid".to_string(),
                to_peer_id: "to-guid".to_string(),
                slot: 0,
                payload: Some(init_message::Payload::Punch(Punch { target_port: 12_345 })),
            })),
        }
    }

    #[test]
    fn round_trip() {
        let key = random_key();
        let message = sample();

        let decoded = decode(encode(&message, &key), &key).unwrap();

        assert_eq!(message, decoded);
    }

    #[test]
    fn every_byte_is_xored_with_the_cycled_key() {
        let key: XorKey = std::array::from_fn(|i| i as u8 + 1);
        let message = sample();
        let plain = message.encode_to_vec();

        let encoded = encode(&message, &key);

        assert!(plain.len() > KEY_LEN, "тест должен пройти вектор больше одного круга");
        for (i, (enc, pln)) in encoded.iter().zip(&plain).enumerate() {
            assert_eq!(enc ^ pln, key[i % KEY_LEN], "байт {i}");
        }
    }

    #[test]
    fn wrong_key_does_not_yield_the_message() {
        let (key, other) = (random_key(), random_key());
        let message = sample();

        let decoded = decode(encode(&message, &key), &other).ok();

        assert_ne!(decoded, Some(message));
    }

    #[test]
    fn random_keys_differ() {
        assert_ne!(random_key(), random_key());
    }
}
