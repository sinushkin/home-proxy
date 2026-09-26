use prost::Message as _;
use uuid::Uuid;

use crate::auth::{RecvKeys, SendKeys, AUTH_LEN, SIGNED_PREFIX};
use crate::proto::PeerMessage;
use crate::xor::xor_in_place;
pub use crate::xor::{KEY_LEN, XorKey};

/// Новый случайный вектор. Берём v4 UUID: 122 бита из CSPRNG ОС — для
/// маскировки хватает, и лишней зависимости не нужно.
pub fn random_key() -> XorKey {
    Uuid::new_v4().into_bytes()
}

/// Сколько первых байт пакета маскируем: подпись и те же `SIGNED_PREFIX` (128) байт protobuf,
/// что она покрывает, — заголовок `Init`/`Lite` и заголовки вложенного IP-пакета. Дальше идёт
/// нагрузка (обычно TLS), XOR по ней — лишняя работа.
pub const MASKED_PREFIX: usize = AUTH_LEN + SIGNED_PREFIX;

/// Пакет на проводе: подпись (`auth`: метка и счётчик) ‖ protobuf, первые `MASKED_PREFIX` байт
/// (подпись и начало protobuf) замаскированы XOR 16-байтным вектором — чтобы на проводе не было узнаваемого protobuf и у
/// каждой дыры был свой вид. Маскировка — не защита; подлинность даёт подпись. К `Rendezvous`
/// в MQTT не применяется (там своя подпись, см. `rendezvous`).
pub fn encode(message: &PeerMessage, keys: &SendKeys) -> Vec<u8> {
    let mut buf = keys.sealer.seal_vec(message.encode_to_vec());
    mask(&mut buf, &keys.xor);
    buf
}

/// Снимает маску, проверяет подпись и разбирает; `None` — не наш или подделанный пакет.
pub fn decode(mut data: Vec<u8>, keys: &mut RecvKeys) -> Option<PeerMessage> {
    mask(&mut data, &keys.xor);
    keys.opener.open(&data)?;
    PeerMessage::decode(&data[AUTH_LEN..]).ok()
}

/// Разбор уже снятого с маски и проверенного буфера (без копии).
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
    use crate::auth::{Opener, RecvKeys, SendKeys, Sealer};
    use crate::proto::{init_message, lite, peer_message, Data, InitMessage, Lite, Punch};

    fn sample() -> PeerMessage {
        PeerMessage {
            body: Some(peer_message::Body::Init(InitMessage {
                session_id: "session-guid".to_string(),
                from_peer_id: "from".to_string(),
                to_peer_id: "to".to_string(),
                slot: 0,
                payload: Some(init_message::Payload::Punch(Punch { target_port: 12_345 })),
            })),
        }
    }

    fn keys(xor: XorKey) -> (SendKeys, RecvKeys) {
        let auth = [5u8; 32];
        (SendKeys { xor, sealer: Sealer::new(&auth) }, RecvKeys { xor, opener: Opener::new(&auth) })
    }

    #[test]
    fn round_trip() {
        let (send, mut recv) = keys(random_key());
        let message = sample();
        assert_eq!(decode(encode(&message, &send), &mut recv), Some(message));
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
    fn the_signature_leads_and_only_the_prefix_is_xored_with_the_cycled_key() {
        let key: XorKey = std::array::from_fn(|i| i as u8 + 1);
        let (send, _) = keys(key);
        let message = big_data();
        let plain = message.encode_to_vec();

        let encoded = encode(&message, &send);

        assert_eq!(encoded.len(), AUTH_LEN + plain.len());
        for (i, (enc, pln)) in encoded[AUTH_LEN..].iter().zip(&plain).enumerate() {
            let at = AUTH_LEN + i;
            let expected = if at < MASKED_PREFIX { key[at % KEY_LEN] } else { 0 };
            assert_eq!(enc ^ pln, expected, "байт {at}");
        }
    }

    #[test]
    fn big_data_round_trip() {
        let (send, mut recv) = keys(random_key());
        let message = big_data();
        assert_eq!(decode(encode(&message, &send), &mut recv), Some(message));
    }

    #[test]
    fn wrong_vector_or_signature_does_not_yield_the_message() {
        let (send, _) = keys(random_key());
        let (_, mut other_vector) = keys(random_key());
        assert_eq!(decode(encode(&sample(), &send), &mut other_vector), None, "чужой вектор");
        let (_, mut recv) = keys(send.xor);
        let foreign = SendKeys { xor: send.xor, sealer: Sealer::new(&[6u8; 32]) };
        assert_eq!(decode(encode(&sample(), &foreign), &mut recv), None, "чужая подпись");
    }

    #[test]
    fn random_keys_differ() {
        assert_ne!(random_key(), random_key());
    }
}
