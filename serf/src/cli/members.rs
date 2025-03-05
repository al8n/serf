use clap::Args;

use super::{Format, Regex, RpcArgs, parse_key_val};

/// Outputs the members of a running Serf agent.
#[derive(Args, Debug)]
pub struct MembersArgs {
  /// Additional information such as protocol verions
  /// will be shown (only affects text output format).
  #[arg(short, long, default_value_t = false)]
  pub detailed: bool,

  /// If provided, output is returned in the specified format. Valid formats are `json` and `text`
  #[arg(short, long, default_value_t = Format::Text)]
  pub format: Format,

  /// If provided, only members matching the regexp are
  /// returned. The regexp is anchored at the start and end,
  /// and must be a full match.
  #[arg(short, long)]
  pub name: Option<Regex>,

  /// If provided, output is filtered to only nodes matching
  /// the regular expression for status. Possible statuses are
  /// "none", "alive", "leaving", "left", and "failed".
  #[arg(short, long)]
  pub status: Option<Regex>,

  /// If provided, output is filtered to only nodes with the
  /// tag <key> with value matching the regular expression.
  /// tag can be specified multiple times to filter on
  /// multiple keys. The regexp is anchored at the start and end,
  /// and must be a full match.
  #[arg(short, long = "tag", value_parser = parse_key_val::<String, Regex>)]
  pub tags: Vec<(String, Regex)>,

  /// Rpc related arguments.
  #[command(flatten)]
  pub rpc: RpcArgs,
}
