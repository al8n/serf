use std::{
  collections::HashSet,
  sync::Arc,
  time::{Duration, Instant},
};

use async_channel::{Receiver, Sender};
use async_lock::RwLock;
use futures::{FutureExt, StreamExt, stream::FuturesUnordered};
use memberlist_core::{
  CheapClone,
  bytes::{BufMut, Bytes, BytesMut},
  proto::{OneOrMore, SmallVec, TinyVec},
  tracing,
  transport::{AddressResolver, Id, Node, Transport},
};

use crate::{
  delegate::Delegate,
  error::Error,
  types::{
    Filter, LamportTime, Member, MemberStatus, MessageType, QueryMessage, QueryResponseMessage,
  },
};

use super::Serf;

/// Provided to [`Serf::query`] to configure the parameters of the
/// query. If not provided, sane defaults will be used.
#[viewit::viewit(
  vis_all = "pub(crate)",
  getters(vis_all = "pub", style = "ref"),
  setters(vis_all = "pub", prefix = "with")
)]
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct QueryParam<I> {
  /// The filters to apply to the query.
  #[viewit(
    getter(const, attrs(doc = "Returns the filters of the query")),
    setter(attrs(doc = "Sets the filters of the query"))
  )]
  filters: OneOrMore<Filter<I>>,

  /// If true, we are requesting an delivery acknowledgement from
  /// every node that meets the filter requirement. This means nodes
  /// the receive the message but do not pass the filters, will not
  /// send an ack.
  #[viewit(
    getter(
      const,
      style = "move",
      attrs(
        doc = "Returns if we are requesting an delivery acknowledgement from every node that meets the filter requirement. This means nodes the receive the message but do not pass the filters, will not send an ack."
      )
    ),
    setter(attrs(
      doc = "Sets if we are requesting an delivery acknowledgement from every node that meets the filter requirement. This means nodes the receive the message but do not pass the filters, will not send an ack."
    ))
  )]
  request_ack: bool,

  /// Controls the number of duplicate responses to relay
  /// back to the sender through other nodes for redundancy.
  #[viewit(
    getter(
      const,
      style = "move",
      attrs(
        doc = "Returns the number of duplicate responses to relay back to the sender through other nodes for redundancy."
      )
    ),
    setter(attrs(
      doc = "Sets the number of duplicate responses to relay back to the sender through other nodes for redundancy."
    ))
  )]
  relay_factor: u8,

  /// The timeout limits how long the query is left open. If not provided,
  /// then a default timeout is used based on the configuration of Serf
  #[viewit(
    getter(
      const,
      style = "move",
      attrs(
        doc = "Returns timeout limits how long the query is left open. If not provided, then a default timeout is used based on the configuration of [`Serf`]"
      )
    ),
    setter(attrs(doc = "Sets timeout limits how long the query is left open."))
  )]
  #[cfg_attr(feature = "serde", serde(with = "humantime_serde"))]
  timeout: Duration,
}

struct QueryResponseChannel<I, A> {
  /// Used to send the name of a node for which we've received an ack
  ack_ch: Option<(Sender<Node<I, A>>, Receiver<Node<I, A>>)>,
  /// Used to send a response from a node
  resp_ch: (Sender<NodeResponse<I, A>>, Receiver<NodeResponse<I, A>>),
}

pub(crate) struct QueryResponseCore<I, A> {
  closed: bool,
  acks: HashSet<Node<I, A>>,
  responses: HashSet<Node<I, A>>,
}

pub(crate) struct QueryResponseInner<I, A> {
  core: RwLock<QueryResponseCore<I, A>>,
  channel: QueryResponseChannel<I, A>,
}

/// Returned for each new Query. It is used to collect
/// Ack's as well as responses and to provide those back to a client.
#[viewit::viewit(vis_all = "pub(crate)")]
#[derive(Clone)]
pub struct QueryResponse<I, A> {
  /// The duration of the query
  #[viewit(
    getter(
      style = "move",
      const,
      attrs(doc = "Returns the ending deadline of the query")
    ),
    setter(skip)
  )]
  deadline: Instant,

  /// The query id
  #[viewit(
    getter(style = "move", const, attrs(doc = "Returns the id of the query")),
    setter(skip)
  )]
  id: u32,

  /// Stores the LTime of the query
  #[viewit(
    getter(
      style = "move",
      const,
      attrs(doc = "Returns the Lamport Time of the query")
    ),
    setter(skip)
  )]
  ltime: LamportTime,

  #[viewit(getter(vis = "pub(crate)", const, style = "ref"), setter(skip))]
  inner: Arc<QueryResponseInner<I, A>>,
}

