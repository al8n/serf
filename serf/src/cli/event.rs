use clap::Args;

use super::RpcArgs;

/// Dispatches a custom event across the Serf cluster.
#[derive(Args, Debug)]
pub struct EventArgs {
  /// Whether this event can be coalesced. This means
  /// that repeated events of the same name within a
  /// short period of time are ignored, except the last
  /// one received.
  #[arg(short, long, default_value_t = true)]
  pub coalesce: bool,
  /// The name of the event.
  #[arg(short, long)]
  pub name: String,
  /// The payload of the event.
  #[arg(short, long)]
  pub payload: String,
  /// Rpc related arguments.
  #[command(flatten)]
  pub rpc: RpcArgs,
}
