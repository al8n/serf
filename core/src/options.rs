use std::{path::PathBuf, time::Duration};

use showbiz_core::{
  humantime_serde, transport::Transport, Options as ShowbizOptions, ProtocolVersion,
};

/// The configuration for creating a Serf instance.
#[viewit::viewit(getters(vis_all = "pub"), setters(vis_all = "pub", prefix = "with"))]
#[derive(serde::Serialize, serde::Deserialize)]
pub struct Options<T: Transport> {
  /// The protocol version to speak
  protocol_version: ProtocolVersion,

  /// The amount of time to wait for a broadcast
  /// message to be sent to the cluster. Broadcast messages are used for
  /// things like leave messages and force remove messages. If this is not
  /// set, a timeout of 5 seconds will be set.
  #[serde(with = "humantime_serde")]
  broadcast_timeout: Duration,

  /// For our leave (node dead) message to propagate
  /// through the cluster. In particular, we want to stay up long enough to
  /// service any probes from other nodes before they learn about us
  /// leaving and stop probing. Otherwise, we risk getting node failures as
  /// we leave.
  #[serde(with = "humantime_serde")]
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
  #[serde(with = "humantime_serde")]
  coalesce_period: Duration,

  /// specifies the duration of time where if no events
  /// are received, coalescence immediately happens. For example, if
  /// `coalesce_period` is set to 10 seconds but `quiescent_period` is set to 2
  /// seconds, then the events will be coalesced and dispatched if no
  /// new events are received within 2 seconds of the last event. Otherwise,
  /// every event will always be delayed by at least 10 seconds.
  #[serde(with = "humantime_serde")]
  quiescent_period: Duration,

  /// The settings below relate to Serf's user event coalescing feature.
  /// The settings operate like above but only affect user messages and
  /// not the Member* messages that Serf generates.
  #[serde(with = "humantime_serde")]
  user_coalesce_period: Duration,
  /// The settings below relate to Serf's user event coalescing feature.
  /// The settings operate like above but only affect user messages and
  /// not the Member* messages that Serf generates.
  #[serde(with = "humantime_serde")]
  user_quiescent_period: Duration,

  /// The interval when the reaper runs. If this is not
  /// set (it is zero), it will be set to a reasonable default.
  #[serde(with = "humantime_serde")]
  reap_interval: Duration,

  /// The interval when we attempt to reconnect
  /// to failed nodes. If this is not set (it is zero), it will be set
  /// to a reasonable default.
  #[serde(with = "humantime_serde")]
  reconnect_interval: Duration,

  /// The amount of time to attempt to reconnect to
  /// a failed node before giving up and considering it completely gone.
  #[serde(with = "humantime_serde")]
  reconnect_timeout: Duration,

  /// The amount of time to keep around nodes
  /// that gracefully left as tombstones for syncing state with other
  /// Serf nodes.
  #[serde(with = "humantime_serde")]
  tombstone_timeout: Duration,

  /// The amount of time less than which we consider a node
  /// being failed and rejoining looks like a flap for telemetry purposes.
  /// This should be set less than a typical reboot time, but large enough
  /// to see actual events, given our expected detection times for a failed
  /// node.
  #[serde(with = "humantime_serde")]
  flap_timeout: Duration,

  /// The interval at which we check the message
  /// queue to apply the warning and max depth.
  #[serde(with = "humantime_serde")]
  queue_check_interval: Duration,

  /// Used to generate warning message if the
  /// number of queued messages to broadcast exceeds this number. This
  /// is to provide the user feedback if events are being triggered
  /// faster than they can be disseminated
  queue_depth_warning: usize,

  /// Used to start dropping messages if the number
  /// of queued messages to broadcast exceeds this number. This is to
  /// prevent an unbounded growth of memory utilization
  max_queue_depth: usize,

  /// if >0 will enforce a lower limit for dropping messages
  /// and then the max will be max(MinQueueDepth, 2*SizeOfCluster). This
  /// defaults to 0 which disables this dynamic sizing feature. If this is
  /// >0 then `max_queue_depth` will be ignored.
  min_queue_depth: usize,

  /// Used to determine how long we store recent
  /// join and leave intents. This is used to guard against the case where
  /// Serf broadcasts an intent that arrives before the Memberlist event.
  /// It is important that this not be too short to avoid continuous
  /// rebroadcasting of dead events.
  #[serde(with = "humantime_serde")]
  recent_intent_timeout: Duration,