impl<I, A> QueryResponse<I, A> {
  pub(crate) fn from_query(q: &QueryMessage<I, A>, num_nodes: usize) -> Self {
    QueryResponse::new(
      q.id(),
      q.ltime(),
      num_nodes,
      Instant::now() + q.timeout(),
      q.ack(),
    )
  }
}

impl<I, A> QueryResponse<I, A> {
  #[inline]
  pub(crate) fn new(
    id: u32,
    ltime: LamportTime,
    num_nodes: usize,
    deadline: Instant,
    ack: bool,
  ) -> Self {
    let (ack_ch, acks) = if ack {
      (
        Some(async_channel::bounded(num_nodes)),
        HashSet::with_capacity(num_nodes),
      )
    } else {
      (None, HashSet::new())
    };

    Self {
      deadline,
      id,
      ltime,
      inner: Arc::new(QueryResponseInner {
        core: RwLock::new(QueryResponseCore {
          closed: false,
          acks,
          responses: HashSet::with_capacity(num_nodes),
        }),
        channel: QueryResponseChannel {
          ack_ch,
          resp_ch: async_channel::bounded(num_nodes),
        },
      }),
    }
  }

  /// Returns a receiver that can be used to listen for acks.
  /// Channel will be closed when the query is finished. This is `None`,
  /// if the query did not specify `request_ack`.
  #[inline]
  pub fn ack_rx(&self) -> Option<async_channel::Receiver<Node<I, A>>> {
    self.inner.channel.ack_ch.as_ref().map(|(_, r)| r.clone())
  }

  /// Returns a receiver that can be used to listen for responses.
  /// Channel will be closed when the query is finished.
  #[inline]
  pub fn response_rx(&self) -> async_channel::Receiver<NodeResponse<I, A>> {
    self.inner.channel.resp_ch.1.clone()
  }

  /// Returns if the query is finished running
  #[inline]
  pub async fn finished(&self) -> bool {
    let c = self.inner.core.read().await;
    c.closed || (Instant::now() > self.deadline)
  }

  /// Used to close the query, which will close the underlying
  /// channels and prevent further deliveries
  #[inline]
  pub async fn close(&self) {
    let mut c = self.inner.core.write().await;
    if c.closed {
      return;
    }

    c.closed = true;

    if let Some((tx, _)) = &self.inner.channel.ack_ch {
      tx.close();
    }

    self.inner.channel.resp_ch.0.close();
  }

  #[inline]
  pub(crate) async fn handle_query_response<T, D>(
    &self,
    resp: QueryResponseMessage<I, A>,
    _local: &T::Id,
    #[cfg(feature = "metrics")] metrics_labels: &memberlist_core::proto::MetricLabels,
  ) where
    I: Eq + std::hash::Hash + CheapClone + core::fmt::Debug,
    A: Eq + std::hash::Hash + CheapClone + core::fmt::Debug,
    D: Delegate<Id = T::Id, Address = <T::Resolver as AddressResolver>::ResolvedAddress>,
    T: Transport,
  {
    // Check if the query is closed
    let c = self.inner.core.read().await;
    if c.closed || (Instant::now() > self.deadline) {
      return;
    }

    // Process each type of response
    if resp.ack() {
      // Exit early if this is a duplicate ack
      if c.acks.contains(&resp.from) {
        #[cfg(feature = "metrics")]
        {
          metrics::counter!("serf.query.duplicate_acks", metrics_labels.iter()).increment(1);
        }
        return;
      }

      #[cfg(feature = "metrics")]
      {
        metrics::counter!("serf.query.acks", metrics_labels.iter()).increment(1);
      }

      drop(c);
      if let Err(e) = self.send_ack::<T, D>(&resp).await {
        tracing::warn!("serf: {}", e);
      }
    } else {
      // Exit early if this is a duplicate response
      if c.responses.contains(&resp.from) {
        #[cfg(feature = "metrics")]
        {
          metrics::counter!("serf.query.duplicate_responses", metrics_labels.iter()).increment(1);
        }
        return;
      }

      #[cfg(feature = "metrics")]
      {
        metrics::counter!("serf.query.responses", metrics_labels.iter()).increment(1);
      }
      drop(c);

      if let Err(e) = self
        .send_response::<T, D>(NodeResponse {
          from: resp.from,
          payload: resp.payload,
        })
        .await
      {
        tracing::warn!("serf: {}", e);
      }
    }
  }

