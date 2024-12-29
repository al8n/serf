use clap::Args;

use super::RpcArgs;

/// Causes the agent to gracefully leave the Serf cluster and shutdown.
#[derive(Args, Debug)]
pub struct LeaveArgs {
  /// Rpc related arguments.
  #[command(flatten)]
  pub rpc: RpcArgs,
}
