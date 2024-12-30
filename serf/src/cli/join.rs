use clap::Args;

use super::RpcArgs;

/// Tells a running Serf agent (with "serf agent") to join the cluster
/// by specifying at least one existing member.
#[derive(Args, Debug)]
pub struct JoinArgs {
  /// Replay past user events.
  #[arg(short, long, default_value_t = false)]
  pub replay: bool,
  /// The address of an existing Serf agent to join.
  #[arg(short, long)]
  pub address: Vec<String>,
  /// Rpc related arguments.
  #[command(flatten)]
  pub rpc: RpcArgs,
}
