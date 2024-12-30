use clap::Args;

use super::{parse_key_val, RpcArgs};

/// Modifies tags on a running `Serf` agent.
#[derive(Args, Debug)]
pub struct TagsArgs {
  /// Creates or updates the value of a tag
  #[arg(short, long = "set", value_parser = parse_key_val::<String, String>)]
  pub sets: Vec<(String, String)>,

  /// Removes a tag, if present
  #[arg(short, long = "delete")]
  pub deletes: Vec<String>,

  /// Rpc related arguments.
  #[command(flatten)]
  pub rpc: RpcArgs,
}
