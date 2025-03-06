use std::{
  collections::{HashMap, HashSet},
  net::{IpAddr, Ipv4Addr, SocketAddr},
  path::PathBuf,
  time::Duration,
};

use serf_core::types::ProtocolVersion;
use smol_str::SmolStr;

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

use super::{Profile, ToPaths};

#[cfg(feature = "serde")]
mod builder;

#[cfg(feature = "serde")]
use builder::AgentOptionsBuilder;

/// The configuration for mDNS
#[viewit::viewit(
  vis_all = "",
  getters(style = "move", vis_all = "pub"),
  setters(vis_all = "pub", prefix = "with")
)]
#[derive(Debug, Default, Clone)]
#[cfg_attr(feature = "serde", serde_with::skip_serializing_none)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct MDNSOptions {
  /// Provide a binding interface to use for mDNS.
  /// If not set. iface will be used.
  #[cfg_attr(feature = "serde", serde(default))]
  #[viewit(getter(style = "ref", const))]
  interface: SmolStr,
  /// Disable IPv4 addresses
  #[cfg_attr(feature = "serde", serde(default))]
  disable_ipv4: bool,
  /// Disable IPv6 addresses
  #[cfg_attr(feature = "serde", serde(default))]
  disable_ipv6: bool,
}

impl MDNSOptions {
  /// Returns a new `MDNSOptions` with default values.
  pub fn new() -> Self {
    Self::default()
  }

  /// Merges the given `MDNSOptions` into this one.
  pub fn merge(&mut self, other: Self) {
    if !other.interface.is_empty() {
      self.interface = other.interface;
    }

    if other.disable_ipv4 {
      self.disable_ipv4 = other.disable_ipv4;
    }

    if other.disable_ipv6 {
      self.disable_ipv6 = other.disable_ipv6;
    }
  }
}

