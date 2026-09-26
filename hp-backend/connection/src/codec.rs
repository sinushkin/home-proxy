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

/// Сколько первых байт пакета маскируем. Этого хватает, чтобы на проводе не было
/// узнаваемого protobuf (теги, длины, заголовок `Init`/`Lite`, начало нагрузки); дальше
/// в `Data` идёт шифротекст WireGuard, и XOR по нему — лишняя работа.
pub const MASKED_PREFIX: usize = 64;

/// Временный симметричный «шифр» для UDP-канала между пирами — XOR первых
/// `MASKED_PREFIX` байт повторяющимся 16-байтным вектором. Достаточно, чтобы на проводе
/// не было чистого protobuf и у каждой дыры была своя «подпись»; потом заменить на
/// настоящий. К `Rendezvous` не применяется: он ходит по MQTT (и внутри `Lite`
/// по дыре, где кодируется как всё остальное).
pub fn encode(message: &PeerMessage, key: &XorKey) -> Vec<u8> {
    let mut buf = message.encode_to_vec();
    mask(&mut buf, key);
    buf
}

pub fn decode(mut data: Vec<u8>, key: &XorKey) -> Result<PeerMessage, prost::DecodeError> {
    mask(&mut data, key);
    PeerMessage::decode(data.as_slice())
}

/// Разбор уже снятого с маски буфера (без копии).
pub fn decode_unmasked(data: &[u8]) -> Result<PeerMessage, prost::DecodeError> {
    PeerMessage::decode(data)
}

/// Маскирует (или снимает маску: XOR симметричен) первые `MASKED_PREFIX` байт на месте.
pub fn mask(data: &mut [u8], key: &XorKey) {
    let len = data.len().min(MASKED_PREFIX);
    xor_in_place(&mut data[..len], key);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::{init_message, lite, peer_message, Data, InitMessage, Lite, Punch};

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

    fn big_data() -> PeerMessage {
        PeerMessage {
            body: Some(peer_message::Body::Lite(Lite {
                slot: 3,
                payload: Some(lite::Payload::Data(Data { payload: (0..1392).map(|i| i as u8).collect() })),
            })),
        }
    }

    #[test]
    fn only_the_prefix_is_xored_with_the_cycled_key() {
        let key: XorKey = std::array::from_fn(|i| i as u8 + 1);
        let message = big_data();
        let plain = message.encode_to_vec();

        let encoded = encode(&message, &key);

        assert_eq!(encoded.len(), plain.len());
        for (i, (enc, pln)) in encoded.iter().zip(&plain).enumerate() {
            let expected = if i < MASKED_PREFIX { key[i % KEY_LEN] } else { 0 };
            assert_eq!(enc ^ pln, expected, "байт {i}");
        }
    }

    #[test]
    fn short_messages_are_masked_whole() {
        let key: XorKey = std::array::from_fn(|i| i as u8 + 1);
        let message = sample();
        let plain = message.encode_to_vec();
        assert!(plain.len() > KEY_LEN && plain.len() < MASKED_PREFIX, "короче префикса, длиннее вектора");

        let encoded = encode(&message, &key);

        for (i, (enc, pln)) in encoded.iter().zip(&plain).enumerate() {
            assert_eq!(enc ^ pln, key[i % KEY_LEN], "байт {i}");
        }
    }

    #[test]
    fn big_data_round_trip() {
        let key = random_key();
        let message = big_data();
        assert_eq!(decode(encode(&message, &key), &key).unwrap(), message);
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
