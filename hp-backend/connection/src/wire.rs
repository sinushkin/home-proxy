//! Быстрый путь кодека для пакетов данных (`Lite` с `Data`, `WrappedData` или `Ordered`): сборка сразу в
//! буфер вызывающего и разбор без выделения памяти. Формат — тот же protobuf, что даёт prost
//! (проверяется тестами байт-в-байт); всё остальное (`Init`, keep-alive, статистика, оферы)
//! по-прежнему кодируется prost'ом. Маскировка — как в `codec` (XOR первых
//! `codec::MASKED_PREFIX` байт, после подписи — `auth`).

use crate::auth::{SendKeys, AUTH_LEN};
use crate::codec;

// Теги protobuf: (номер поля << 3) | тип (0 — varint, 2 — длина + байты).
const PEER_LITE: u8 = 2 << 3 | 2;
const LITE_SLOT: u8 = 1 << 3;
const LITE_DATA: u8 = 3 << 3 | 2;
const LITE_WRAPPED: u8 = 7 << 3 | 2;
const DATA_PAYLOAD: u8 = 1 << 3 | 2;
const WRAPPED_SEQ: u8 = 1 << 3;
const WRAPPED_PAYLOAD: u8 = 2 << 3 | 2;
const WRAPPED_CLIENT: u8 = 3 << 3;
const WRAPPED_FLOW: u8 = 4 << 3;
const LITE_ORDERED: u8 = 8 << 3 | 2;
const ORDERED_FLOW: u8 = 1 << 3;
const ORDERED_SEQ: u8 = 2 << 3;
const ORDERED_PAYLOAD: u8 = 3 << 3 | 2;

/// Разобранный пакет данных (ссылки внутрь буфера приёма).
#[derive(Debug, PartialEq, Eq)]
pub enum Fast<'a> {
    Data { slot: u32, payload: &'a [u8] },
    Wrapped { slot: u32, seq: u64, payload: &'a [u8], client_id: u32, flow: Option<u32> },
    Ordered { slot: u32, flow: u32, seq: u64, payload: &'a [u8] },
}

fn varint_len(mut v: u64) -> usize {
    let mut n = 1;
    while v >= 0x80 {
        v >>= 7;
        n += 1;
    }
    n
}

struct Writer<'a> {
    out: &'a mut [u8],
    pos: usize,
}

impl Writer<'_> {
    fn byte(&mut self, b: u8) -> Option<()> {
        *self.out.get_mut(self.pos)? = b;
        self.pos += 1;
        Some(())
    }

    fn varint(&mut self, mut v: u64) -> Option<()> {
        while v >= 0x80 {
            self.byte(v as u8 | 0x80)?;
            v >>= 7;
        }
        self.byte(v as u8)
    }

    fn bytes(&mut self, data: &[u8]) -> Option<()> {
        self.out.get_mut(self.pos..self.pos + data.len())?.copy_from_slice(data);
        self.pos += data.len();
        Some(())
    }
}

/// Длина поля `bytes` вместе с тегом и длиной (0, если пусто: proto3 его не пишет).
fn bytes_field_len(len: usize) -> usize {
    if len == 0 { 0 } else { 1 + varint_len(len as u64) + len }
}

fn varint_field_len(v: u64) -> usize {
    if v == 0 { 0 } else { 1 + varint_len(v) }
}

/// Пишет `PeerMessage { lite: Lite { slot, <inner_tag>: <inner> } }`, подписывает и маскирует;
/// `inner` дописывает тело вложенного сообщения длины `inner_len`.
fn write_lite(
    out: &mut [u8],
    keys: &SendKeys,
    slot: u32,
    inner_tag: u8,
    inner_len: usize,
    inner: impl FnOnce(&mut Writer) -> Option<()>,
) -> Option<usize> {
    let lite_len = varint_field_len(u64::from(slot)) + 1 + varint_len(inner_len as u64) + inner_len;
    // Место под подпись — в начале; protobuf пишем сразу за ним.
    let mut w = Writer { out, pos: AUTH_LEN };
    w.byte(PEER_LITE)?;
    w.varint(lite_len as u64)?;
    if slot != 0 {
        w.byte(LITE_SLOT)?;
        w.varint(u64::from(slot))?;
    }
    w.byte(inner_tag)?;
    w.varint(inner_len as u64)?;
    inner(&mut w)?;
    let len = w.pos - AUTH_LEN;
    let n = keys.sealer.seal(out, len)?;
    codec::mask(&mut out[..n], &keys.xor);
    Some(n)
}

