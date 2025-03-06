use std::{
  collections::{HashMap, HashSet},
  net::SocketAddr,
  path::PathBuf,
  time::Duration,
};

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serf_core::types::ProtocolVersion;
use smol_str::SmolStr;

#[cfg(feature = "encryption")]
use serf_core::types::SecretKey;

use super::{AgentOptions, MDNSOptions, Profile, ToPaths};

#[serde_with::skip_serializing_none]
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(bound(
  serialize = "I: Serialize, A: Serialize",
  deserialize = "I: Deserialize<'de>, A: Deserialize<'de> + Eq + core::hash::Hash",
))]
pub(super) struct AgentOptionsBuilder<I, A> {
  #[serde(default)]
  name: Option<I>,
  #[serde(default)]
  disable_coordinates: bool,
  #[serde(default, skip_serializing_if = "HashMap::is_empty")]
  tags: HashMap<SmolStr, SmolStr>,
  #[serde(default)]
  tags_file: Option<PathBuf>,
  #[serde(default)]
  bind: Option<SocketAddr>,
  #[serde(default)]
  advertise: Option<A>,

  #[serde(default)]
  rpc_addr: Option<SocketAddr>,
  #[serde(default)]
  rpc_auth_key: Option<SmolStr>,
  #[serde(default)]
  protocol: Option<ProtocolVersion>,
  #[serde(default)]
  replay_on_join: bool,
  #[serde(default)]
  query_response_size_limit: Option<usize>,
  #[serde(default)]
  query_size_limit: Option<usize>,
  #[serde(default)]
  user_event_size_limit: Option<usize>,
  #[serde(default, skip_serializing_if = "HashSet::is_empty")]
  start_join: HashSet<A>,
  #[serde(default, skip_serializing_if = "HashSet::is_empty")]
  event_handlers: HashSet<SmolStr>,
  #[serde(default)]
  profile: Option<Profile>,
  #[serde(default)]
  snapshot_path: Option<PathBuf>,
  #[serde(default)]
  leave_on_terminate: bool,
  #[serde(default)]
  skip_leave_on_interrupt: bool,
  #[serde(default)]
  discover: Option<SmolStr>,
  #[serde(default)]
  mdns: Option<MDNSOptions>,
  #[serde(default)]
  interface: Option<SmolStr>,
  #[serde(with = "humantime_serde::option")]
  reconnect_interval: Option<Duration>,
  #[serde(with = "humantime_serde::option")]
  reconnect_timeout: Option<Duration>,
  #[serde(with = "humantime_serde::option")]
  tombstone_timeout: Option<Duration>,
  #[serde(default)]
  disable_name_resolution: bool,
  #[serde(default)]
  enable_sys_log: bool,
  #[serde(default)]
  sys_log_facility: Option<SmolStr>,
  #[serde(default, skip_serializing_if = "HashSet::is_empty")]
  retry_join: HashSet<A>,
  #[serde(default)]
  retry_max_attempts: Option<usize>,
  #[serde(default, with = "humantime_serde::option")]
  retry_interval: Option<Duration>,
  #[serde(default)]
  rejoin_after_leave: bool,
  #[serde(default, with = "humantime_serde::option")]
  broadcast_timeout: Option<Duration>,
  #[serde(default)]
  #[cfg(any(
    feature = "zstd",
    feature = "brotli",
    feature = "snappy",
    feature = "lz4",
  ))]
  compression: Option<serf_core::types::CompressAlgorithm>,
  #[serde(default)]
  #[cfg(any(
    feature = "crc32",
    feature = "xxhash32",
    feature = "xxhash64",
    feature = "xxhash3",
    feature = "murmur3",
  ))]
  checksum: Option<serf_core::types::ChecksumAlgorithm>,
  #[cfg(feature = "encryption")]
  #[serde(default)]
  encrypt_key: Option<SecretKey>,
  #[cfg(feature = "encryption")]
  #[serde(default)]
  keyring_file: Option<PathBuf>,
  #[serde(default)]
  #[cfg(feature = "statsd")]
  statsd_addr: Option<SocketAddr>,
}

