#[cfg(not(any(feature = "p2p", feature = "vps")))]
compile_error!("включите хотя бы один режим знакомства: фича `p2p` или `vps`");

pub mod auth;
pub mod bind;
pub mod codec;
pub mod dedup;
pub mod discovery;
pub(crate) mod drain;
pub mod holes;
pub mod label;
pub mod link_id;
pub mod multilink;
#[cfg(feature = "p2p")]
pub mod p2p;
pub mod pool;
pub mod port_pool;
pub mod port_utils;
pub mod punch;
pub mod reorder;
pub mod rendezvous;
#[cfg(test)]
mod test_support;
#[cfg(feature = "p2p")]
pub use p2p::stun;
#[cfg(feature = "stats")]
pub(crate) mod stats_feedback;
#[cfg(feature = "vps")]
pub mod vps;
pub mod wire;
pub mod xor;

pub mod proto {
    include!(concat!(env!("OUT_DIR"), "/hp_backend.connection.rs"));
}
