//! Утилиты для перебора портов при hole punching: какой диапазон пробивать и
//! в каком порядке по нему идти.
//!
//! # Примеры
//!
//! Диапазон перебора — от меньшего из двух портов минус запас до большего
//! плюс запас. Якорями служат оба порта, увиденных STUN'ом (наш и пира):
//!
//! ```
//! use connection::port_utils::sweep_bounds;
//!
//! // наш STUN-порт 20101, порт пира 20102, запас 1000
//! assert_eq!(sweep_bounds(20101, 20102, 1000), (19101, 21102));
//!
//! // порядок аргументов не важен
//! assert_eq!(sweep_bounds(20102, 20101, 1000), (19101, 21102));
//!
//! // у нижней границы порт 0 не используется — начинаем с 1
//! assert_eq!(sweep_bounds(500, 600, 1000), (1, 1600));
//!
//! // у верхней границы диапазон упирается в 65535, а не переполняется
//! assert_eq!(sweep_bounds(65000, 65100, 1000), (64000, 65535));
//! ```
//!
//! Порядок перебора — зигзаг от центра наружу: сначала сам центр, затем
//! попеременно на шаг вверх и на шаг вниз:
//!
//! ```
//! use connection::port_utils::zigzag_ports;
//!
//! assert_eq!(zigzag_ports(100, 97, 103), vec![100, 101, 99, 102, 98, 103, 97]);
//!
//! // диапазон несимметричный (центр ближе к нижнему краю): когда нижняя
//! // сторона исчерпана, идём дальше только по верхней
//! assert_eq!(zigzag_ports(100, 99, 104), vec![100, 101, 99, 102, 103, 104]);
//!
//! // порт 0 пропускается
//! assert_eq!(zigzag_ports(1, 0, 3), vec![1, 2, 3]);
//! ```
//!
//! Вместе: диапазон по обоим портам, а начинаем с порта пира — его NAT,
//! скорее всего, выделит нам близкий порт:
//!
//! ```
//! use connection::port_utils::{sweep_bounds, zigzag_ports};
//!
//! let (my_port, peer_port) = (20101, 20102);
//! let (low, high) = sweep_bounds(my_port, peer_port, 2); // (20099, 20104)
//! let ports = zigzag_ports(peer_port, low, high);
//!
//! assert_eq!(ports, vec![20102, 20103, 20101, 20104, 20100, 20099]);
//! ```

/// Строит порядок перебора портов: сначала `center`, затем поочерёдно
/// наружу (center+1, center-1, center+2, center-2, ...), пока не дойдём и до
/// `low`, и до `high`. Ожидается `low <= center <= high` (вызывающий выводит
/// `low`/`high` из `center` плюс запас, так что это всегда выполняется);
/// порты в любом случае ограничиваются допустимыми u16.
pub fn zigzag_ports(center: u16, low: u16, high: u16) -> Vec<u16> {
    let down_steps = center.saturating_sub(low);
    let up_steps = high.saturating_sub(center);
    let steps = down_steps.max(up_steps);

    let mut ports = Vec::with_capacity(steps as usize * 2 + 1);
    ports.push(center);
    for d in 1..=steps {
        if d <= up_steps {
            if let Some(p) = center.checked_add(d) {
                ports.push(p);
            }
        }
        if d <= down_steps {
            if let Some(p) = center.checked_sub(d).filter(|&p| p != 0) {
                ports.push(p);
            }
        }
    }
    ports
}

/// Диапазон `[min(my_port, peer_port) - margin, max(my_port, peer_port) + margin]`.
pub fn sweep_bounds(my_port: u16, peer_port: u16, margin: u16) -> (u16, u16) {
    let low = my_port.min(peer_port).saturating_sub(margin).max(1);
    let high = my_port.max(peer_port).saturating_add(margin);
    (low, high)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zigzag_fans_outward_from_center() {
        assert_eq!(zigzag_ports(100, 97, 103), vec![100, 101, 99, 102, 98, 103, 97]);
    }

    #[test]
    fn zigzag_covers_asymmetric_bounds() {
        // центр ближе к нижнему краю, чем к верхнему: когда нижняя сторона
        // исчерпана, продолжаем идти по верхней до `high`.
        assert_eq!(zigzag_ports(100, 99, 104), vec![100, 101, 99, 102, 103, 104]);
    }

    #[test]
    fn zigzag_clamps_at_port_bounds() {
        assert_eq!(zigzag_ports(1, 0, 3), vec![1, 2, 3]);
        assert_eq!(zigzag_ports(u16::MAX, u16::MAX - 2, u16::MAX), vec![u16::MAX, u16::MAX - 1, u16::MAX - 2]);
    }

    #[test]
    fn sweep_bounds_span_both_ports_plus_margin() {
        assert_eq!(sweep_bounds(20101, 20102, 1000), (19101, 21102));
        // порядок аргументов не важен
        assert_eq!(sweep_bounds(20102, 20101, 1000), (19101, 21102));
    }
}