  /// Sends a response on the response channel ensuring the channel is not closed.
  #[inline]
  pub(crate) async fn send_response<T, D>(&self, nr: NodeResponse<I, A>) -> Result<(), Error<T, D>>
  where
    I: Eq + std::hash::Hash + CheapClone + core::fmt::Debug,
    A: Eq + std::hash::Hash + CheapClone + core::fmt::Debug,
    D: Delegate<Id = T::Id, Address = <T::Resolver as AddressResolver>::ResolvedAddress>,
    T: Transport,
  {
    let mut c = self.inner.core.write().await;
    // Exit early if this is a duplicate ack
    if c.responses.contains(&nr.from) {
      return Ok(());
    }

    if c.closed {
      Ok(())
    } else {
      let id = nr.from.cheap_clone();
      futures::select! {
        _ = self.inner.channel.resp_ch.0.send(nr).fuse() => {
          c.responses.insert(id);
          Ok(())
        },
        default => {
          Err(Error::query_response_delivery_failed())
        }
      }
    }
  }

  /// Sends a response on the ack channel ensuring the channel is not closed.
  #[inline]
  pub(crate) async fn send_ack<T, D>(
    &self,
    nr: &QueryResponseMessage<I, A>,
  ) -> Result<(), Error<T, D>>
  where
    I: Eq + std::hash::Hash + CheapClone,
    A: Eq + std::hash::Hash + CheapClone,
    D: Delegate<Id = T::Id, Address = <T::Resolver as AddressResolver>::ResolvedAddress>,
    T: Transport,
  {
    let mut c = self.inner.core.write().await;
    // Exit early if this is a duplicate ack
    if c.acks.contains(&nr.from) {
      return Ok(());
    }

    if c.closed {
      Ok(())
    } else if let Some((tx, _)) = &self.inner.channel.ack_ch {
      futures::select! {
        _ = tx.send(nr.from.cheap_clone()).fuse() => {
          c.acks.insert(nr.from.clone());
          Ok(())
        },
        default => {
          Err(Error::query_response_delivery_failed())
        }
      }
    } else {
      Ok(())
    }
  }
}

/// Used to represent a single response from a node
#[viewit::viewit(
  vis_all = "pub(crate)",
  setters(skip),
  getters(vis_all = "pub", style = "ref")
)]
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct NodeResponse<I, A> {
  #[viewit(getter(attrs(doc = "Returns the node that sent the response")))]
  from: Node<I, A>,
  #[viewit(getter(attrs(doc = "Returns the payload of the response")))]
  payload: Bytes,
}

#[inline]
fn random_members<I, A>(k: usize, mut members: SmallVec<Member<I, A>>) -> SmallVec<Member<I, A>> {
  let n = members.len();
  if n == 0 {
    return SmallVec::new();
  }

  // The modified Fisher-Yates algorithm, but up to 3*n times to ensure exhaustive search for small n.
  let rounds = 3 * n;
  let mut i = 0;

  while i < rounds && i < n {
    let j = (rand::random::<u32>() as usize) % (n - i) + i;
    members.swap(i, j);
    i += 1;
    if i >= k && i >= rounds {
      break;
    }
  }

  members.truncate(k);
  members
}

