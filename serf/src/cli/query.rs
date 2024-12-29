use std::time::Duration;

use clap::Args;

use super::{Format, Regex, RpcArgs};

/// Dispatches a query to the `Serf` cluster.
#[derive(Args, Debug)]
pub struct QueryArgs {
  /// If provided, output is returned in the specified format. Valid formats are `json` and `text`
  #[arg(short, long, default_value_t = Format::Text)]
  pub format: Format,

  /// The name of the event.
  #[arg(short, long)]
  pub name: String,

  /// The payload of the event.
  #[arg(short, long)]
  pub payload: String,

  /// This flag can be provided multiple times to filter
  /// responses to only named nodes.
  #[arg(long = "node")]
  pub nodes: Vec<String>,

  /// This flag can be provided multiple times to filter
  /// responses to only nodes matching the tags.
  #[arg(short, long = "tag", value_parser = super::parse_key_val::<String, Regex>)]
  pub tags: Vec<(String, Regex)>,

  /// Providing a timeout overrides the default timeout.
  #[arg(short, long, value_parser = humantime::parse_duration, default_value = "15s")]
  pub timeout: Duration,

  /// Setting this prevents nodes from sending an acknowledgement
  /// of the query.
  #[arg(long, default_value_t = false)]
  pub no_ack: bool,

  /// If provided, query responses will be relayed through this
  /// number of extra nodes for redundancy.
  #[arg(long, default_value_t = 0)]
  pub relay_factor: u8,

  /// Rpc related arguments.
  #[command(flatten)]
  pub rpc: RpcArgs,
}
