//! The [`GossipIo`](serf_embedded::GossipIo) implementation over an embassy-net
//! [`UdpSocket`], with serf's self-delivery loopback.
//!
//! A short-lived view over the gossip socket, rebuilt each engine pump. The
//! engine reads inbound gossip and writes outbound gossip through it without
//! touching embassy-net; the actual stack progress (link RX/TX) the embassy-net
//! `Stack` drives on its own.
//!
//! embassy-net 0.9 exposes only async + `poll_*` UDP methods (no `try_*`), so
//! the non-blocking [`GossipIo`] ops drive `poll_recv_from` / `poll_send_to`
//! with a no-op [`Waker`]: the engine pump is synchronous and re-polls each
//! socket on the next driver tick, so no datagram is lost by not registering a
//! real waker here (the runner registers the real recv waker around the pump).
//!
//! # Self-delivery loopback
//!
//! A real OS UDP socket loops a datagram addressed to its own bound address back
//! into `recv`; embassy-net (smoltcp underneath) does not — a self-addressed frame
//! goes to the wire and is dropped. serf directs a node's response to its OWN
//! locally-originated query / key-request to the originator's advertise address,
//! i.e. THIS node's address, so on embassy-net that response would be lost and a
//! local op would report `num_resp < num_nodes`. This view emulates the OS
//! self-delivery: [`send`] to the node's own `advertise` address queues the
//! datagram on a driver-owned `loopback` buffer instead of the socket, and
//! [`recv`] returns those queued datagrams FIRST (as if received from `advertise`)
//! so a subsequent pump ingests them exactly as an OS loopback would. The bytes
//! are the already-transformed wire form the engine emitted, so feeding them back
//! through the normal gossip ingress decodes them identically.
//!
//! [`send`]: GossipIo::send
//! [`recv`]: GossipIo::recv

use core::{
  cell::RefCell,
  net::{IpAddr, Ipv4Addr, SocketAddr},
  task::{Context, Poll, Waker},
};

use alloc::{collections::VecDeque, vec::Vec};

use embassy_net::{IpEndpoint, udp::UdpSocket};
use serf_embedded::GossipIo;

/// A [`GossipIo`] view over a single bound gossip [`UdpSocket`] plus the driver's
/// self-delivery loopback buffer.
///
/// serf binds one advertise address, so a single socket suffices (no v4/v6 split).
/// Built fresh for each engine pump over the already-progressed socket.
pub struct SerfGossip<'a> {
  socket: &'a UdpSocket<'a>,
  /// Driver-owned self-delivery buffer: datagrams this node addressed to its own
  /// `advertise` address, awaiting loopback ingestion on the next pump. A separate
  /// field, never inside the socket borrow.
  loopback: &'a RefCell<VecDeque<Vec<u8>>>,
  /// The local node's resolved advertise address — the destination a self-addressed
  /// gossip datagram carries.
  advertise: SocketAddr,
}

impl<'a> SerfGossip<'a> {
  /// Build the gossip view over the bound `socket`, with the driver's `loopback`
  /// self-delivery buffer and the node's `advertise` address.
  #[inline]
  pub fn new(
    socket: &'a UdpSocket<'a>,
    loopback: &'a RefCell<VecDeque<Vec<u8>>>,
    advertise: SocketAddr,
  ) -> Self {
    Self {
      socket,
      loopback,
      advertise,
    }
  }
}

impl GossipIo for SerfGossip<'_> {
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

    let mut cx = Context::from_waker(Waker::noop());
    match self.socket.poll_recv_from(buf, &mut cx) {
      Poll::Ready(Ok((len, meta))) => Some((meta.endpoint.into(), len)),
      // `RecvError::Truncated` (the only `Poll::Ready(Err)`): an oversized
      // datagram — larger than this buffer (the configured gossip MTU plus
      // encryption overhead) — was already DEQUEUED by embassy-net before the
      // length check, so it is consumed and gone. Surface a zero-length marker
      // (like the smoltcp driver's `Truncated` skip) so the engine's drain loop
      // treats it as nothing to deliver and RE-POLLS for the next datagram,
      // instead of stopping early: one oversized datagram cannot stall the
      // in-budget datagrams queued behind it. The source address is irrelevant
      // for a zero-length frame; `handle_gossip` on an empty slice is a no-op.
      Poll::Ready(Err(_)) => Some((SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0), 0)),
      // No datagram queued: the drain loop ends for this pump.
      Poll::Pending => None,
    }
  }

  fn send(&mut self, bytes: &[u8], dest: SocketAddr) {
    if dest == self.advertise {
      // Self-addressed: embassy-net will not loop this back into its own rx ring
      // the way an OS UDP socket does, so deliver it into the driver's loopback
      // buffer for the next pump's ingress instead of dropping it on the wire.
      self.loopback.borrow_mut().push_back(bytes.to_vec());
      return;
    }
    let mut cx = Context::from_waker(Waker::noop());
    // Ignoring the result: gossip is best-effort. A full tx ring (`Poll::Pending`)
    // or any `SendError` (no route, socket not bound, packet too large) drops this
    // datagram and SWIM recovers on the next gossip round — no error is surfaced.
    let _ = self
      .socket
      .poll_send_to(bytes, IpEndpoint::from(dest), &mut cx);
  }
}
