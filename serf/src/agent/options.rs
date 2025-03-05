use std::{
  collections::{HashMap, HashSet},
  net::{IpAddr, Ipv4Addr, SocketAddr},
  path::PathBuf,
  time::Duration,
};

use memberlist::net::NodeId;
use serde::{Deserialize, Serialize};
use serf_core::types::{ProtocolVersion, SecretKey};
use smol_str::SmolStr;

use super::{ToPaths, Profile};

/// The configuration for mDNS,
#[serde_with::skip_serializing_none]
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct MDNSOptions {
  /// Provide a binding IPv4 interface to use for mDNS.
  /// If not set. iface will be used.
  #[serde(default)]
  pub ifv4: Option<SmolStr>,
  /// Provide a binding IPv6 interface to use for mDNS.
  /// If not set. iface will be used.
  #[serde(default)]
  pub ifv6: Option<u32>,
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
#[serde_with::skip_serializing_none]
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct AgentOptionsBuilder {
  /// The name of the node. This must be unique in the cluster.
  #[serde(default)]
  name: Option<NodeId>,
  /// Disable coordinates for this node.
  #[serde(default)]
  disable_coordinates: bool,
  /// Tags are used to attach key/value metadata to a node. They have
  /// replaced 'Role' as a more flexible meta data mechanism. For compatibility,
  /// the 'role' key is special, and is used for backwards compatibility.
  #[serde(default, skip_serializing_if = "HashMap::is_empty")]
  tags: HashMap<SmolStr, SmolStr>,
  /// The path to a file where Serf can store its tags. Tag
  /// persistence is desirable since tags may be set or deleted while the
  /// agent is running. Tags can be reloaded from this file on later starts.
  #[serde(default)]
  tags_file: Option<PathBuf>,

  /// The address that the `Serf` agent's communication ports will bind to.
  /// `Serf` will use this address to bind to for both TCP and UDP connections. Defaults to `0.0.0.0:7946`.
  #[serde(default)]
  bind: Option<SocketAddr>,

  /// The address that the `Serf` agent will advertise to other members of the cluster. Can be used for basic NAT traversal where both the internal ip:port and external ip:port are known.
  #[serde(default)]
  advertise: Option<SocketAddr>,

  /// The secret key to use for encrypting communication
  /// traffic for Serf. If this is not specified, the
  /// traffic will not be encrypted.
  #[serde(default)]
  encrypt_key: Option<SecretKey>,

  /// The path to a file containing a serialized keyring.
  /// The keyring is used to facilitate encryption. If left blank, the
  /// keyring will not be persisted to a file.
  #[serde(default)]
  keyring_file: Option<PathBuf>,

  /// The address and port to listen on for the agent's RPC interface
  #[serde(default)]
  rpc_addr: Option<SocketAddr>,

  /// A key that can be set to optionally require that RPC's provide an authentication key.
  /// This is meant to be a very simple authentication control.
  #[serde(default)]
  rpc_auth_key: Option<SmolStr>,

  /// The `Serf` protocol version to use.
  #[serde(default)]
  protocol: Option<ProtocolVersion>,
  /// Tells `Serf` to replay past user events when joining based on
  /// a `StartJoin`.
  #[serde(default)]
  replay_on_join: bool,

  /// Limits the inbound payload sizes for queries.
  ///
  /// If [`NetTransport`](memberlist::net::NetTransport) is used, then
  /// must fit in a UDP packet with some additional overhead,
  /// so tuning these past the default values of `1024` will depend on your network
  /// configuration.
  #[serde(default)]
  query_response_size_limit: Option<usize>,
  /// Limits the outbound payload size for queries.
  ///
  /// If [`NetTransport`](memberlist::net::NetTransport) is used, then
  /// must fit in a UDP packet with some additional overhead,
  /// so tuning these past the default values of `1024` will depend on your network
  /// configuration.
  #[serde(default)]
  query_size_limit: Option<usize>,

  /// Maximum byte size limit of user event `name` + `payload` in bytes.
  /// It's optimal to be relatively small, since it's going to be gossiped through the cluster.
  #[serde(default)]
  user_event_size_limit: Option<usize>,

  /// A list of addresses to attempt to join when the
  /// agent starts. If `Serf` is unable to communicate with any of these
  /// addresses, then the agent will error and exit.
  #[serde(default, skip_serializing_if = "HashSet::is_empty")]
  start_join: HashSet<SocketAddr>,

  /// A list of event handlers that will be invoked.
  /// These can be updated during a reload.
  #[serde(default, skip_serializing_if = "HashSet::is_empty")]
  event_handlers: HashSet<SmolStr>,

  /// Used to select a timing profile for Serf. The supported choices
  /// are "wan", "lan", and "local". The default is "lan"
  #[serde(default)]
  profile: Option<Profile>,

  /// Used to allow `Serf` to snapshot important transactional
  /// state to make a more graceful recovery possible. This enables auto
  /// re-joining a cluster on failure and avoids old message replay.
  #[serde(default)]
  snapshot_path: Option<PathBuf>,

  /// Controls if `Serf` does a graceful leave when receiving
  /// the TERM signal. Defaults `false`. This can be changed on reload.
  #[serde(default)]
  leave_on_terminate: bool,

  /// Controls if `Serf` skips a graceful leave when receiving
  /// the INT signal. Defaults `false`. This can be changed on reload.
  #[serde(default)]
  skip_leave_on_interrupt: bool,

  /// Used to setup an mDNS Discovery name. When this is set, the
  /// agent will setup an mDNS responder and periodically run an mDNS query
  /// to look for peers. For peers on a network that supports multicast, this
  /// allows Serf agents to join each other with zero configuration.
  #[serde(default)]
  discover: Option<SmolStr>,

  /// The configuration for mDNS.
  mdns: Option<MDNSOptions>,

  /// Used to provide a binding interface to use. It can be
  /// used instead of providing a bind address, as `Serf` will discover the
  /// address of the provided interface. It is also used to set the multicast
  /// device used with `--discover`, if `mdns-iface` is not set
  interface: Option<SmolStr>,

  /// Reconnect interval time. This interval
  /// controls how often we attempt to connect to a failed node.
  #[serde(with = "humantime_serde::option")]
  reconnect_interval: Option<Duration>,

  /// Controls for how long we attempt to connect to a failed node before removing
  /// it from the cluster.
  #[serde(with = "humantime_serde::option")]
  reconnect_timeout: Option<Duration>,

  /// Controls for how long we remember a left node before removing it from the cluster.
  #[serde(with = "humantime_serde::option")]
  tombstone_timeout: Option<Duration>,

  /// By default `Serf` will attempt to resolve name conflicts. This is done by
  /// determining which node the majority believe to be the proper node, and
  /// by having the minority node shutdown. If you want to disable this behavior,
  /// then this flag can be set to true.
  #[serde(default)]
  disable_name_resolution: bool,

  /// Used to also tee all the logs over to syslog. Only supported
  /// on linux and OSX. Other platforms will generate an error.
  #[serde(default)]
  enable_sys_log: bool,

  /// Used to control which syslog facility messages are
  /// sent to. Defaults to `LOCAL0`.
  #[serde(default)]
  sys_log_facility: Option<SmolStr>,

  /// A list of addresses to attempt to join when the
  /// agent starts. `Serf` will continue to retry the join until it
  /// succeeds or [`retry_max_attempts`](AgentOptions::retry_max_attempts) is reached.
  #[serde(default, skip_serializing_if = "HashSet::is_empty")]
  retry_join: HashSet<SocketAddr>,

  /// Used to limit the maximum attempts made
  /// by RetryJoin to reach other nodes. If this is 0, then no limit
  /// is imposed, and Serf will continue to try forever. Defaults to `0`.
  #[serde(default)]
  retry_max_attempts: Option<usize>,

  /// The retry interval. This interval
  /// controls how often we retry the join for RetryJoin. This defaults
  /// to `30` seconds.
  #[serde(default, with = "humantime_serde::option")]
  retry_interval: Option<Duration>,

  /// Controls our interaction with the snapshot file.
  /// When set to false (default), a leave causes a `Serf` to not rejoin
  /// the cluster until an explicit join is received. If this is set to
  /// true, we ignore the leave, and rejoin the cluster on start. This
  /// only has an affect if the snapshot file is enabled.
  rejoin_after_leave: bool,

  /// Specifies whether message compression is enabled
  /// when broadcasting events. This defaults to `false`.
  #[serde(default)]
  enable_compression: bool,

  /// The address of a statsite instance. If provided,
  /// metrics will be streamed to that instance.
  #[serde(default)]
  statsite_addr: Option<SocketAddr>,

  /// The address of a statsd instance. If provided,
  /// metrics will be sent to that instance.
  #[serde(default)]
  statsd_addr: Option<SocketAddr>,

  /// The broadcast interval. This interval
  /// controls the timeout for broadcast events. This defaults to
  /// `5` seconds.
  #[serde(default, with = "humantime_serde::option")]
  broadcast_timeout: Option<Duration>,
}

impl AgentOptionsBuilder {
  /// Create a new config builder.
  #[inline]
  pub fn new() -> Self {
    Self::default()
  }

  /// Merges two configurations builder together to make a single new
  /// configuration builder.
  pub fn merge(&mut self, other: Self) {
    if let Some(name) = other.node_name {
      if !name.is_empty() {
        self.node_name = Some(name);
      }
    }

    if other.disable_coordinates {
      self.disable_coordinates = other.disable_coordinates;
    }

    self.tags.extend(other.tags);

    if let Some(bind_addr) = other.bind {
      self.bind = Some(bind_addr);
    }

    if let Some(advertise) = other.advertise {
      self.advertise = Some(advertise);
    }

    if let Some(encrypt_key) = other.encrypt_key {
      self.encrypt_key = Some(encrypt_key);
    }

    if let Some(protocol) = other.protocol {
      self.protocol = Some(protocol);
    }

    if let Some(rpc) = other.rpc_addr {
      self.rpc_addr = Some(rpc);
    }

    if let Some(rpc_auth) = other.rpc_auth_key {
      self.rpc_auth_key = Some(rpc_auth);
    }

    if other.replay_on_join {
      self.replay_on_join = other.replay_on_join;
    }

    if let Some(profile) = other.profile {
      self.profile = Some(profile);
    }

    if let Some(snapshot) = other.snapshot_path {
      self.snapshot_path = Some(snapshot);
    }

    if other.leave_on_terminate {
      self.leave_on_terminate = other.leave_on_terminate;
    }

    if other.skip_leave_on_interrupt {
      self.skip_leave_on_interrupt = other.skip_leave_on_interrupt;
    }

    if let Some(discover) = other.discover {
      if !discover.is_empty() {
        self.discover = Some(discover);
      }
    }

    if let Some(interface) = other.interface {
      if !interface.is_empty() {
        self.interface = Some(interface);
      }
    }

    if let Some(mdns) = other.mdns {
      if let Some(interface) = mdns.interface {
        let this_mdns = self.mdns.get_or_insert_with(MDNSOptions::default);
        if !interface.is_empty() {
          this_mdns.interface = Some(interface);
        }

        if mdns.disable_ipv4 {
          this_mdns.disable_ipv4 = mdns.disable_ipv4;
        }

        if mdns.disable_ipv6 {
          this_mdns.disable_ipv6 = mdns.disable_ipv6;
        }
      }
    }

    if let Some(reconnect_interval) = other.reconnect_interval {
      if !reconnect_interval.is_zero() {
        self.reconnect_interval = Some(reconnect_interval);
      }
    }

    if let Some(reconnect_timeout) = other.reconnect_timeout {
      if !reconnect_timeout.is_zero() {
        self.reconnect_timeout = Some(reconnect_timeout);
      }
    }

    if let Some(tombstone_timeout) = other.tombstone_timeout {
      if !tombstone_timeout.is_zero() {
        self.tombstone_timeout = Some(tombstone_timeout);
      }
    }

    if other.disable_name_resolution {
      self.disable_name_resolution = other.disable_name_resolution;
    }

    if let Some(tags_file) = other.tags_file {
      self.tags_file = Some(tags_file);
    }

    if let Some(keyring_file) = other.keyring_file {
      self.keyring_file = Some(keyring_file);
    }

    if other.enable_sys_log {
      self.enable_sys_log = other.enable_sys_log;
    }

    if let Some(retry_max_attempts) = other.retry_max_attempts {
      if retry_max_attempts != 0 {
        self.retry_max_attempts = Some(retry_max_attempts);
      }
    }

    if let Some(retry_interval) = other.retry_interval {
      if !retry_interval.is_zero() {
        self.retry_interval = Some(retry_interval);
      }
    }

    if other.rejoin_after_leave {
      self.rejoin_after_leave = other.rejoin_after_leave;
    }

    if let Some(syslog_facility) = other.sys_log_facility {
      self.sys_log_facility = Some(syslog_facility);
    }

    if let Some(statsite_addr) = other.statsite_addr {
      self.statsite_addr = Some(statsite_addr);
    }

    if let Some(statsd_addr) = other.statsd_addr {
      self.statsd_addr = Some(statsd_addr);
    }

    if let Some(query_response_size_limit) = other.query_response_size_limit {
      if query_response_size_limit != 0 {
        self.query_response_size_limit = Some(query_response_size_limit);
      }
    }

    if let Some(query_size_limit) = other.query_size_limit {
      if query_size_limit != 0 {
        self.query_size_limit = Some(query_size_limit);
      }
    }

    if let Some(user_event_size_limit) = other.user_event_size_limit {
      if user_event_size_limit != 0 {
        self.user_event_size_limit = Some(user_event_size_limit);
      }
    }

    if let Some(broadcast_timeout) = other.broadcast_timeout {
      if !broadcast_timeout.is_zero() {
        self.broadcast_timeout = Some(broadcast_timeout);
      }
    }

    if other.enable_compression {
      self.enable_compression = other.enable_compression;
    }

    self.event_handlers.extend(other.event_handlers);
    self.start_join.extend(other.start_join);
    self.retry_join.extend(other.retry_join);
  }

  /// Reads the paths in the given order to load configurations.
  /// The paths can be to files or directories. If the path is a directory,
  /// we read one directory deep and read any files ending in ".json", ".yaml", ".yml", ".toml", ".json5", ".ron" and ".ini" as
  /// configuration files.
  pub fn read_from_paths<P: ToPaths>(paths: P) -> std::io::Result<Self> {
    let mut config = Self::default();

    for path in paths.to_paths() {
      let path = path.as_ref();

      if path.is_file() {
        let settings = config::Config::builder()
          .add_source(config::File::from(path))
          .build()
          .map_err(invalid_input)?;

        let new_config = settings.try_deserialize::<Self>().map_err(invalid_input)?;

        config.merge(new_config);

        continue;
      }

      let contents = std::fs::read_dir(path)?;
      for entry in contents {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
          // Don't recursively read contents
          continue;
        }

        // If the extension is not a valid file format, skip it
        if !path.extension().map(check_ext).unwrap_or(false) {
          continue;
        }

        let settings = config::Config::builder()
          .add_source(config::File::from(path))
          .build()
          .map_err(invalid_input)?;

        let new_config = settings.try_deserialize::<Self>().map_err(invalid_input)?;

        config.merge(new_config);
      }
    }
    Ok(config)
  }

  /// Finalize the configuration.
  #[inline]
  pub fn finalize(self) -> AgentOptions {
    let mut config = AgentOptions::new();
    if let Some(name) = self.name {
      config.name = name;
    }

    if self.disable_coordinates {
      config.disable_coordinates = self.disable_coordinates;
    }

    config.tags.extend(self.tags);

    if let Some(tags_file) = self.tags_file {
      config.tags_file = Some(tags_file);
    }

    if let Some(bind) = self.bind {
      config.bind = bind;
    }

    if let Some(advertise) = self.advertise {
      config.advertise = Some(advertise);
    }

    if let Some(encrypt_key) = self.encrypt_key {
      config.encrypt_key = Some(encrypt_key);
    }

    if let Some(keyring_file) = self.keyring_file {
      config.keyring_file = Some(keyring_file);
    }

    if let Some(rpc_addr) = self.rpc_addr {
      config.rpc_addr = rpc_addr;
    }

    if let Some(rpc_auth_key) = self.rpc_auth_key {
      config.rpc_auth_key = Some(rpc_auth_key);
    }

    if let Some(protocol) = self.protocol {
      config.protocol = protocol;
    }

    if self.replay_on_join {
      config.replay_on_join = self.replay_on_join;
    }

    if let Some(query_response_size_limit) = self.query_response_size_limit {
      config.query_response_size_limit = query_response_size_limit;
    }

    if let Some(query_size_limit) = self.query_size_limit {
      config.query_size_limit = query_size_limit;
    }

    if let Some(user_event_size_limit) = self.user_event_size_limit {
      config.user_event_size_limit = user_event_size_limit;
    }

    config.start_join.extend(self.start_join);

    config.event_handlers.extend(self.event_handlers);

    if let Some(profile) = self.profile {
      config.profile = profile;
    }

    if let Some(snapshot_path) = self.snapshot_path {
      config.snapshot_path = Some(snapshot_path);
    }

    if self.leave_on_terminate {
      config.leave_on_terminate = self.leave_on_terminate;
    }

    if self.skip_leave_on_interrupt {
      config.skip_leave_on_interrupt = self.skip_leave_on_interrupt;
    }

    if let Some(discover) = self.discover {
      if !discover.is_empty() {
        config.discover = discover;
      }
    }

    if let Some(mdns) = self.mdns {
      config.mdns = mdns;
    }

    if let Some(interface) = self.interface {
      if !interface.is_empty() {
        config.interface = interface;
      }
    }

    if let Some(reconnect_interval) = self.reconnect_interval {
      config.reconnect_interval = Some(reconnect_interval);
    }

    if let Some(reconnect_timeout) = self.reconnect_timeout {
      config.reconnect_timeout = Some(reconnect_timeout);
    }

    if let Some(tombstone_timeout) = self.tombstone_timeout {
      config.tombstone_timeout = Some(tombstone_timeout);
    }

    if self.disable_name_resolution {
      config.disable_name_resolution = self.disable_name_resolution;
    }

    if self.enable_sys_log {
      config.enable_sys_log = self.enable_sys_log;
    }

    if let Some(sys_log_facility) = self.sys_log_facility {
      config.sys_log_facility = sys_log_facility;
    }

    config.retry_join.extend(self.retry_join);

    if let Some(retry_max_attempts) = self.retry_max_attempts {
      config.retry_max_attempts = retry_max_attempts;
    }

    if let Some(retry_interval) = self.retry_interval {
      config.retry_interval = retry_interval;
    }

    if self.rejoin_after_leave {
      config.rejoin_after_leave = self.rejoin_after_leave;
    }

    if self.enable_compression {
      config.enable_compression = self.enable_compression;
    }

    if let Some(statsite_addr) = self.statsite_addr {
      config.statsite_addr = Some(statsite_addr);
    }

    if let Some(statsd_addr) = self.statsd_addr {
      config.statsd_addr = Some(statsd_addr);
    }

    if let Some(broadcast_timeout) = self.broadcast_timeout {
      config.broadcast_timeout = broadcast_timeout;
    }

    config
  }
}