/// The configuration that can be set for an Agent. Some of these
/// configurations are exposed as command-line flags to `serf agent`, whereas
/// many of the more advanced configurations can only be set by creating
/// a configuration file.
#[viewit::viewit(
  vis_all = "",
  getters(style = "ref", vis_all = "pub"),
  setters(vis_all = "pub", prefix = "with")
)]
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[cfg_attr(
  feature = "serde",
  serde(bound(
    serialize = "I: Serialize, A: Serialize",
    deserialize = "I: Deserialize<'de>, A: Deserialize<'de> + Eq + core::hash::Hash",
  ))
)]
pub struct AgentOptions<I, A> {
  /// The name of the node. This must be unique in the cluster.
  name: I,
  /// Disable coordinates for this node.
  #[cfg_attr(feature = "serde", serde(default))]
  disable_coordinates: bool,
  /// Tags are used to attach key/value metadata to a node. They have
  /// replaced 'Role' as a more flexible meta data mechanism. For compatibility,
  /// the 'role' key is special, and is used for backwards compatibility.
  #[cfg_attr(
    feature = "serde",
    serde(default, skip_serializing_if = "HashMap::is_empty")
  )]
  tags: HashMap<SmolStr, SmolStr>,
  /// The path to a file where Serf can store its tags. Tag
  /// persistence is desirable since tags may be set or deleted while the
  /// agent is running. Tags can be reloaded from this file on later starts.
  #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
  tags_file: Option<PathBuf>,

  /// The address that the `Serf` agent's communication ports will bind to.
  /// `Serf` will use this address to bind to for both TCP and UDP connections. Defaults to `0.0.0.0:7946`.
  #[cfg_attr(feature = "serde", serde(default = "default_bind_addr"))]
  bind: SocketAddr,

  /// The address that the `Serf` agent will advertise to other members of the cluster. Can be used for basic NAT traversal where both the internal ip:port and external ip:port are known.
  #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
  advertise: Option<A>,

  /// The address and port to listen on for the agent's RPC interface
  #[cfg_attr(feature = "serde", serde(default = "default_rpc_addr"))]
  rpc_addr: SocketAddr,

  /// A key that can be set to optionally require that RPC's provide an authentication key.
  /// This is meant to be a very simple authentication control.
  #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
  rpc_auth_key: Option<SmolStr>,

  /// The `Serf` protocol version to use.
  #[cfg_attr(feature = "serde", serde(default))]
  protocol: ProtocolVersion,
  /// Tells `Serf` to replay past user events when joining based on
  /// a `StartJoin`.
  #[cfg_attr(feature = "serde", serde(default))]
  replay_on_join: bool,

  /// Limits the inbound payload sizes for queries.
  ///
  /// If [`NetTransport`](memberlist::net::NetTransport) is used, then
  /// must fit in a UDP packet with some additional overhead,
  /// so tuning these past the default values of `1024` will depend on your network
  /// configuration.
  #[cfg_attr(
    feature = "serde",
    serde(default = "default_query_response_size_limit")
  )]
  query_response_size_limit: usize,
  /// Limits the outbound payload size for queries.
  ///
  /// If [`NetTransport`](memberlist::net::NetTransport) is used, then
  /// must fit in a UDP packet with some additional overhead,
  /// so tuning these past the default values of `1024` will depend on your network
  /// configuration.
  #[cfg_attr(feature = "serde", serde(default = "default_query_size_limit"))]
  query_size_limit: usize,

  /// Maximum byte size limit of user event `name` + `payload` in bytes.
  /// It's optimal to be relatively small, since it's going to be gossiped through the cluster.
  #[cfg_attr(feature = "serde", serde(default = "default_user_event_size_limit"))]
  user_event_size_limit: usize,

  /// A list of addresses to attempt to join when the
  /// agent starts. If `Serf` is unable to communicate with any of these
  /// addresses, then the agent will error and exit.
  #[cfg_attr(feature = "serde", serde(skip_serializing_if = "HashSet::is_empty"))]
  start_join: HashSet<A>,

  /// A list of event handlers that will be invoked.
  /// These can be updated during a reload.
  #[cfg_attr(feature = "serde", serde(skip_serializing_if = "HashSet::is_empty"))]
  event_handlers: HashSet<SmolStr>,

  /// Used to select a timing profile for Serf. The supported choices
  /// are "wan", "lan", and "local". The default is "lan"
  #[cfg_attr(feature = "serde", serde(default))]
  profile: Profile,

  /// Used to allow `Serf` to snapshot important transactional
  /// state to make a more graceful recovery possible. This enables auto
  /// re-joining a cluster on failure and avoids old message replay.
  #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
  snapshot_path: Option<PathBuf>,

  /// Controls if `Serf` does a graceful leave when receiving
  /// the TERM signal. Defaults `false`. This can be changed on reload.
  #[cfg_attr(feature = "serde", serde(default))]
  leave_on_terminate: bool,

  /// Controls if `Serf` skips a graceful leave when receiving
  /// the INT signal. Defaults `false`. This can be changed on reload.
  #[cfg_attr(feature = "serde", serde(default))]
  skip_leave_on_interrupt: bool,

  /// Used to setup an mDNS Discovery name. When this is set, the
  /// agent will setup an mDNS responder and periodically run an mDNS query
  /// to look for peers. For peers on a network that supports multicast, this
  /// allows Serf agents to join each other with zero configuration.
  #[cfg_attr(feature = "serde", serde(default))]
  discover: SmolStr,

  /// The configuration for mDNS.
  mdns: MDNSOptions,

  /// Used to provide a binding interface to use. It can be
  /// used instead of providing a bind address, as `Serf` will discover the
  /// address of the provided interface. It is also used to set the multicast
  /// device used with `--discover`, if `mdns-iface` is not set
  #[cfg_attr(feature = "serde", serde(default))]
  interface: SmolStr,

  /// Reconnect interval time. This interval
  /// controls how often we attempt to connect to a failed node.
  #[cfg_attr(feature = "serde", serde(with = "humantime_serde::option"))]
  reconnect_interval: Option<Duration>,

  /// Controls for how long we attempt to connect to a failed node before removing
  /// it from the cluster.
  #[cfg_attr(feature = "serde", serde(with = "humantime_serde::option"))]
  reconnect_timeout: Option<Duration>,

  /// Controls for how long we remember a left node before removing it from the cluster.
  #[cfg_attr(feature = "serde", serde(with = "humantime_serde::option"))]
  tombstone_timeout: Option<Duration>,

  /// By default `Serf` will attempt to resolve name conflicts. This is done by
  /// determining which node the majority believe to be the proper node, and
  /// by having the minority node shutdown. If you want to disable this behavior,
  /// then this flag can be set to true.
  #[cfg_attr(feature = "serde", serde(default))]
  disable_name_resolution: bool,

  /// Used to also tee all the logs over to syslog. Only supported
  /// on linux and OSX. Other platforms will generate an error.
  #[cfg_attr(feature = "serde", serde(default))]
  enable_sys_log: bool,

  /// Used to control which syslog facility messages are
  /// sent to. Defaults to `LOCAL0`.
  #[cfg_attr(feature = "serde", serde(default = "default_syslog_facility"))]
  sys_log_facility: SmolStr,

  /// A list of addresses to attempt to join when the
  /// agent starts. `Serf` will continue to retry the join until it
  /// succeeds or [`retry_max_attempts`](AgentOptions::retry_max_attempts) is reached.
  #[cfg_attr(
    feature = "serde",
    serde(default, skip_serializing_if = "HashSet::is_empty")
  )]
  retry_join: HashSet<A>,

  /// Used to limit the maximum attempts made
  /// by RetryJoin to reach other nodes. If this is 0, then no limit
  /// is imposed, and Serf will continue to try forever. Defaults to `0`.
  #[cfg_attr(feature = "serde", serde(default))]
  retry_max_attempts: usize,

  /// The retry interval. This interval
  /// controls how often we retry the join for RetryJoin. This defaults
  /// to `30` seconds.
  #[cfg_attr(
    feature = "serde",
    serde(with = "humantime_serde", default = "default_retry_interval")
  )]
  retry_interval: Duration,

  /// Controls our interaction with the snapshot file.
  /// When set to false (default), a leave causes a `Serf` to not rejoin
  /// the cluster until an explicit join is received. If this is set to
  /// true, we ignore the leave, and rejoin the cluster on start. This
  /// only has an affect if the snapshot file is enabled.
  #[cfg_attr(feature = "serde", serde(default))]
  rejoin_after_leave: bool,

  /// The broadcast interval. This interval
  /// controls the timeout for broadcast events. This defaults to
  /// `5` seconds.
  #[cfg_attr(
    feature = "serde",
    serde(with = "humantime_serde", default = "default_broadcast_timeout")
  )]
  broadcast_timeout: Duration,

  /// The secret key to use for encrypting communication
  /// traffic for Serf. If this is not specified, the
  /// traffic will not be encrypted.
  #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
  #[cfg(feature = "encryption")]
  #[cfg_attr(docsrs, doc(cfg(feature = "encryption")))]
  encrypt_key: Option<serf_core::types::SecretKey>,

  /// The path to a file containing a serialized keyring.
  /// The keyring is used to facilitate encryption. If left blank, the
  /// keyring will not be persisted to a file.
  #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
  #[cfg(feature = "encryption")]
  #[cfg_attr(docsrs, doc(cfg(feature = "encryption")))]
  keyring_file: Option<PathBuf>,

  /// Specifies whether message compression is enabled
  /// when broadcasting events. This defaults to `false`.
  #[cfg(any(
    feature = "lz4",
    feature = "snappy",
    feature = "zstd",
    feature = "brotli"
  ))]
  #[cfg_attr(
    docsrs,
    doc(cfg(any(
      feature = "compression",
      feature = "zlib",
      feature = "zstd",
      feature = "brotli"
    )))
  )]
  #[cfg_attr(feature = "serde", serde(default))]
  #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
  compression: Option<serf_core::types::CompressAlgorithm>,

  #[cfg(any(
    feature = "crc32",
    feature = "xxhash32",
    feature = "xxhash64",
    feature = "xxhash3",
    feature = "murmur3",
  ))]
  #[cfg_attr(
    docsrs,
    doc(cfg(any(
      feature = "crc32",
      feature = "xxhash32",
      feature = "xxhash64",
      feature = "xxhash3",
      feature = "murmur3"
    )))
  )]
  #[cfg_attr(feature = "serde", serde(default))]
  #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
  checksum: Option<serf_core::types::ChecksumAlgorithm>,

  // /// The address of a statsite instance. If provided,
  // /// metrics will be streamed to that instance.
  // #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
  // statsite_addr: Option<SocketAddr>,
  /// The address of a statsd instance. If provided,
  /// metrics will be sent to that instance.
  #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
  #[cfg(feature = "statsd")]
  #[cfg_attr(docsrs, doc(cfg(feature = "statsd")))]
  statsd_addr: Option<SocketAddr>,
}