impl<I, A> AgentOptionsBuilder<I, A> {
  fn new() -> Self {
    Self {
      name: None,
      disable_coordinates: false,
      tags: Default::default(),
      tags_file: None,
      bind: None,
      advertise: None,
      rpc_addr: None,
      rpc_auth_key: None,
      protocol: None,
      replay_on_join: false,
      query_response_size_limit: None,
      query_size_limit: None,
      user_event_size_limit: None,
      start_join: Default::default(),
      event_handlers: Default::default(),
      profile: None,
      snapshot_path: None,
      leave_on_terminate: false,
      skip_leave_on_interrupt: false,
      discover: None,
      mdns: None,
      interface: None,
      reconnect_interval: None,
      reconnect_timeout: None,
      tombstone_timeout: None,
      disable_name_resolution: false,
      enable_sys_log: false,
      sys_log_facility: None,
      retry_join: Default::default(),
      retry_max_attempts: None,
      retry_interval: None,
      rejoin_after_leave: false,
      broadcast_timeout: None,
      #[cfg(any(
        feature = "zstd",
        feature = "brotli",
        feature = "snappy",
        feature = "lz4",
      ))]
      compression: None,
      #[cfg(any(
        feature = "crc32",
        feature = "xxhash32",
        feature = "xxhash64",
        feature = "xxhash3",
        feature = "murmur3",
      ))]
      checksum: None,
      #[cfg(feature = "encryption")]
      encrypt_key: None,
      #[cfg(feature = "encryption")]
      keyring_file: None,
      #[cfg(feature = "statsd")]
      statsd_addr: None,
    }
  }
}

impl<I, A> AgentOptionsBuilder<I, A>
where
  A: core::hash::Hash + Eq,
{
  /// Merges two configurations builder together to make a single new
  /// configuration builder.
  pub fn merge(&mut self, other: Self) {
    if let Some(name) = other.name {
      self.name = Some(name);
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
      if let Some(ref mut this_mdns) = self.mdns {
        this_mdns.merge(mdns);
      } else {
        self.mdns = Some(mdns);
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

    #[cfg(any(
      feature = "zstd",
      feature = "brotli",
      feature = "snappy",
      feature = "lz4",
    ))]
    if let Some(compression) = other.compression {
      self.compression = Some(compression);
    }

    #[cfg(any(
      feature = "crc32",
      feature = "xxhash32",
      feature = "xxhash64",
      feature = "xxhash3",
      feature = "murmur3",
    ))]
    if let Some(checksum) = other.checksum {
      self.checksum = Some(checksum);
    }

    #[cfg(feature = "encryption")]
    {
      if let Some(keyring_file) = other.keyring_file {
        self.keyring_file = Some(keyring_file);
      }

      if let Some(encrypt_key) = other.encrypt_key {
        self.encrypt_key = Some(encrypt_key);
      }
    }

    #[cfg(feature = "statsd")]
    if let Some(statsd_addr) = other.statsd_addr {
      self.statsd_addr = Some(statsd_addr);
    }

    self.event_handlers.extend(other.event_handlers);
    self.start_join.extend(other.start_join);
    self.retry_join.extend(other.retry_join);
  }

  /// Reads the paths in the given order to load configurations.
  /// The paths can be to files or directories. If the path is a directory,
  /// we read one directory deep and read any files ending in ".json", ".yaml", ".yml", ".toml", ".json5", ".ron" and ".ini" as
  /// configuration files.
  #[cfg(feature = "cli")]
  pub fn read_from_paths<P: ToPaths>(paths: P) -> std::io::Result<Self>
  where
    I: DeserializeOwned,
    A: DeserializeOwned + Eq + core::hash::Hash,
  {
    let mut config = Self::new();

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
  pub fn finalize(self, name: I) -> AgentOptions<I, A> {
    let mut config = AgentOptions::new(name);
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

    if let Some(compression) = self.compression {
      config.compression = self.compression;
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
