use std::{path::PathBuf, sync::Arc, time::Duration};

use arc_swap::ArcSwap;
pub use memberlist_core::Options as MemberlistOptions;
use smol_str::SmolStr;

use super::types::{DelegateVersion, ProtocolVersion, Tags};

fn tags(tags: &Arc<ArcSwap<Tags>>) -> Arc<Tags> {
  tags.load().clone()
}

/// The configuration for creating a Serf instance.
#[viewit::viewit(getters(vis_all = "pub"), setters(vis_all = "pub", prefix = "with"))]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Options {
  /// The tags for this role, if any. This is used to provide arbitrary
  /// key/value metadata per-node. For example, a "role" tag may be used to
  /// differentiate "load-balancer" from a "web" role as parts of the same cluster.
  /// Tags are deprecating 'Role', and instead it acts as a special key in this
  /// map.
  #[viewit(
    vis = "pub(crate)",
    getter(
      vis = "pub",
      style = "ref",
      result(converter(style = "ref", fn = "tags",), type = "Arc<Tags>",),
      attrs(
        doc = "Returns the tags for this role, if any. This is used to provide arbitrary key/value metadata per-node. For example, a \"role\" tag may be used to differentiate \"load-balancer\" from a \"web\" role as parts of the same cluster."
      )
    ),
    setter(skip)
  )]
  #[cfg_attr(feature = "serde", serde(with = "tags_serde"))]
  tags: Arc<ArcSwap<Tags>>,

  /// The protocol version to speak
  #[viewit(
    getter(const, attrs(doc = "Returns the protocol version to speak")),
    setter(attrs(doc = "Sets the protocol version to speak"))
  )]
  protocol_version: ProtocolVersion,

  /// The delegate version to speak
  #[viewit(
    getter(const, attrs(doc = "Returns the delegate version to speak")),
    setter(attrs(doc = "Sets the delegate version to speak"))
  )]
  delegate_version: DelegateVersion,

  /// The amount of time to wait for a broadcast
  /// message to be sent to the cluster. Broadcast messages are used for
  /// things like leave messages and force remove messages. If this is not
  /// set, a timeout of 5 seconds will be set.
  #[cfg_attr(feature = "serde", serde(with = "humantime_serde"))]
  #[viewit(
    getter(
      const,
      attrs(
        doc = "Returns the amount of time to wait for a broadcast message to be sent to the cluster."
      )
    ),
    setter(attrs(
      doc = "Sets the amount of time to wait for a broadcast message to be sent to the cluster."
    ))
  )]
  broadcast_timeout: Duration,

  /// For our leave (node dead) message to propagate
  /// through the cluster. In particular, we want to stay up long enough to
  /// service any probes from other nodes before they learn about us
  /// leaving and stop probing. Otherwise, we risk getting node failures as
  /// we leave.
  #[cfg_attr(feature = "serde", serde(with = "humantime_serde"))]
  #[viewit(
    getter(const, attrs(doc = "Returns the leave propagate delay.")),
    setter(attrs(doc = "Sets the leave propagate delay."))
  )]
  leave_propagate_delay: Duration,

  /// The settings below relate to Serf's event coalescence feature. Serf
  /// is able to coalesce multiple events into single events in order to
  /// reduce the amount of noise that is sent along the event channel. For example
  /// if five nodes quickly join, the event channel will be sent one EventMemberJoin
  /// containing the five nodes rather than five individual EventMemberJoin
  /// events. Coalescence can mitigate potential flapping behavior.
  ///
  /// Coalescence is disabled by default and can be enabled by setting
  /// `coalesce_period`.
  ///
  /// `coalesce_period` specifies the time duration to coalesce events.
  /// For example, if this is set to 5 seconds, then all events received
  /// within 5 seconds that can be coalesced will be.
  ///
  #[cfg_attr(feature = "serde", serde(with = "humantime_serde"))]
  #[viewit(
    getter(const, attrs(doc = "Returns the coalesce period.")),
    setter(attrs(doc = "Sets the coalesce period."))
  )]
  coalesce_period: Duration,

  /// specifies the duration of time where if no events
  /// are received, coalescence immediately happens. For example, if
  /// `coalesce_period` is set to 10 seconds but `quiescent_period` is set to 2
  /// seconds, then the events will be coalesced and dispatched if no
  /// new events are received within 2 seconds of the last event. Otherwise,
  /// every event will always be delayed by at least 10 seconds.
  #[cfg_attr(feature = "serde", serde(with = "humantime_serde"))]
  #[viewit(
    getter(
      const,
      attrs(
        doc = "Returns the specifies the duration of time where if no events are received, coalescence immediately happens."
      )
    ),
    setter(attrs(
      doc = "Sets specifies the duration of time where if no events are received, coalescence immediately happens."
    ))
  )]
  quiescent_period: Duration,

  /// The settings below relate to Serf's user event coalescing feature.
  /// The settings operate like above but only affect user messages and
  /// not the Member* messages that Serf generates.
  #[cfg_attr(feature = "serde", serde(with = "humantime_serde"))]
  #[viewit(
    getter(const, attrs(doc = "Returns the user event coalesce period.")),
    setter(attrs(doc = "Sets the user event coalesce period."))
  )]
  user_coalesce_period: Duration,

  /// The settings below relate to Serf's user event coalescing feature.
  /// The settings operate like above but only affect user messages and
  /// not the Member* messages that Serf generates.
  #[cfg_attr(feature = "serde", serde(with = "humantime_serde"))]
  #[viewit(
    getter(const, attrs(doc = "Returns the user quiescent period.")),
    setter(attrs(doc = "Sets the user quiescent period."))
  )]
  user_quiescent_period: Duration,

  /// The interval when the reaper runs. If this is not
  /// set (it is zero), it will be set to a reasonable default.
  #[cfg_attr(feature = "serde", serde(with = "humantime_serde"))]
  #[viewit(
    getter(const, attrs(doc = "Returns the interval when the reaper runs.")),
    setter(attrs(doc = "Sets the interval when the reaper runs."))
  )]
  reap_interval: Duration,

  /// The interval when we attempt to reconnect
  /// to failed nodes. If this is not set (it is zero), it will be set
  /// to a reasonable default.
  #[cfg_attr(feature = "serde", serde(with = "humantime_serde"))]
  #[viewit(
    getter(
      const,
      attrs(doc = "Returns the interval when we attempt to reconnect to failed nodes.")
    ),
    setter(attrs(doc = "Sets the interval when we attempt to reconnect to failed nodes."))
  )]
  reconnect_interval: Duration,

  /// The amount of time to attempt to reconnect to
  /// a failed node before giving up and considering it completely gone.
  #[cfg_attr(feature = "serde", serde(with = "humantime_serde"))]
  #[viewit(
    getter(
      const,
      attrs(
        doc = "Returns the amount of time to attempt to reconnect to a failed node before giving up and considering it completely gone."
      )
    ),
    setter(attrs(
      doc = "Sets the amount of time to attempt to reconnect to a failed node before giving up and considering it completely gone."
    ))
  )]
  reconnect_timeout: Duration,

  /// The amount of time to keep around nodes
  /// that gracefully left as tombstones for syncing state with other
  /// Serf nodes.
  #[cfg_attr(feature = "serde", serde(with = "humantime_serde"))]
  #[viewit(
    getter(
      const,
      attrs(
        doc = "Returns the amount of time to keep around nodes that gracefully left as tombstones for syncing state with other Serf nodes."
      )
    ),
    setter(attrs(
      doc = "Sets the amount of time to keep around nodes that gracefully left as tombstones for syncing state with other Serf nodes."
    ))
  )]
  tombstone_timeout: Duration,

  /// The amount of time less than which we consider a node
  /// being failed and rejoining looks like a flap for telemetry purposes.
  /// This should be set less than a typical reboot time, but large enough
  /// to see actual events, given our expected detection times for a failed
  /// node.
  #[cfg_attr(feature = "serde", serde(with = "humantime_serde"))]
  #[viewit(
    getter(
      const,
      attrs(
        doc = "Returns the amount of time less than which we consider a node being failed and rejoining looks like a flap for telemetry purposes."
      )
    ),
    setter(attrs(
      doc = "Sets the amount of time less than which we consider a node being failed and rejoining looks like a flap for telemetry purposes."
    ))
  )]
  flap_timeout: Duration,

  /// The interval at which we check the message
  /// queue to apply the warning and max depth.
  #[cfg_attr(feature = "serde", serde(with = "humantime_serde"))]
  #[viewit(
    getter(
      const,
      attrs(
        doc = "Returns the interval at which we check the message queue to apply the warning and max depth."
      )
    ),
    setter(attrs(
      doc = "Sets the interval at which we check the message queue to apply the warning and max depth."
    ))
  )]
  queue_check_interval: Duration,

  /// Used to generate warning message if the
  /// number of queued messages to broadcast exceeds this number. This
  /// is to provide the user feedback if events are being triggered
  /// faster than they can be disseminated
  #[viewit(
    getter(const, attrs(doc = "Returns the queue depth warning.")),
    setter(attrs(doc = "Sets the queue depth warning."))
  )]
  queue_depth_warning: usize,

  /// Used to start dropping messages if the number
  /// of queued messages to broadcast exceeds this number. This is to
  /// prevent an unbounded growth of memory utilization
  #[viewit(
    getter(const, attrs(doc = "Returns the max queue depth.")),
    setter(attrs(doc = "Sets the max queue depth."))
  )]
  max_queue_depth: usize,

  /// if >0 will enforce a lower limit for dropping messages
  /// and then the max will be max(MinQueueDepth, 2*SizeOfCluster). This
  /// defaults to 0 which disables this dynamic sizing feature. If this is
  /// >0 then `max_queue_depth` will be ignored.
  #[viewit(
    getter(
      const,
      attrs(
        doc = "Returns if `>0` will enforce a lower limit for dropping messages and then the max will be `max(min_queue_depth, 2 * size_of_cluster)`. This defaults to 0 which disables this dynamic sizing feature. If this is `>0` then `max_queue_depth` will be ignored."
      )
    ),
    setter(attrs(
      doc = "Sets if `>0` will enforce a lower limit for dropping messages and then the max will be `max(min_queue_depth, 2 * size_of_cluster)`. This defaults to 0 which disables this dynamic sizing feature. If this is `>0` then `max_queue_depth` will be ignored."
    ))
  )]
  min_queue_depth: usize,

  /// Used to determine how long we store recent
  /// join and leave intents. This is used to guard against the case where
  /// Serf broadcasts an intent that arrives before the Memberlist event.
  /// It is important that this not be too short to avoid continuous
  /// rebroadcasting of dead events.
  #[cfg_attr(feature = "serde", serde(with = "humantime_serde"))]
  #[viewit(
    getter(
      const,
      attrs(doc = "Returns how long we store recent join and leave intents.")
    ),
    setter(attrs(doc = "Sets how long we store recent join and leave intents."))
  )]
  recent_intent_timeout: Duration,

  /// Used to control how many events are buffered.
  /// This is used to prevent re-delivery of events to a client. The buffer
  /// must be large enough to handle all "recent" events, since Serf will
  /// not deliver messages that are older than the oldest entry in the buffer.
  /// Thus if a client is generating too many events, it's possible that the
  /// buffer gets overrun and messages are not delivered.
  #[viewit(
    getter(const, attrs(doc = "Returns how many events are buffered.")),
    setter(attrs(doc = "Sets how many events are buffered."))
  )]
  event_buffer_size: usize,

  /// used to control how many queries are buffered.
  /// This is used to prevent re-delivery of queries to a client. The buffer
  /// must be large enough to handle all "recent" events, since Serf will not
  /// deliver queries older than the oldest entry in the buffer.
  /// Thus if a client is generating too many queries, it's possible that the
  /// buffer gets overrun and messages are not delivered.
  #[viewit(
    getter(const, attrs(doc = "Returns how many queries are buffered.")),
    setter(attrs(doc = "Sets how many queries are buffered."))
  )]
  query_buffer_size: usize,

  /// Configures the default timeout multipler for a query to run if no
  /// specific value is provided. Queries are real-time by nature, where the
  /// reply is time sensitive. As a result, results are collected in an async
  /// fashion, however the query must have a bounded duration. We want the timeout
  /// to be long enough that all nodes have time to receive the message, run a handler,
  /// and generate a reply. Once the timeout is exceeded, any further replies are ignored.
  /// The default value is
  ///
  /// ```text
  /// timeout = gossip_interval * query_timeout_mult * log(N+1)
  /// ```
  #[viewit(
    getter(
      const,
      attrs(
        doc = "Returns the default timeout multipler for a query to run if no specific value is provided."
      )
    ),
    setter(attrs(
      doc = "Sets the default timeout multipler for a query to run if no specific value is provided."
    ))
  )]
  query_timeout_mult: usize,

  /// Limit the outbound payload sizes for queries, respectively. These must fit
  /// in a UDP packet with some additional overhead, so tuning these
  /// past the default values of 1024 will depend on your network
  /// configuration.
  #[viewit(
    getter(
      const,
      attrs(doc = "Returns the limit of the outbound payload sizes for queries.")
    ),
    setter(attrs(doc = "Sets the limit of the outbound payload sizes for queries."))
  )]
  query_response_size_limit: usize,

  /// Limit the inbound payload sizes for queries, respectively. These must fit
  /// in a UDP packet with some additional overhead, so tuning these
  /// past the default values of 1024 will depend on your network
  /// configuration.
  #[viewit(
    getter(
      const,
      attrs(doc = "Returns the limit of the inbound payload sizes for queries.")
    ),
    setter(attrs(doc = "Sets the limit of the inbound payload sizes for queries."))
  )]
  query_size_limit: usize,

  /// The memberlist configuration that Serf will
  /// use to do the underlying membership management and gossip.
  #[viewit(
    getter(
      const,
      style = "ref",
      attrs(
        doc = "Returns the memberlist configuration that Serf will use to do the underlying membership management and gossip."
      )
    ),
    setter(attrs(
      doc = "Sets the memberlist configuration that Serf will use to do the underlying membership management and gossip."
    ))
  )]
  memberlist_options: MemberlistOptions,

  /// If provided is used to snapshot live nodes as well
  /// as lamport clock values. When Serf is started with a snapshot,
  /// it will attempt to join all the previously known nodes until one
  /// succeeds and will also avoid replaying old user events.
  #[viewit(
    getter(
      const,
      style = "ref",
      result(converter(fn = "Option::as_ref"), type = "Option<&PathBuf>"),
      attrs(doc = "Returns the path to the snapshot file.")
    ),
    setter(attrs(doc = "Sets the path to the snapshot file."))
  )]
  snapshot_path: Option<PathBuf>,

  /// Controls our interaction with the snapshot file.
  /// When set to false (default), a leave causes a Serf to not rejoin
  /// the cluster until an explicit join is received. If this is set to
  /// true, we ignore the leave, and rejoin the cluster on start.
  #[viewit(
    getter(
      const,
      attrs(doc = "Returns if Serf will rejoin the cluster after a leave.")
    ),
    setter(attrs(doc = "Sets if Serf will rejoin the cluster after a leave."))
  )]
  rejoin_after_leave: bool,

  /// Controls if Serf will actively attempt
  /// to resolve a name conflict. Since each Serf member must have a unique
  /// name, a cluster can run into issues if multiple nodes claim the same
  /// name. Without automatic resolution, Serf merely logs some warnings, but
  /// otherwise does not take any action. Automatic resolution detects the
  /// conflict and issues a special query which asks the cluster for the
  /// Name -> IP:Port mapping. If there is a simple majority of votes, that
  /// node stays while the other node will leave the cluster and exit.
  #[viewit(
    getter(
      const,
      attrs(doc = "Returns if Serf will attempt to resolve a name conflict.")
    ),
    setter(attrs(doc = "Sets if Serf will attempt to resolve a name conflict."))
  )]
  enable_id_conflict_resolution: bool,

  /// Controls if Serf will maintain an estimate of this
  /// node's network coordinate internally. A network coordinate is useful
  /// for estimating the network distance (i.e. round trip time) between
  /// two nodes. Enabling this option adds some overhead to ping messages.
  #[viewit(
    getter(
      const,
      attrs(
        doc = "Returns if Serf will maintain an estimate of this node's network coordinate internally."
      )
    ),
    setter(attrs(
      doc = "Sets if Serf will maintain an estimate of this node's network coordinate internally."
    ))
  )]
  disable_coordinates: bool,

  /// Provides the location of a writable file where Serf can
  /// persist changes to the encryption keyring.
  #[cfg(feature = "encryption")]
  #[viewit(
    getter(
      const,
      style = "ref",
      result(converter(fn = "Option::as_ref"), type = "Option<&PathBuf>"),
      attrs(
        doc = "Returns the location of a writable file where Serf can persist changes to the encryption keyring.",
        cfg(feature = "encryption")
      )
    ),
    setter(attrs(
      doc = "Sets the location of a writable file where Serf can persist changes to the encryption keyring.",
      cfg(feature = "encryption")
    ))
  )]
  keyring_file: Option<PathBuf>,

  /// Maximum byte size limit of user event `name` + `payload` in bytes.
  /// It's optimal to be relatively small, since it's going to be gossiped through the cluster.
  #[viewit(
    getter(
      const,
      attrs(
        doc = "Returns the maximum byte size limit of user event `name` + `payload` in bytes."
      )
    ),
    setter(attrs(
      doc = "Sets the maximum byte size limit of user event `name` + `payload` in bytes."
    ))
  )]
  max_user_event_size: usize,
}