/// The configuration that can be set for an Agent. Some of these
/// configurations are exposed as command-line flags to `serf agent`, whereas
/// many of the more advanced configurations can only be set by creating
/// a configuration file.
#[viewit::viewit(vis_all = "", getters(style = "ref", vis_all = "pub"), setters(vis_all = "pub", prefix = "with"))]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentOptions {
  /// The name of the node. This must be unique in the cluster.
  name: NodeId,
  /// Disable coordinates for this node.
  #[serde(default)]
  disable_coordinates: bool,
  /// Tags are used to attach key/value metadata to a node. They have
  /// replaced 'Role' as a more flexible meta data mechanism. For compatibility,
  /// the 'role' key is special, and is used for backwards compatibility.
  #[serde(default, skip_serializing_if = "HashMap::is_empty")]
  tags: HashMap<SmolStr, SmolStr>,
  /// The path to a file where Serf can store its tags. Tag
  /// persistence is desirable since tags may be set or deleted while the
  /// agent is running. Tags can be reloaded from this file on later starts.
  #[serde(skip_serializing_if = "Option::is_none")]
  tags_file: Option<PathBuf>,

  /// The address that the `Serf` agent's communication ports will bind to.
  /// `Serf` will use this address to bind to for both TCP and UDP connections. Defaults to `0.0.0.0:7946`.
  #[serde(default = "default_bind_addr")]
  bind: SocketAddr,

  /// The address that the `Serf` agent will advertise to other members of the cluster. Can be used for basic NAT traversal where both the internal ip:port and external ip:port are known.
  #[serde(skip_serializing_if = "Option::is_none")]
  advertise: Option<SocketAddr>,

  /// The secret key to use for encrypting communication
  /// traffic for Serf. If this is not specified, the
  /// traffic will not be encrypted.
  #[serde(skip_serializing_if = "Option::is_none")]
  encrypt_key: Option<SecretKey>,

  /// The path to a file containing a serialized keyring.
  /// The keyring is used to facilitate encryption. If left blank, the
  /// keyring will not be persisted to a file.
  #[serde(skip_serializing_if = "Option::is_none")]
  #[cfg(feature = "encryption")]
  #[cfg_attr(docsrs, doc(cfg(feature = "encryption")))]
  keyring_file: Option<PathBuf>,

  /// The address and port to listen on for the agent's RPC interface
  #[serde(default = "default_rpc_addr")]
  rpc_addr: SocketAddr,

  /// A key that can be set to optionally require that RPC's provide an authentication key.
  /// This is meant to be a very simple authentication control.
  #[serde(skip_serializing_if = "Option::is_none")]
  rpc_auth_key: Option<SmolStr>,

  /// The `Serf` protocol version to use.
  #[serde(default)]
  protocol: ProtocolVersion,
  /// Tells `Serf` to replay past user events when joining based on
  /// a `StartJoin`.
  #[serde(default)]
  replay_on_join: bool,

  /// Limits the inbound payload sizes for queries.
  ///
  /// If [`NetTransport`](memberlist::net::NetTransport) is used, then
  /// must fit in a UDP packet with some additional overhead,
  /// so tuning these past the default values of `1024` will depend on your network
  /// configuration.
  #[serde(default = "default_query_response_size_limit")]
  query_response_size_limit: usize,
  /// Limits the outbound payload size for queries.
  ///
  /// If [`NetTransport`](memberlist::net::NetTransport) is used, then
  /// must fit in a UDP packet with some additional overhead,
  /// so tuning these past the default values of `1024` will depend on your network
  /// configuration.
  #[serde(default = "default_query_size_limit")]
  query_size_limit: usize,

  /// Maximum byte size limit of user event `name` + `payload` in bytes.
  /// It's optimal to be relatively small, since it's going to be gossiped through the cluster.
  #[serde(default = "default_user_event_size_limit")]
  user_event_size_limit: usize,

  /// A list of addresses to attempt to join when the
  /// agent starts. If `Serf` is unable to communicate with any of these
  /// addresses, then the agent will error and exit.
  #[serde(skip_serializing_if = "HashSet::is_empty")]
  start_join: HashSet<SocketAddr>,

  /// A list of event handlers that will be invoked.
  /// These can be updated during a reload.
  #[serde(skip_serializing_if = "HashSet::is_empty")]
  event_handlers: HashSet<SmolStr>,

  /// Used to select a timing profile for Serf. The supported choices
  /// are "wan", "lan", and "local". The default is "lan"
  #[serde(default)]
  profile: Profile,

  /// Used to allow `Serf` to snapshot important transactional
  /// state to make a more graceful recovery possible. This enables auto
  /// re-joining a cluster on failure and avoids old message replay.
  #[serde(skip_serializing_if = "Option::is_none")]
  snapshot_path: Option<PathBuf>,

  /// Controls if `Serf` does a graceful leave when receiving
  /// the TERM signal. Defaults `false`. This can be changed on reload.
  #[serde(default)]
  leave_on_terminate: bool,

  /// Controls if `Serf` skips a graceful leave when receiving
  /// the INT signal. Defaults `false`. This can be changed on reload.
  #[serde(default)]
  skip_leave_on_interrupt: bool,

  /// Used to setup an mDNS Discovery name. When this is set, the
  /// agent will setup an mDNS responder and periodically run an mDNS query
  /// to look for peers. For peers on a network that supports multicast, this
  /// allows Serf agents to join each other with zero configuration.
  #[serde(default)]
  discover: SmolStr,

  /// The configuration for mDNS.
  mdns: MDNSOptions,

  /// Used to provide a binding interface to use. It can be
  /// used instead of providing a bind address, as `Serf` will discover the
  /// address of the provided interface. It is also used to set the multicast
  /// device used with `--discover`, if `mdns-iface` is not set
  interface: SmolStr,

  /// Reconnect interval time. This interval
  /// controls how often we attempt to connect to a failed node.
  #[serde(with = "humantime_serde::option")]
  reconnect_interval: Option<Duration>,

  /// Controls for how long we attempt to connect to a failed node before removing
  /// it from the cluster.
  #[serde(with = "humantime_serde::option")]
  reconnect_timeout: Option<Duration>,

  /// Controls for how long we remember a left node before removing it from the cluster.
  #[serde(with = "humantime_serde::option")]
  tombstone_timeout: Option<Duration>,

  /// By default `Serf` will attempt to resolve name conflicts. This is done by
  /// determining which node the majority believe to be the proper node, and
  /// by having the minority node shutdown. If you want to disable this behavior,
  /// then this flag can be set to true.
  #[serde(default)]
  disable_name_resolution: bool,

  /// Used to also tee all the logs over to syslog. Only supported
  /// on linux and OSX. Other platforms will generate an error.
  #[serde(default)]
  enable_sys_log: bool,

  /// Used to control which syslog facility messages are
  /// sent to. Defaults to `LOCAL0`.
  #[serde(default = "default_syslog_facility")]
  sys_log_facility: SmolStr,

  /// A list of addresses to attempt to join when the
  /// agent starts. `Serf` will continue to retry the join until it
  /// succeeds or [`retry_max_attempts`](AgentOptions::retry_max_attempts) is reached.
  #[serde(default, skip_serializing_if = "HashSet::is_empty")]
  retry_join: HashSet<SocketAddr>,

  /// Used to limit the maximum attempts made
  /// by RetryJoin to reach other nodes. If this is 0, then no limit
  /// is imposed, and Serf will continue to try forever. Defaults to `0`.
  #[serde(default)]
  retry_max_attempts: usize,

  /// The retry interval. This interval
  /// controls how often we retry the join for RetryJoin. This defaults
  /// to `30` seconds.
  #[serde(with = "humantime_serde", default = "default_retry_interval")]
  retry_interval: Duration,

  /// Controls our interaction with the snapshot file.
  /// When set to false (default), a leave causes a `Serf` to not rejoin
  /// the cluster until an explicit join is received. If this is set to
  /// true, we ignore the leave, and rejoin the cluster on start. This
  /// only has an affect if the snapshot file is enabled.
  rejoin_after_leave: bool,

  /// Specifies whether message compression is enabled
  /// when broadcasting events. This defaults to `false`.
  #[serde(default)]
  enable_compression: bool,

  /// The address of a statsite instance. If provided,
  /// metrics will be streamed to that instance.
  #[serde(skip_serializing_if = "Option::is_none")]
  statsite_addr: Option<SocketAddr>,

  /// The address of a statsd instance. If provided,
  /// metrics will be sent to that instance.
  #[serde(skip_serializing_if = "Option::is_none")]
  statsd_addr: Option<SocketAddr>,

  /// The broadcast interval. This interval
  /// controls the timeout for broadcast events. This defaults to
  /// `5` seconds.
  #[serde(with = "humantime_serde", default = "default_broadcast_timeout")]
  broadcast_timeout: Duration,
}

impl Default for AgentOptions {
  fn default() -> Self {
    Self::new()
  }
}

impl AgentOptions {
  /// Returns a new `AgentOptions` with default values.
  pub fn new() -> Self {
    Self {
      name: SmolStr::default(),
      disable_coordinates: false,
      tags: HashMap::new(),
      tags_file: None,
      bind: default_bind_addr(),
      advertise: None,
      encrypt_key: None,
      keyring_file: None,
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

#[inline]
fn check_ext(ext: &std::ffi::OsStr) -> bool {
  // TOML, JSON, YAML, INI, RON, JSON5
  ext == "toml"
    || ext == "json"
    || ext == "yaml"
    || ext == "yml"
    || ext == "ini"
    || ext == "ron"
    || ext == "json5"
}