/// `Lite { slot, data: Data { payload } }` в `out`, замаскированный; длина или `None`, если не влезло.
pub fn encode_data(slot: u32, payload: &[u8], keys: &SendKeys, out: &mut [u8]) -> Option<usize> {
    write_lite(out, keys, slot, LITE_DATA, bytes_field_len(payload.len()), |w| {
        if !payload.is_empty() {
            w.byte(DATA_PAYLOAD)?;
            w.varint(payload.len() as u64)?;
            w.bytes(payload)?;
        }
        Some(())
    })
}

/// `Lite { slot, wrapped: WrappedData { seq, payload, client_id, flow } }` в `out`, замаскированный.
pub fn encode_wrapped(
    slot: u32,
    seq: u64,
    payload: &[u8],
    client_id: u32,
    flow: Option<u32>,
    keys: &SendKeys,
    out: &mut [u8],
) -> Option<usize> {
    // `optional`-поле пишется всегда, когда задано (и нулём тоже).
    let flow_len = flow.map_or(0, |f| 1 + varint_len(u64::from(f)));
    let inner_len = varint_field_len(seq) + bytes_field_len(payload.len()) + varint_field_len(u64::from(client_id)) + flow_len;
    write_lite(out, keys, slot, LITE_WRAPPED, inner_len, |w| {
        if seq != 0 {
            w.byte(WRAPPED_SEQ)?;
            w.varint(seq)?;
        }
        if !payload.is_empty() {
            w.byte(WRAPPED_PAYLOAD)?;
            w.varint(payload.len() as u64)?;
            w.bytes(payload)?;
        }
        if client_id != 0 {
            w.byte(WRAPPED_CLIENT)?;
            w.varint(u64::from(client_id))?;
        }
        if let Some(flow) = flow {
            w.byte(WRAPPED_FLOW)?;
            w.varint(u64::from(flow))?;
        }
        Some(())
    })
}

/// `Lite { slot, ordered: Ordered { flow, seq, payload } }` в `out`, замаскированный.
pub fn encode_ordered(slot: u32, flow: u32, seq: u64, payload: &[u8], keys: &SendKeys, out: &mut [u8]) -> Option<usize> {
    let inner_len = varint_field_len(u64::from(flow)) + varint_field_len(seq) + bytes_field_len(payload.len());
    write_lite(out, keys, slot, LITE_ORDERED, inner_len, |w| {
        if flow != 0 {
            w.byte(ORDERED_FLOW)?;
            w.varint(u64::from(flow))?;
        }
        if seq != 0 {
            w.byte(ORDERED_SEQ)?;
            w.varint(seq)?;
        }
        if !payload.is_empty() {
            w.byte(ORDERED_PAYLOAD)?;
            w.varint(payload.len() as u64)?;
            w.bytes(payload)?;
        }
        Some(())
    })
}

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn done(&self) -> bool {
        self.pos == self.buf.len()
    }

    fn byte(&mut self) -> Option<u8> {
        let b = *self.buf.get(self.pos)?;
        self.pos += 1;
        Some(b)
    }

    fn varint(&mut self) -> Option<u64> {
        let mut v = 0u64;
        for shift in (0..64).step_by(7) {
            let b = self.byte()?;
            v |= u64::from(b & 0x7f) << shift;
            if b < 0x80 {
                return Some(v);
            }
        }
        None
    }

    fn bytes(&mut self) -> Option<&'a [u8]> {
        let len = usize::try_from(self.varint()?).ok()?;
        let end = self.pos.checked_add(len)?;
        let data = self.buf.get(self.pos..end)?;
        self.pos = end;
        Some(data)
    }
}

