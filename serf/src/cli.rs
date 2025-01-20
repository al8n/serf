use std::{error::Error, path::Path, str::FromStr};

use clap::{Args, Parser, Subcommand, ValueEnum};

pub use agent::*;
pub use event::*;
pub use force_leave::*;
pub use info::*;
pub use join::*;
pub use keygen::*;
pub use keys::*;
pub use leave::*;
pub use members::*;
pub use monitor::*;
pub use query::*;
pub use reachability::*;
pub use rtt::*;
pub use tags::*;

mod agent;
mod event;
mod force_leave;
mod info;
mod join;
mod keygen;
mod keys;
mod leave;
mod members;
mod monitor;
mod query;
mod reachability;
mod rtt;
mod tags;

/// Convert to a bunch of paths.
pub trait ToPaths {
  /// Convert to a path.
  fn to_paths(&self) -> impl Iterator<Item = impl AsRef<Path>>;
}

impl<P: AsRef<Path>> ToPaths for P {
  fn to_paths(&self) -> impl Iterator<Item = impl AsRef<Path>> {
    std::iter::once(self)
  }
}

impl<P: AsRef<Path>> ToPaths for [P] {
  fn to_paths(&self) -> impl Iterator<Item = impl AsRef<Path>> {
    self.iter()
  }
}

/// A regular expression wrapper can be used as a command line argument.
#[derive(Debug, Clone)]
pub struct Regex(regex::Regex);

impl FromStr for Regex {
  type Err = regex::Error;

  #[inline]
  fn from_str(s: &str) -> Result<Self, Self::Err> {
    regex::Regex::new(s).map(Regex)
  }
}

impl core::fmt::Display for Regex {
  fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
    write!(f, "{}", self.0)
  }
}

impl From<regex::Regex> for Regex {
  fn from(re: regex::Regex) -> Self {
    Regex(re)
  }
}

impl From<Regex> for regex::Regex {
  fn from(re: Regex) -> regex::Regex {
    re.0
  }
}

/// Format of the information output
#[derive(Default, ValueEnum, Copy, Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Format {
  /// Output information in JSON format
  Json,
  /// Output information in text format
  #[default]
  Text,
}

impl core::fmt::Display for Format {
  fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
    match self {
      Format::Json => write!(f, "json"),
      Format::Text => write!(f, "text"),
    }
  }
}

/// Arguments for the RPC.
#[derive(Debug, Args)]
pub struct RpcArgs {
  /// RPC address of the `Serf` agent.
  #[arg(long = "rpc-addr", env = "SERF_RPC_ADDR", default_value_t = String::from("127.0.0.1:7373"))]
  pub rpc_addr: String,
  /// RPC auth token of the `Serf` agent.
  #[arg(long = "rpc-auth", env = "SERF_RPC_AUTH", default_value_t = String::from(""))]
  pub rpc_auth: String,
}

/// A command line interface for `Serf`.
#[derive(Parser)]
pub struct Cli {
  /// Commands for interacting with `Serf`.
  #[command(subcommand)]
  pub commands: Commands,
}

/// Commands for interacting with `Serf`.
#[derive(Debug, Subcommand)]
#[allow(missing_docs, clippy::large_enum_variant)]
pub enum Commands {
  Agent(AgentArgs),
  Info(InfoArgs),
  Event(EventArgs),
  Join(JoinArgs),
  ForceLeave(ForceLeaveArgs),
  Keygen(KeygenArgs),
  Keys(KeysArgs),
  Leave(LeaveArgs),
  Members(MembersArgs),
  Monitor(MonitorArgs),
  Query(QueryArgs),
  Reachability(ReachabilityArgs),
  Rtt(RttArgs),
  Tags(TagsArgs),
}

/// Parse a single key-value pair
fn parse_key_val<T, U>(src: &str) -> Result<(T, U), Box<dyn Error + Send + Sync + 'static>>
where
  T: std::str::FromStr,
  T::Err: Error + Send + Sync + 'static,
  U: std::str::FromStr,
  U::Err: Error + Send + Sync + 'static,
{
  let pos = src
    .find('=')
    .ok_or_else(|| format!("invalid KEY=value: no `=` found in `{src}`"))?;
  Ok((src[..pos].parse()?, src[pos + 1..].parse()?))
}
