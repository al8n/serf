use clap::Args;

use super::RpcArgs;

/// Shows recent log messages of a Serf agent, and attaches to the agent,
/// outputting log messages as they occur in real time. The monitor lets you
/// listen for log levels that may be filtered out of the Serf agent. For
/// example your agent may only be logging at INFO level, but with the monitor
/// you can see the DEBUG level logs.
#[derive(Args, Debug)]
pub struct MonitorArgs {
  /// The log level to show. This can be one of "trace", "debug", "info",
  /// "warn", "error".
  #[arg(short, long, default_value = "info")]
  pub log_level: String,

  /// Rpc related arguments.
  #[command(flatten)]
  pub rpc: RpcArgs,
}
