use clap::Args;
use smol_str::SmolStr;

use super::{RpcArgs, parse_key_val};

/// Modifies tags on a running `Serf` agent.
#[derive(Args, Debug)]
pub struct TagsArgs {
  /// Creates or updates the value of a tag
  #[arg(short, long = "set", value_parser = parse_key_val::<SmolStr, SmolStr>)]
  pub sets: Vec<(SmolStr, SmolStr)>,

  /// Removes a tag, if present
  #[arg(short, long = "delete")]
  pub deletes: Vec<SmolStr>,

  /// Rpc related arguments.
  #[command(flatten)]
  pub rpc: RpcArgs,
}