impl<I, A> AgentOptions<I, A> {
  /// Returns a new `AgentOptions` with default values.
  pub fn new(name: I) -> Self {
    Self {
      name,
      disable_coordinates: false,
      tags: HashMap::new(),
      tags_file: None,
      bind: default_bind_addr(),
      advertise: None,
      rpc_addr: default_rpc_addr(),
      rpc_auth_key: None,
      protocol: ProtocolVersion::default(),
      replay_on_join: false,
      query_response_size_limit: default_query_response_size_limit(),
      query_size_limit: default_query_size_limit(),
      user_event_size_limit: default_user_event_size_limit(),
      start_join: HashSet::new(),
      event_handlers: HashSet::new(),
      profile: Profile::default(),
      snapshot_path: None,
      leave_on_terminate: false,
      skip_leave_on_interrupt: false,
      discover: SmolStr::default(),
      mdns: Default::default(),
      interface: SmolStr::default(),
      reconnect_interval: None,
      reconnect_timeout: None,
      tombstone_timeout: None,
      disable_name_resolution: false,
      enable_sys_log: false,
      sys_log_facility: default_syslog_facility(),
      retry_join: HashSet::new(),
      retry_max_attempts: 0,
      retry_interval: default_retry_interval(),
      rejoin_after_leave: false,
      broadcast_timeout: default_broadcast_timeout(),
      #[cfg(any(
        feature = "lz4",
        feature = "snappy",
        feature = "zstd",
        feature = "brotli"
      ))]
      compression: None,
      #[cfg(any(
        feature = "crc32",
        feature = "xxhash32",
        feature = "xxhash64",
        feature = "xxhash3",
        feature = "murmur3",
      ))]
      checksum: Some(Default::default()),
      #[cfg(feature = "encryption")]
      encrypt_key: None,
      #[cfg(feature = "encryption")]
      keyring_file: None,
      #[cfg(feature = "statsd")]
      statsd_addr: None,
    }
  }
}

const fn default_bind_addr() -> SocketAddr {
  SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 7946)
}

const fn default_rpc_addr() -> SocketAddr {
  SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 7373)
}

fn default_syslog_facility() -> SmolStr {
  "LOCAL0".into()
}

const fn default_query_response_size_limit() -> usize {
  1024
}

const fn default_query_size_limit() -> usize {
  1024
}

const fn default_user_event_size_limit() -> usize {
  512
}

const fn default_retry_interval() -> Duration {
  Duration::from_secs(30)
}

const fn default_broadcast_timeout() -> Duration {
  Duration::from_secs(5)
}

#[inline]
fn invalid_input<E>(e: E) -> std::io::Error
where
  E: Into<Box<dyn std::error::Error + Send + Sync + 'static>>,
{
  std::io::Error::new(std::io::ErrorKind::InvalidInput, e)
}
