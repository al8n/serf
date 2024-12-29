
use clap::Args;

use super::RpcArgs;


/// Tests the network reachability of this node
#[derive(Args, Debug)]
pub struct ReachabilityArgs {
  /// Verbose mode
  #[arg(short, long, default_value_t = false)]
  pub verbose: bool,

  /// Rpc related arguments.
  #[command(flatten)]
  pub rpc: RpcArgs,
}