  /// Used to control how many events are buffered.
  /// This is used to prevent re-delivery of events to a client. The buffer
  /// must be large enough to handle all "recent" events, since Serf will
  /// not deliver messages that are older than the oldest entry in the buffer.
  /// Thus if a client is generating too many events, it's possible that the
  /// buffer gets overrun and messages are not delivered.
  event_buffer_size: usize,

  /// used to control how many queries are buffered.
  /// This is used to prevent re-delivery of queries to a client. The buffer
  /// must be large enough to handle all "recent" events, since Serf will not
  /// deliver queries older than the oldest entry in the buffer.
  /// Thus if a client is generating too many queries, it's possible that the
  /// buffer gets overrun and messages are not delivered.
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
  query_timeout_mult: usize,

  /// Limit the outbound payload sizes for queries, respectively. These must fit
  /// in a UDP packet with some additional overhead, so tuning these
  /// past the default values of 1024 will depend on your network
  /// configuration.
  query_response_size_limit: usize,

  /// Limit the inbound payload sizes for queries, respectively. These must fit
  /// in a UDP packet with some additional overhead, so tuning these
  /// past the default values of 1024 will depend on your network
  /// configuration.
  query_size_limit: usize,

  /// The memberlist configuration that Serf will
  /// use to do the underlying membership management and gossip.
  #[viewit(getter(const, style = "ref"))]
  showbiz_options: ShowbizOptions<T>,

  /// If provided is used to snapshot live nodes as well
  /// as lamport clock values. When Serf is started with a snapshot,
  /// it will attempt to join all the previously known nodes until one
  /// succeeds and will also avoid replaying old user events.
  #[viewit(getter(
    const,
    style = "ref",
    result(converter(fn = "Option::as_ref"), type = "Option<&PathBuf>")
  ))]
  snapshot_path: Option<PathBuf>,

  /// Controls our interaction with the snapshot file.
  /// When set to false (default), a leave causes a Serf to not rejoin
  /// the cluster until an explicit join is received. If this is set to
  /// true, we ignore the leave, and rejoin the cluster on start.
  rejoin_after_leave: bool,

  /// Controls if Serf will actively attempt
  /// to resolve a name conflict. Since each Serf member must have a unique
  /// name, a cluster can run into issues if multiple nodes claim the same
  /// name. Without automatic resolution, Serf merely logs some warnings, but
  /// otherwise does not take any action. Automatic resolution detects the
  /// conflict and issues a special query which asks the cluster for the
  /// Name -> IP:Port mapping. If there is a simple majority of votes, that
  /// node stays while the other node will leave the cluster and exit.
  enable_name_conflict_resolution: bool,

  /// Controls if Serf will maintain an estimate of this
  /// node's network coordinate internally. A network coordinate is useful
  /// for estimating the network distance (i.e. round trip time) between
  /// two nodes. Enabling this option adds some overhead to ping messages.
  disable_coordinates: bool,

  /// Provides the location of a writable file where Serf can
  /// persist changes to the encryption keyring.
  #[viewit(getter(
    const,
    style = "ref",
    result(converter(fn = "Option::as_ref"), type = "Option<&PathBuf>")
  ))]
  keyring_file: Option<PathBuf>,

  /// Maximum byte size limit of user event `name` + `payload` in bytes.
  /// It's optimal to be relatively small, since it's going to be gossiped through the cluster.
  max_user_event_size: usize,

  // /// An optional interface which when present allows
  // /// the application to cause reaping of a node to happen when it otherwise wouldn't
  // reconnect_timeout_override: ,
  /// Controls whether nodenames only
  /// contain alphanumeric, dashes and '.'characters
  /// and sets maximum length to 128 characters
  validate_node_names: bool,
}

impl<T: Transport> Default for Options<T> {
  #[inline]
  fn default() -> Self {
    Self::new()
  }
}

impl<T: Transport> Clone for Options<T> {
  #[inline]
  fn clone(&self) -> Self {
    Self {
      showbiz_options: self.showbiz_options.clone(),
      keyring_file: self.keyring_file.clone(),
      snapshot_path: self.snapshot_path.clone(),
      ..*self
    }
  }
}

impl<T: Transport> Options<T> {
  #[inline]
  pub fn new() -> Self {
    Self {
      protocol_version: ProtocolVersion::V0,
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
      showbiz_options: ShowbizOptions::lan(),
      snapshot_path: None,
      rejoin_after_leave: false,
      enable_name_conflict_resolution: true,
      disable_coordinates: false,
      keyring_file: None,
      max_user_event_size: 512,
      validate_node_names: false,
    }
  }
}
