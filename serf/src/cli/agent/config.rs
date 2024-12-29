use std::{
  collections::HashMap,
  net::{IpAddr, Ipv4Addr, SocketAddr},
  path::PathBuf,
  time::Duration,
};

use memberlist::types::SecretKey;
use serde::{Deserialize, Serialize};
use serf_core::types::ProtocolVersion;

use super::{Profile, TraceLevel};

/// The configuration for mDNS,
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct MDNSConfig {
  /// Provide a binding interface to use for mDNS.
  /// If not set. iface will be used.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub interface: Option<String>,
  /// Disable IPv4 addresses
  #[serde(default)]
  pub disable_ipv4: bool,
  /// Disable IPv6 addresses
  #[serde(default)]
  pub disable_ipv6: bool,
}

/// The configuration that can be set for an Agent. Some of these
/// configurations are exposed as command-line flags to `serf agent`, whereas
/// many of the more advanced configurations can only be set by creating
/// a configuration file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
  /// The name of the node. This must be unique in the cluster.
  #[serde(default)]
  pub node_name: String,
  /// Disable coordinates for this node.
  #[serde(default)]
  pub disable_coordinates: bool,
  /// Tags are used to attach key/value metadata to a node. They have
  /// replaced 'Role' as a more flexible meta data mechanism. For compatibility,
  /// the 'role' key is special, and is used for backwards compatibility.
  #[serde(default, skip_serializing_if = "HashMap::is_empty")]
  pub tags: HashMap<String, String>,
  /// The path to a file where Serf can store its tags. Tag
  /// persistence is desirable since tags may be set or deleted while the
  /// agent is running. Tags can be reloaded from this file on later starts.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub tags_file: Option<PathBuf>,

  /// The address that the `Serf` agent's communication ports will bind to.
  /// `Serf` will use this address to bind to for both TCP and UDP connections. Defaults to `0.0.0.0:7946`.
  #[serde(default = "default_bind_addr")]
  pub bind: SocketAddr,

  /// The address that the `Serf` agent will advertise to other members of the cluster. Can be used for basic NAT traversal where both the internal ip:port and external ip:port are known.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub advertise: Option<SocketAddr>,

  /// The secret key to use for encrypting communication
  /// traffic for Serf. If this is not specified, the
  /// traffic will not be encrypted.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub encrypt_key: Option<SecretKey>,

  /// The path to a file containing a serialized keyring.
  /// The keyring is used to facilitate encryption. If left blank, the
  /// keyring will not be persisted to a file.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub keyring_file: Option<PathBuf>,

  /// The level of the logs to output.
  /// This can be updated during a reload.
  #[serde(default)]
  pub log_level: TraceLevel,

  /// The address and port to listen on for the agent's RPC interface
  #[serde(default = "default_rpc_addr")]
  pub rpc_addr: SocketAddr,

  /// A key that can be set to optionally require that RPC's provide an authentication key.
  /// This is meant to be a very simple authentication control.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub rpc_auth_key: Option<String>,

  /// The `Serf` protocol version to use.
  #[serde(default)]
  pub protocol: ProtocolVersion,
  /// Tells `Serf` to replay past user events when joining based on
  /// a `StartJoin`.
  #[serde(default)]
  pub replay_on_join: bool,

  /// Limits the inbound payload sizes for queries.
  ///
  /// If [`NetTransport`](memberlist::net::NetTransport) is used, then
  /// must fit in a UDP packet with some additional overhead,
  /// so tuning these past the default values of `1024` will depend on your network
  /// configuration.
  #[serde(default = "default_query_response_size_limit")]
  pub query_response_size_limit: usize,
  /// Limits the outbound payload size for queries.
  ///
  /// If [`NetTransport`](memberlist::net::NetTransport) is used, then
  /// must fit in a UDP packet with some additional overhead,
  /// so tuning these past the default values of `1024` will depend on your network
  /// configuration.
  #[serde(default = "default_query_size_limit")]
  pub query_size_limit: usize,

  /// Maximum byte size limit of user event `name` + `payload` in bytes.
  /// It's optimal to be relatively small, since it's going to be gossiped through the cluster.
  #[serde(default = "default_user_event_size_limit")]
  pub user_event_size_limit: usize,

  /// A list of addresses to attempt to join when the
  /// agent starts. If `Serf` is unable to communicate with any of these
  /// addresses, then the agent will error and exit.
  #[serde(skip_serializing_if = "Vec::is_empty")]
  pub start_join: Vec<SocketAddr>,

  /// A list of event handlers that will be invoked.
  /// These can be updated during a reload.
  #[serde(skip_serializing_if = "Vec::is_empty")]
  pub event_handlers: Vec<String>,

  /// Used to select a timing profile for Serf. The supported choices
  /// are "wan", "lan", and "local". The default is "lan"
  #[serde(default)]
  pub profile: Profile,

  /// Used to allow `Serf` to snapshot important transactional
  /// state to make a more graceful recovery possible. This enables auto
  /// re-joining a cluster on failure and avoids old message replay.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub snapshot_path: Option<PathBuf>,

  /// Controls if `Serf` does a graceful leave when receiving
  /// the TERM signal. Defaults `false`. This can be changed on reload.
  #[serde(default)]
  pub leave_on_terminate: bool,

  /// Controls if `Serf` skips a graceful leave when receiving
  /// the INT signal. Defaults `false`. This can be changed on reload.
  #[serde(default)]
  pub skip_leave_on_interrupt: bool,

  /// Used to setup an mDNS Discovery name. When this is set, the
  /// agent will setup an mDNS responder and periodically run an mDNS query
  /// to look for peers. For peers on a network that supports multicast, this
  /// allows Serf agents to join each other with zero configuration.
  #[serde(default)]
  pub discover: String,

  /// The configuration for mDNS.
  pub mdns: MDNSConfig,

  /// Used to provide a binding interface to use. It can be
  /// used instead of providing a bind address, as `Serf` will discover the
  /// address of the provided interface. It is also used to set the multicast
  /// device used with `--discover`, if `mdns-iface` is not set
  pub interface: String,

  /// Reconnect interval time. This interval
  /// controls how often we attempt to connect to a failed node.
  #[serde(with = "humantime_serde::option")]
  pub reconnect_interval: Option<Duration>,

  /// Controls for how long we attempt to connect to a failed node before removing
  /// it from the cluster.
  #[serde(with = "humantime_serde::option")]
  pub reconnect_timeout: Option<Duration>,

  /// Controls for how long we remember a left node before removing it from the cluster.
  #[serde(with = "humantime_serde::option")]
  pub tombstone_timeout: Option<Duration>,

  /// By default `Serf` will attempt to resolve name conflicts. This is done by
  /// determining which node the majority believe to be the proper node, and
  /// by having the minority node shutdown. If you want to disable this behavior,
  /// then this flag can be set to true.
  #[serde(default)]
  pub disable_name_resolution: bool,

  /// Used to also tee all the logs over to syslog. Only supported
  /// on linux and OSX. Other platforms will generate an error.
  #[serde(default)]
  pub enable_sys_log: bool,

  /// Used to control which syslog facility messages are
  /// sent to. Defaults to `LOCAL0`.
  #[serde(default = "default_syslog_facility")]
  pub sys_log_facility: String,

  /// A list of addresses to attempt to join when the
  /// agent starts. `Serf` will continue to retry the join until it
  /// succeeds or [`retry_max_attempts`](Config::retry_max_attempts) is reached.
  #[serde(skip_serializing_if = "Vec::is_empty")]
  pub retry_join: Vec<SocketAddr>,

  /// Used to limit the maximum attempts made
  /// by RetryJoin to reach other nodes. If this is 0, then no limit
  /// is imposed, and Serf will continue to try forever. Defaults to `0`.
  #[serde(default)]
  pub retry_max_attempts: usize,

  /// The retry interval. This interval
  /// controls how often we retry the join for RetryJoin. This defaults
  /// to `30` seconds.
  #[serde(with = "humantime_serde", default = "default_retry_interval")]
  pub retry_interval: Duration,

  /// Controls our interaction with the snapshot file.
  /// When set to false (default), a leave causes a `Serf` to not rejoin
  /// the cluster until an explicit join is received. If this is set to
  /// true, we ignore the leave, and rejoin the cluster on start. This
  /// only has an affect if the snapshot file is enabled.
  pub rejoin_after_leave: bool,

  /// Specifies whether message compression is enabled
  /// when broadcasting events. This defaults to `false`.
  #[serde(default)]
  pub enable_compression: bool,

  /// The address of a statsite instance. If provided,
  /// metrics will be streamed to that instance.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub statsite_addr: Option<SocketAddr>,

  /// The address of a statsd instance. If provided,
  /// metrics will be sent to that instance.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub statsd_addr: Option<SocketAddr>,

  /// The broadcast interval. This interval
  /// controls the timeout for broadcast events. This defaults to
  /// `5` seconds.
  #[serde(with = "humantime_serde", default = "default_broadcast_timeout")]
  pub broadcast_timeout: Duration,
}

