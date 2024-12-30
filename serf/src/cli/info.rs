use clap::Args;

use super::{Format, RpcArgs};

/// Provides debugging information for operators
#[derive(Args, Debug)]
pub struct InfoArgs {
  /// If provided, output is returned in the specified format. Valid formats are `json` and `text` (default)
  #[arg(short, long, default_value_t = Format::Text)]
  pub format: Format,
  /// Rpc related arguments.
  #[command(flatten)]
  pub rpc: RpcArgs,
}