/// Разбор снятого с маски пакета: `Some`, только если это ровно `Lite` с `Data` или
/// `WrappedData` без неизвестных и повторных полей; иначе `None` — разбирать prost'ом.
pub fn parse(buf: &[u8]) -> Option<Fast<'_>> {
    let mut top = Reader { buf, pos: 0 };
    if top.byte()? != PEER_LITE {
        return None;
    }
    let lite = top.bytes()?;
    if !top.done() {
        return None;
    }
    let mut r = Reader { buf: lite, pos: 0 };
    let (mut slot, mut body) = (None, None);
    while !r.done() {
        match r.byte()? {
            LITE_SLOT if slot.is_none() => slot = Some(r.varint()? as u32),
            tag @ (LITE_DATA | LITE_WRAPPED | LITE_ORDERED) if body.is_none() => body = Some((tag, r.bytes()?)),
            _ => return None,
        }
    }
    let slot = slot.unwrap_or(0);
    let (tag, inner) = body?;
    let mut r = Reader { buf: inner, pos: 0 };
    if tag == LITE_DATA {
        let mut payload = None;
        while !r.done() {
            match r.byte()? {
                DATA_PAYLOAD if payload.is_none() => payload = Some(r.bytes()?),
                _ => return None,
            }
        }
        return Some(Fast::Data { slot, payload: payload.unwrap_or(&[]) });
    }
    if tag == LITE_ORDERED {
        let (mut flow, mut seq, mut payload) = (None, None, None);
        while !r.done() {
            match r.byte()? {
                ORDERED_FLOW if flow.is_none() => flow = Some(r.varint()? as u32),
                ORDERED_SEQ if seq.is_none() => seq = Some(r.varint()?),
                ORDERED_PAYLOAD if payload.is_none() => payload = Some(r.bytes()?),
                _ => return None,
            }
        }
        return Some(Fast::Ordered { slot, flow: flow.unwrap_or(0), seq: seq.unwrap_or(0), payload: payload.unwrap_or(&[]) });
    }
    let (mut seq, mut payload, mut client_id, mut flow) = (None, None, None, None);
    while !r.done() {
        match r.byte()? {
            WRAPPED_SEQ if seq.is_none() => seq = Some(r.varint()?),
            WRAPPED_PAYLOAD if payload.is_none() => payload = Some(r.bytes()?),
            WRAPPED_CLIENT if client_id.is_none() => client_id = Some(r.varint()? as u32),
            WRAPPED_FLOW if flow.is_none() => flow = Some(r.varint()? as u32),
            _ => return None,
        }
    }
    Some(Fast::Wrapped {
        slot,
        seq: seq.unwrap_or(0),
        payload: payload.unwrap_or(&[]),
        client_id: client_id.unwrap_or(0),
        flow,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::{lite, peer_message, Data, KeepAlive, Lite, Ordered, PeerMessage, WrappedData};
    use prost::Message as _;

    /// Два подписчика с одним ключом и счётчиками с нуля: при одинаковом числе вызовов дают
    /// одинаковые подписи — так быстрый путь сверяется с prost байт-в-байт.
    fn twin_sealers() -> (SendKeys, SendKeys) {
        let xor = codec::random_key();
        let keys = || SendKeys { xor, sealer: crate::auth::Sealer::new(&[4u8; 32]) };
        (keys(), keys())
    }

    fn data_msg(slot: u32, payload: Vec<u8>) -> PeerMessage {
        PeerMessage { body: Some(peer_message::Body::Lite(Lite { slot, payload: Some(lite::Payload::Data(Data { payload })) })) }
    }

    fn wrapped_msg(slot: u32, seq: u64, payload: Vec<u8>, client_id: u32, flow: Option<u32>) -> PeerMessage {
        PeerMessage {
            body: Some(peer_message::Body::Lite(Lite {
                slot,
                payload: Some(lite::Payload::Wrapped(WrappedData { seq, payload, client_id, flow })),
            })),
        }
    }

    fn lens() -> [usize; 8] {
        [0, 1, 50, 126, 127, 128, 1392, 1400]
    }

    #[test]
    fn data_is_byte_for_byte_what_prost_produces() {
        let (prost_sealer, fast_sealer) = twin_sealers();
        let mut out = [0u8; 1500];
        for slot in [0u32, 1, 9, 127, 128, 300] {
            for len in lens() {
                let payload: Vec<u8> = (0..len).map(|i| (i * 7) as u8).collect();
                let expected = codec::encode(&data_msg(slot, payload.clone()), &prost_sealer);
                let n = encode_data(slot, &payload, &fast_sealer, &mut out).unwrap();
                assert_eq!(&out[..n], &expected[..], "slot {slot}, len {len}");
            }
        }
    }

    #[test]
    fn wrapped_is_byte_for_byte_what_prost_produces() {
        let (prost_sealer, fast_sealer) = twin_sealers();
        let mut out = [0u8; 1500];
        for (seq, client_id) in [(0u64, 0u32), (1, 1), (127, 255), (u64::from(u32::MAX) + 5, 3), (u64::MAX, 200)] {
            for flow in [None, Some(0u32), Some(15), Some(300)] {
                for len in lens() {
                    let payload: Vec<u8> = (0..len).map(|i| (i * 3) as u8).collect();
                    let msg = wrapped_msg(4, seq, payload.clone(), client_id, flow);
                    let expected = codec::encode(&msg, &prost_sealer);
                    let n = encode_wrapped(4, seq, &payload, client_id, flow, &fast_sealer, &mut out).unwrap();
                    assert_eq!(&out[..n], &expected[..], "seq {seq}, client {client_id}, flow {flow:?}, len {len}");
                    let plain = msg.encode_to_vec();
                    assert_eq!(parse(&plain), Some(Fast::Wrapped { slot: 4, seq, payload: &payload, client_id, flow }));
                }
            }
        }
    }

    fn ordered_msg(slot: u32, flow: u32, seq: u64, payload: Vec<u8>) -> PeerMessage {
        PeerMessage {
            body: Some(peer_message::Body::Lite(Lite { slot, payload: Some(lite::Payload::Ordered(Ordered { flow, seq, payload })) })),
        }
    }

    #[test]
    fn ordered_is_byte_for_byte_what_prost_produces_and_parses_back() {
        let (prost_sealer, fast_sealer) = twin_sealers();
        let mut out = [0u8; 1500];
        for (flow, seq) in [(0u32, 0u64), (1, 1), (15, 127), (7, 128), (3, u64::from(u32::MAX) + 9)] {
            for len in lens() {
                let payload: Vec<u8> = (0..len).map(|i| (i * 5) as u8).collect();
                let expected = codec::encode(&ordered_msg(6, flow, seq, payload.clone()), &prost_sealer);
                let n = encode_ordered(6, flow, seq, &payload, &fast_sealer, &mut out).unwrap();
                assert_eq!(&out[..n], &expected[..], "flow {flow}, seq {seq}, len {len}");
                let plain = ordered_msg(6, flow, seq, payload.clone()).encode_to_vec();
                assert_eq!(parse(&plain), Some(Fast::Ordered { slot: 6, flow, seq, payload: &payload }));
            }
        }
    }

    #[test]
    fn parse_reads_what_prost_encodes() {
        for len in lens() {
            let payload: Vec<u8> = (0..len).map(|i| i as u8).collect();
            let bytes = data_msg(7, payload.clone()).encode_to_vec();
            assert_eq!(parse(&bytes), Some(Fast::Data { slot: 7, payload: &payload }));
            let bytes = wrapped_msg(0, 42, payload.clone(), 9, None).encode_to_vec();
            assert_eq!(parse(&bytes), Some(Fast::Wrapped { slot: 0, seq: 42, payload: &payload, client_id: 9, flow: None }));
        }
    }

    #[test]
    fn everything_else_goes_to_prost() {
        let keepalive = PeerMessage {
            body: Some(peer_message::Body::Lite(Lite { slot: 1, payload: Some(lite::Payload::KeepAlive(KeepAlive { seq: 3 })) })),
        };
        assert_eq!(parse(&keepalive.encode_to_vec()), None);
        let mut trailing = data_msg(1, b"x".to_vec()).encode_to_vec();
        trailing.push(0);
        assert_eq!(parse(&trailing), None, "мусор после сообщения");
        let mut truncated = data_msg(1, vec![5; 100]).encode_to_vec();
        truncated.truncate(50);
        assert_eq!(parse(&truncated), None, "обрезанный пакет");
        assert_eq!(parse(&[]), None);
        assert_eq!(parse(&[0xff; 64]), None);
    }

    #[test]
    fn too_small_output_is_refused() {
        let keys = SendKeys { xor: codec::random_key(), sealer: crate::auth::Sealer::new(&[1u8; 32]) };
        assert_eq!(encode_data(1, &[0u8; 1400], &keys, &mut [0u8; 100]), None);
        assert_eq!(encode_data(1, &[0u8; 80], &keys, &mut [0u8; 100]), None, "без места под подпись");
    }
}