impl Default for Config {
  fn default() -> Self {
    Self::new()
  }
}

impl Config {
  /// Returns a new `Config` with default values.
  pub fn new() -> Self {
    Self {
      node_name: String::new(),
      disable_coordinates: false,
      tags: HashMap::new(),
      tags_file: None,
      bind: default_bind_addr(),
      advertise: None,
      encrypt_key: None,
      keyring_file: None,
      log_level: TraceLevel::default(),
      rpc_addr: default_rpc_addr(),
      rpc_auth_key: None,
      protocol: ProtocolVersion::default(),
      replay_on_join: false,
      query_response_size_limit: default_query_response_size_limit(),
      query_size_limit: default_query_size_limit(),
      user_event_size_limit: default_user_event_size_limit(),
      start_join: Vec::new(),
      event_handlers: Vec::new(),
      profile: Profile::default(),
      snapshot_path: None,
      leave_on_terminate: false,
      skip_leave_on_interrupt: false,
      discover: String::new(),
      mdns: Default::default(),
      interface: String::new(),
      reconnect_interval: None,
      reconnect_timeout: None,
      tombstone_timeout: None,
      disable_name_resolution: false,
      enable_sys_log: false,
      sys_log_facility: default_syslog_facility(),
      retry_join: Vec::new(),
      retry_max_attempts: 0,
      retry_interval: default_retry_interval(),
      rejoin_after_leave: false,
      enable_compression: false,
      statsite_addr: None,
      statsd_addr: None,
      broadcast_timeout: default_broadcast_timeout(),
    }
  }
}

const fn default_bind_addr() -> SocketAddr {
  SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 7946)
}

const fn default_rpc_addr() -> SocketAddr {
  SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 7373)
}

fn default_syslog_facility() -> String {
  "LOCAL0".to_string()
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
