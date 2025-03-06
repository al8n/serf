use std::{net::SocketAddr, path::PathBuf, time::Duration};

use clap::Args;
use memberlist::net::Transport;
use serf_core::{delegate::Delegate, types::ProtocolVersion};
use smol_str::SmolStr;

use super::{super::parse_key_val, Config, Profile};

/// Starts the `Serf` agent and runs until an interrupt is received. The
/// agent represents a single node in a cluster.
///
/// Event handlers:
///
///   For more information on what event handlers are, please read the
///   Serf documentation. This section will document how to configure them
///   on the command-line. There are three methods of specifying an event
///   handler:
///
///   - The value can be a plain script, such as "event.sh". In this case,
///     Serf will send all events to this script, and you'll be responsible
///     for differentiating between them based on the SERF_EVENT.
///
///   - The value can be in the format of "TYPE=SCRIPT", such as
///     "member-join=join.sh". With this format, Serf will only send events
///     of that type to that script.
///
///   - The value can be in the format of "user:EVENT=SCRIPT", such as
///     "user:deploy=deploy.sh". This means that Serf will only invoke this
///     script in the case of user events named "deploy".
#[derive(Debug, Args)]
pub struct AgentArgs {
  /// Address to bind network listeners to. To use an IPv6
  /// address, specify [::1] or [::1]:7946.
  #[arg(short, long, default_value = "0.0.0.0:7946")]
  pub bind: SocketAddr,
  /// Network interface to bind to. Can be used instead of
  /// -bind if the interface is known but not the address.
  /// If both are provided, then Serf verifies that the
  /// interface has the bind address that is provided. This
  /// flag also sets the multicast device used for -discover,
  /// if mdns-iface is not specified.
  #[arg(short, long)]
  pub iface: Option<String>,
  /// Network interface to use for mDNS. If not provided, the
  /// -iface value is used.
  #[arg(long)]
  pub mdns_iface: Option<String>,
  /// Disable IPv4 for mDNS.
  #[arg(long)]
  pub mdns_disable_ipv4: bool,
  /// Disable IPv6 for mDNS.
  #[arg(long)]
  pub mdns_disable_ipv6: bool,
  /// Address to advertise to the other cluster members.
  #[arg(short, long)]
  pub advertise: Option<SmolStr>,
  /// Path to a JSON or Yaml file to read configuration from.
  /// This can be specified multiple times.
  #[arg(short, long = "config-file")]
  pub config_files: Vec<PathBuf>,
  /// Path to a directory to read configuration files from.
  /// This will read every file ending in ".json" or ".yml"
  /// as configuration in this directory in alphabetical order.
  #[arg(long)]
  pub config_dir: Option<PathBuf>,
  /// A cluster name used to discovery peers. On
  /// networks that support multicast, this can be used to have
  /// peers join each other without an explicit join.
  #[arg(short, long)]
  pub discover: Option<SmolStr>,
  /// Key for encrypting network traffic within Serf.
  /// Must be a base64-encoded 32-byte key.
  #[arg(long)]
  pub encrypt: Option<SmolStr>,
  /// The keyring file is used to store encryption keys used
  /// by Serf. As encryption keys are changed, the content of
  /// this file is updated so that the same keys may be used
  /// during later agent starts.
  #[arg(short, long)]
  pub keyring_file: Option<PathBuf>,
  /// Script to execute when events occur. This can
  /// be specified multiple times. See the event scripts
  /// section below for more info.
  #[arg(long = "event-handler")]
  pub event_handlers: Vec<PathBuf>,
  /// An initial agent to join with. This flag can be
  /// specified multiple times.
  #[arg(short, long = "join")]
  pub joins: Vec<SmolStr>,
  /// Name of this node. Must be unique in the cluster
  #[arg(short, long)]
  pub node: SmolStr,
  /// Profile is used to control the timing profiles used in `Serf`.
  #[arg(short, long, default_value = "lan")]
  pub profile: Profile,
  /// `Serf` protocol version to use. This defaults to the latest version,
  /// but can be set back for upgrades.
  #[arg(long, default_value = "v1")]
  pub protocol: ProtocolVersion,
  /// Ignores a previous leave and attempts to rejoin the cluster.
  /// Only works if provided along with a snapshot file.
  #[arg(short, long, default_value = "false")]
  pub rejoin: bool,
  /// An agent to join with. This flag be specified multiple times.
  /// Does not exit on failure like -join, used to retry until success.
  #[arg(long = "retry-join")]
  pub retry_joins: Vec<SmolStr>,
  /// Sets the interval on which a node will attempt to retry joining
  /// nodes provided by `--retry-join`.
  #[arg(long, default_value = "30s", value_parser = humantime::parse_duration)]
  pub retry_interval: Duration,
  /// Limits the number of retry events. `0` means unlimited.
  #[arg(long, default_value = "0")]
  pub retry_max: usize,
  /// Disable message compression for broadcasting events. Enabled by default.
  #[arg(long, default_value = "true")]
  pub disable_compression: bool,
  /// Address to bind the RPC listener.
  #[arg(long, default_value = "127.0.0.1:7373")]
  pub rpc_addr: SocketAddr,
  /// The snapshot file is used to store alive nodes and
  /// event information so that Serf can rejoin a cluster
  /// and avoid event replay on restart.
  #[arg(short, long)]
  pub snapshot: PathBuf,
  /// Tag can be specified multiple times to attach multiple key/value tag pairs to the given node.
  #[arg(short, long = "tag", value_parser = parse_key_val::<SmolStr, SmolStr>)]
  pub tags: Vec<(SmolStr, SmolStr)>,
  /// The tags file is used to persist tag data. As an agent's
  /// can be reloaded during later agent starts. This option
  /// is incompatible with the '-tag' option and requires there
  /// be no tags in the agent configuration file, if given.
  #[arg(long)]
  pub tags_file: Option<PathBuf>,
  /// When provided, logs will also be sent to syslog.
  #[arg(long, default_value = "false")]
  pub syslog: bool,
  /// Sets the broadcast timeout, which is the max time allowed for responses to events including
  /// leave and force remove messages.
  #[arg(long, default_value = "5s", value_parser = humantime::parse_duration)]
  pub broadcast_timeout: Duration,
}

impl TryFrom<AgentArgs> for Config {
  type Error = Box<dyn std::error::Error + Send + Sync + 'static>;

  fn try_from(args: AgentArgs) -> Result<Self, Self::Error> {
    let mut config = Config::default();

    for conf in args.config_files {}

    todo!()
  }
}

impl AgentArgs {
  /// Builds the agent from the arguments.
  pub async fn build<T, D>(self) -> std::io::Result<super::Agent<T, D>>
  where
    D: Delegate<Id = T::Id, Address = T::ResolvedAddress>,
    T: Transport,
  {
    todo!()
  }
}