impl Default for Options {
  #[inline]
  fn default() -> Self {
    Self::new()
  }
}

impl Clone for Options {
  #[inline]
  fn clone(&self) -> Self {
    Self {
      memberlist_options: self.memberlist_options.clone(),
      #[cfg(feature = "encryption")]
      keyring_file: self.keyring_file.clone(),
      snapshot_path: self.snapshot_path.clone(),
      tags: self.tags.clone(),
      ..*self
    }
  }
}

impl Options {
  /// Returns a new instance of `Options` with default configurations.
  #[inline]
  pub fn new() -> Self {
    Self {
      tags: Arc::new(ArcSwap::from_pointee(Tags::default())),
      protocol_version: ProtocolVersion::V1,
      delegate_version: DelegateVersion::V1,
      broadcast_timeout: Duration::from_secs(5),
      leave_propagate_delay: Duration::from_secs(1),
      coalesce_period: Duration::ZERO,
      quiescent_period: Duration::ZERO,
      user_coalesce_period: Duration::ZERO,
      user_quiescent_period: Duration::ZERO,
      reap_interval: Duration::from_secs(15),
      reconnect_interval: Duration::from_secs(30),
      reconnect_timeout: Duration::from_secs(3600 * 24),
      tombstone_timeout: Duration::from_secs(3600 * 24),
      flap_timeout: Duration::from_secs(60),
      queue_check_interval: Duration::from_secs(30),
      queue_depth_warning: 128,
      max_queue_depth: 4096,
      min_queue_depth: 0,
      recent_intent_timeout: Duration::from_secs(60 * 5),
      event_buffer_size: 512,
      query_buffer_size: 512,
      query_timeout_mult: 16,
      query_response_size_limit: 1024,
      query_size_limit: 1024,
      memberlist_options: MemberlistOptions::lan(),
      snapshot_path: None,
      rejoin_after_leave: false,
      enable_id_conflict_resolution: true,
      disable_coordinates: false,
      #[cfg(feature = "encryption")]
      keyring_file: None,
      max_user_event_size: 512,
    }
  }

