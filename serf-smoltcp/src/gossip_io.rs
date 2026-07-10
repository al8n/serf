//! The [`GossipIo`](serf_embedded::GossipIo) implementation over the bound smoltcp
//! gossip `udp::Socket`.
//!
//! A short-lived view, rebuilt each [`Serf::poll`](crate::Serf::poll) over the
//! already-ticked socket set and the gossip UDP [`SocketHandle`]. The engine reads
//! inbound gossip and writes outbound gossip through it without touching smoltcp;
//! the actual stack tick (`iface.poll`) the driver performs before handing this
//! view to the engine.
//!
//! smoltcp keeps every socket — the gossip UDP socket and the reliable-plane TCP
//! pool alike — in one [`SocketSet`], so the gossip and stream views must share
//! mutable access to it. They borrow it through a [`RefCell`] the driver holds for
//! the duration of one pump; each trait method takes a brief `borrow_mut`, never
//! holding a socket borrow across a call into the other view, so the borrows never
//! overlap at runtime.

use core::{cell::RefCell, net::SocketAddr};

use std::{collections::VecDeque, vec::Vec};

use serf_embedded::GossipIo;
use smoltcp::{
  iface::{SocketHandle, SocketSet},
  socket::udp,
};

use crate::addr::{from_endpoint, to_endpoint};

/// A [`GossipIo`] view over the gossip `udp::Socket` in a shared [`SocketSet`].
///
/// Resolves the gossip socket by its [`SocketHandle`] on each call, taking a brief
/// `borrow_mut` of the shared set.
///
/// # Self-delivery loopback
///
/// A real OS UDP socket loops a datagram addressed to its own bound address back
/// into `recv`; smoltcp does not — a self-addressed frame goes to the wire and is
/// dropped. serf directs a node's response to its OWN locally-originated query /
/// key-request to the originator's advertise address, i.e. THIS node's address, so
/// on smoltcp that response would be lost and a local op would report
/// `num_resp < num_nodes`. This view emulates the OS self-delivery: [`send`] to the
/// node's own `advertise` address queues the datagram on a driver-owned `loopback`
/// buffer instead of the socket, and [`recv`] returns those queued datagrams FIRST
/// (as if received from `advertise`) so a subsequent pump ingests them exactly as an
/// OS loopback would. The bytes are the already-transformed wire form the engine
/// emitted, so feeding them back through the normal gossip ingress decodes them
/// identically.
///
/// [`send`]: GossipIo::send
/// [`recv`]: GossipIo::recv
pub(crate) struct SmoltcpGossip<'a, 'b> {
  sockets: &'a RefCell<&'a mut SocketSet<'b>>,
  udp: SocketHandle,
  /// Driver-owned self-delivery buffer: datagrams this node addressed to its own
  /// `advertise` address, awaiting loopback ingestion on the next pump. A separate
  /// field, never inside the `SocketSet` borrow.
  loopback: &'a RefCell<&'a mut VecDeque<Vec<u8>>>,
  /// The local node's resolved advertise address — the destination a self-addressed
  /// gossip datagram carries.
  advertise: SocketAddr,
}

impl<'a, 'b> SmoltcpGossip<'a, 'b> {
  /// Build the view over the shared `sockets` for the gossip socket `udp`, with the
  /// driver's `loopback` self-delivery buffer and the node's `advertise` address.
  pub(crate) fn new(
    sockets: &'a RefCell<&'a mut SocketSet<'b>>,
    udp: SocketHandle,
    loopback: &'a RefCell<&'a mut VecDeque<Vec<u8>>>,
    advertise: SocketAddr,
  ) -> Self {
    Self {
      sockets,
      udp,
      loopback,
      advertise,
    }
  }
}

impl GossipIo for SmoltcpGossip<'_, '_> {
  fn recv(&mut self, buf: &mut [u8]) -> Option<(SocketAddr, usize)> {
    // Self-delivery first: return any datagrams this node addressed to its own
    // advertise address, exactly as an OS UDP socket would loop them back into recv,
    // before draining the real rx ring.
    loop {
      let datagram = self.loopback.borrow_mut().pop_front();
      let Some(datagram) = datagram else { break };
      let n = datagram.len();
      if n <= buf.len() {
        buf[..n].copy_from_slice(&datagram);
        return Some((self.advertise, n));
      }
      // An own datagram larger than the ingress buffer cannot be delivered; drop it
      // and continue, mirroring the rx-ring `Truncated` skip below. A self datagram
      // is bounded by this node's own gossip MTU + encryption overhead, so it fits
      // the arena-sized ingress buffer in practice — this is defence-in-depth.
    }

    let mut set = self.sockets.borrow_mut();
    let sock = set.get_mut::<udp::Socket>(self.udp);
    // Pop the next deliverable datagram. `recv_slice` is called only while
    // `can_recv()` holds, so an empty rx ring is a clean `None` rather than an
    // `Exhausted` error to interpret.
    while sock.can_recv() {
      match sock.recv_slice(buf) {
        Ok((n, meta)) => return Some((from_endpoint(meta.endpoint), n)),
        // The datagram exceeded `buf` and was already POPPED by `recv_slice`
        // (smoltcp dequeues before the length check), so it is consumed and gone.
        // This is an over-budget peer datagram — larger than the configured gossip
        // MTU plus encryption overhead the buffer is sized for. Skip it and CONTINUE
        // draining the rest of the rx ring rather than returning, so one oversized
        // datagram cannot stall delivery of the in-budget datagrams queued behind it.
        Err(udp::RecvError::Truncated) => continue,
        // The ring is empty (`can_recv()` raced false): nothing more to deliver.
        Err(udp::RecvError::Exhausted) => return None,
      }
    }
    None
  }

  fn send(&mut self, bytes: &[u8], dest: SocketAddr) {
    if dest == self.advertise {
      // Self-addressed: smoltcp will not loop this back into its own rx ring the way
      // an OS UDP socket does, so deliver it into the driver's loopback buffer for
      // the next pump's ingress instead of dropping it on the wire.
      self.loopback.borrow_mut().push_back(bytes.to_vec());
      return;
    }
    let mut set = self.sockets.borrow_mut();
    let sock = set.get_mut::<udp::Socket>(self.udp);
    // Ignoring Err: gossip is best-effort — a full or errored UDP tx ring drops
    // this datagram and SWIM recovers on the next gossip round.
    let _ = sock.send_slice(bytes, to_endpoint(dest));
  }
}
