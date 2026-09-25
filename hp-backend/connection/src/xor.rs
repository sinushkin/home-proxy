//! Побайтовый XOR пакета циклическим 16-байтным вектором, с ядрами под разные
//! процессоры.
//!
//! Одно и то же тело (`xor_wide`) компилируется несколько раз: под базовый
//! набор инструкций (SSE2 на x86_64, NEON на aarch64 — по 16 байт за шаг), под
//! AVX2 (32 байта) и под AVX-512 (64 байта). Какое ядро запускать, решаем на
//! лету по `is_x86_feature_detected!`, так что один бинарник работает на любом
//! CPU и использует максимум, что тот умеет.
//!
//! Блок каждого ядра кратен 16, поэтому на границе блока вектор всегда
//! начинается заново, и хвост считается от его начала.

pub const KEY_LEN: usize = 16;

/// XOR-вектор одной дыры в одну сторону.
pub type XorKey = [u8; KEY_LEN];

/// Вектор, повторённый до самого широкого блока (64 байта): ядра берут из него
/// префикс своей ширины.
type Pad = [u8; 64];

/// XOR `data` на `key`, повторяемый циклически.
pub fn xor_in_place(data: &mut [u8], key: &XorKey) {
    let pad = expand(key);

    #[cfg(target_arch = "x86_64")]
    {
        if is_x86_feature_detected!("avx512f") {
            // SAFETY: наличие avx512f только что проверено.
            return unsafe { xor_avx512(data, &pad) };
        }
        if is_x86_feature_detected!("avx2") {
            // SAFETY: наличие avx2 только что проверено.
            return unsafe { xor_avx2(data, &pad) };
        }
    }

    xor_baseline(data, &pad);
}

fn expand(key: &XorKey) -> Pad {
    let mut pad = [0u8; 64];
    for block in pad.chunks_exact_mut(KEY_LEN) {
        block.copy_from_slice(key);
    }
    pad
}

fn xor_baseline(data: &mut [u8], pad: &Pad) {
    xor_wide::<16>(data, pad);
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
fn xor_avx2(data: &mut [u8], pad: &Pad) {
    xor_wide::<32>(data, pad);
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx512f")]
fn xor_avx512(data: &mut [u8], pad: &Pad) {
    xor_wide::<64>(data, pad);
}

/// Целые блоки по `N` байт, затем остаток блоками по 16 (короткие пакеты, вроде
/// keep-alive, не должны целиком уходить в побайтовый хвост), затем байты.
/// `inline(always)` — чтобы тело скомпилировалось с набором инструкций
/// вызывающего ядра.
#[inline(always)]
fn xor_wide<const N: usize>(data: &mut [u8], pad: &Pad) {
    let rest = xor_blocks::<N>(data, pad);
    let rest = xor_blocks::<KEY_LEN>(rest, pad);
    for (byte, k) in rest.iter_mut().zip(pad) {
        *byte ^= k;
    }
}

#[inline(always)]
fn xor_blocks<'a, const N: usize>(data: &'a mut [u8], pad: &Pad) -> &'a mut [u8] {
    let pad: &[u8; N] = pad[..N].try_into().unwrap();
    let mut blocks = data.chunks_exact_mut(N);
    for block in &mut blocks {
        for (byte, k) in block.iter_mut().zip(pad) {
            *byte ^= k;
        }
    }
    blocks.into_remainder()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reference(data: &[u8], key: &XorKey) -> Vec<u8> {
        data.iter().enumerate().map(|(i, b)| b ^ key[i % KEY_LEN]).collect()
    }

    fn sample_key() -> XorKey {
        std::array::from_fn(|i| (i as u8).wrapping_mul(37).wrapping_add(11))
    }

    /// Длины 0..=300 покрывают все остатки по модулю 16/32/64 и несколько
    /// полных блоков каждой ширины.
    fn check(kernel: impl Fn(&mut [u8], &Pad)) {
        let key = sample_key();
        let pad = expand(&key);
        for len in 0..=300usize {
            let original: Vec<u8> = (0..len).map(|i| (i as u8).wrapping_mul(7).wrapping_add(3)).collect();
            let mut got = original.clone();
            kernel(&mut got, &pad);
            assert_eq!(got, reference(&original, &key), "длина {len}");
        }
    }

    #[test]
    fn dispatched_matches_reference() {
        let key = sample_key();
        for len in 0..=300usize {
            let original: Vec<u8> = (0..len).map(|i| (i as u8).wrapping_mul(7).wrapping_add(3)).collect();
            let mut got = original.clone();
            xor_in_place(&mut got, &key);
            assert_eq!(got, reference(&original, &key), "длина {len}");
        }
    }

    #[test]
    fn baseline_matches_reference() {
        check(xor_baseline);
    }

    #[test]
    fn block_widths_match_reference_on_any_cpu() {
        check(xor_wide::<16>);
        check(xor_wide::<32>);
        check(xor_wide::<64>);
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn avx2_matches_reference_when_available() {
        if !is_x86_feature_detected!("avx2") {
            return;
        }
        // SAFETY: наличие avx2 проверено выше.
        check(|d, p| unsafe { xor_avx2(d, p) });
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn avx512_matches_reference_when_available() {
        if !is_x86_feature_detected!("avx512f") {
            return;
        }
        // SAFETY: наличие avx512f проверено выше.
        check(|d, p| unsafe { xor_avx512(d, p) });
    }

    #[test]
    fn xor_twice_restores_data() {
        let key = sample_key();
        let original: Vec<u8> = (0..1500u32).map(|i| i as u8).collect();
        let mut data = original.clone();
        xor_in_place(&mut data, &key);
        assert_ne!(data, original);
        xor_in_place(&mut data, &key);
        assert_eq!(data, original);
    }
}
