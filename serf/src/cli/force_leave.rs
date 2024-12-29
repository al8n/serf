use clap::Args;

use super::RpcArgs;


/// Forces a member of a Serf cluster to enter the "left" state. Note
/// that if the member is still actually alive, it will eventually rejoin
/// the cluster. This command is most useful for cleaning out "failed" nodes
/// that are never coming back. If you do not force leave a failed node,
/// Serf will attempt to reconnect to those failed nodes for some period of
/// time before eventually reaping them.
#[derive(Args, Debug)]
pub struct ForceLeaveArgs {
  /// Remove agent forcibly from list of members.
  #[arg(short, long, default_value_t = false)]
  pub prune: bool,
  /// The name of the event.
  #[arg(short, long)]
  pub name: String,
  /// Rpc related arguments.
  #[command(flatten)]
  pub rpc: RpcArgs,
}