impl<T, D> Serf<T, D>
where
  D: Delegate<Id = T::Id, Address = <T::Resolver as AddressResolver>::ResolvedAddress>,
  T: Transport,
{
  /// Returns the default timeout value for a query
  /// Computed as
  /// ```text
  /// gossip_interval * query_timeout_mult * log(N+1)
  /// ```
  pub async fn default_query_timeout(&self) -> Duration {
    let n = self.inner.memberlist.num_online_members().await;
    let mut timeout = self.inner.opts.memberlist_options.gossip_interval();
    timeout *= self.inner.opts.query_timeout_mult as u32;
    timeout *= ((n + 1) as f64).log10().ceil() as u32; // Using ceil approximation
    timeout
  }

  /// Used to return the default query parameters
  pub async fn default_query_param(&self) -> QueryParam<T::Id> {
    QueryParam {
      filters: OneOrMore::new(),
      request_ack: false,
      relay_factor: 0,
      timeout: self.default_query_timeout().await,
    }
  }

  pub(crate) fn should_process_query(&self, filters: &[Bytes]) -> bool {
    for filter in filters.iter() {
      if filter.is_empty() {
        tracing::warn!("serf: empty filter");
        return false;
      }

      // Decode the filter
      let filter = match decode_filter(filter) {
        Ok((read, filter)) => {
          tracing::trace!(read=%read, filter=?filter, "serf: decoded filter successully");
          filter
        }
        Err(err) => {
          tracing::warn!(
            err = %err,
            "serf: failed to decode filter"
          );
          return false;
        }
      };

      match filter {
        Filter::Id(nodes) => {
          // Check if we are being targeted
          let found = nodes
            .iter()
            .any(|n: &T::Id| n.eq(self.inner.memberlist.local_id()));
          if !found {
            return false;
          }
        }
        Filter::Tag(tag) => {
          // Check if we match this regex
          let tags = self.inner.opts.tags.load();
          if !tags.is_empty() {
            if let Some(expr) = tags.get(&tag) {
              match regex::Regex::new(&fexpr) {
                Ok(re) => {
                  if !re.is_match(expr) {
                    return false;
                  }
                }
                Err(err) => {
                  tracing::warn!(err=%err, "serf: failed to compile filter regex ({})", fexpr);
                  return false;
                }
              }
            } else {
              return false;
            }
          } else {
            return false;
          }
        }
      }
    }
    true
  }

  pub(crate) async fn relay_response(
    &self,
    relay_factor: u8,
    node: Node<T::Id, <T::Resolver as AddressResolver>::ResolvedAddress>,
    resp: QueryResponseMessage<T::Id, <T::Resolver as AddressResolver>::ResolvedAddress>,
  ) -> Result<(), Error<T, D>> {
    if relay_factor == 0 {
      return Ok(());
    }

    // Needs to be worth it; we need to have at least relayFactor *other*
    // nodes. If you have a tiny cluster then the relayFactor shouldn't
    // be needed.
    let members = {
      let members = self.inner.members.read().await;
      if members.states.len() < relay_factor as usize + 1 {
        return Ok(());
      }
      members
        .states
        .iter()
        .filter_map(|(id, m)| {
          if m.member.status == MemberStatus::Alive && id != self.inner.memberlist.local_id() {
            Some(m.member.clone())
          } else {
            None
          }
        })
        .collect::<SmallVec<_>>()
    };

    if members.is_empty() {
      return Ok(());
    }

    // Prep the relay message, which is a wrapped version of the original.
    let encoded_len = serf_proto::Encodable::encoded_len_with_relay(&resp);
    if encoded_len > self.inner.opts.query_response_size_limit {
      return Err(Error::relayed_response_too_large(
        self.inner.opts.query_response_size_limit,
      ));
    }

    let raw = serf_proto::Encodable::encode_relay_to_bytes(&resp)?;

    // Relay to a random set of peers.
    let relay_members = random_members(relay_factor as usize, members);

    let futs: FuturesUnordered<_> = relay_members
      .into_iter()
      .map(|m| {
        let raw = raw.clone();
        async move {
          self
            .inner
            .memberlist
            .send(m.node.address(), raw)
            .await
            .map_err(|e| (m, e))
        }
      })
      .collect();

    let mut errs = TinyVec::new();
    let stream = StreamExt::filter_map(futs, |res| async move {
      if let Err((m, e)) = res {
        Some((m, e))
      } else {
        None
      }
    });
    futures::pin_mut!(stream);

    while let Some(err) = stream.next().await {
      errs.push(err);
    }

    Ok(())
  }
}
