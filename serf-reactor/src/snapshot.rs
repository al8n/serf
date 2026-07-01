//! Snapshot of serf membership state, published lock-free for handle reads.
//!
//! [`SerfSnapshot`] is the immutable membership view the driver republishes after
//! every state-affecting tick. The reactor driver stores the current one in the
//! [`Shared`](crate::shared) state's [`arc_swap::ArcSwap`], and a
//! [`Serf`](crate::Serf) handle (plus every clone) reads the latest with a
//! lock-free `load_full`. It is the `Send`/`Arc` analog of serf-compio's
//! single-owner `Rc<RefCell<Rc<SerfSnapshot>>>` cell.
//!
//! The handle's snapshot read-forwarders (`members` / `local_member` / `state` /
//! `advertise_address` / `advertise_node` / `local_id` / `default_query_timeout`
//! / `default_query_param`) live on [`Serf`](crate::Serf); they each load the
//! published snapshot and project one view out of it.

#[cfg(any(feature = "tcp", feature = "quic"))]
pub use serf_driver::SerfSnapshot;