  /// Sets the tags for this node.
  #[inline]
  pub fn with_tags<K: Into<SmolStr>, V: Into<SmolStr>>(
    self,
    tags: impl Iterator<Item = (K, V)>,
  ) -> Self {
    self
      .tags
      .store(Arc::new(tags.map(|(k, v)| (k.into(), v.into())).collect()));
    self
  }

  #[inline]
  pub(crate) fn queue_opts(&self) -> QueueOptions {
    QueueOptions {
      max_queue_depth: self.max_queue_depth,
      min_queue_depth: self.min_queue_depth,
      check_interval: self.queue_check_interval,
      depth_warning: self.queue_depth_warning,
      #[cfg(feature = "metrics")]
      metric_labels: self.memberlist_options.metric_labels().clone(),
    }
  }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct QueueOptions {
  pub(crate) max_queue_depth: usize,
  pub(crate) min_queue_depth: usize,
  pub(crate) check_interval: Duration,
  pub(crate) depth_warning: usize,
  #[cfg(feature = "metrics")]
  pub(crate) metric_labels: Arc<memberlist_core::proto::MetricLabels>,
}

#[cfg(feature = "serde")]
mod tags_serde {
  use std::sync::Arc;

  use arc_swap::ArcSwap;
  use serde::{Deserialize, Deserializer, Serialize, Serializer};

  use crate::types::Tags;

  pub fn serialize<S>(tags: &Arc<ArcSwap<Tags>>, serializer: S) -> Result<S::Ok, S::Error>
  where
    S: Serializer,
  {
    let tags = tags.load();
    Tags::serialize(&**tags, serializer)
  }

  pub fn deserialize<'de, D>(deserializer: D) -> Result<Arc<ArcSwap<Tags>>, D::Error>
  where
    D: Deserializer<'de>,
  {
    Tags::deserialize(deserializer).map(|map| Arc::new(ArcSwap::from_pointee(map)))
  }
}
