use clap::Args;

use super::RpcArgs;

/// Estimates the round trip time between two nodes using `Serf`'s network
/// coordinate model of the cluster.
///
/// At least one node name is required. If the second node name isn't given, it
/// is set to the agent's node name. Note that these are node names as known to
/// `Serf` as "serf members" would show, not IP addresses.
#[derive(Args, Debug)]
pub struct RttArgs {
  /// The from node name.
  #[arg(short, long)]
  pub from: String,
  /// The to node name.
  #[arg(short, long)]
  pub to: String,

  /// Rpc related arguments.
  #[command(flatten)]
  pub rpc: RpcArgs,
}
