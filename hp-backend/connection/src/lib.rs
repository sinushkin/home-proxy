pub mod auth;
pub mod bind;
pub mod codec;
pub mod dedup;
pub mod holes;
pub mod label;
pub mod link_id;
pub mod multilink;
mod p2p;
pub mod pool;
pub mod port_utils;
pub mod punch;
pub mod reorder;
pub mod rendezvous;
#[cfg(feature = "stats")]
pub(crate) mod stats_feedback;
pub mod stun;
pub mod vps;
pub mod wire;
pub mod xor;

pub mod proto {
    include!(concat!(env!("OUT_DIR"), "/hp_backend.connection.rs"));
}
