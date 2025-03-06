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

/// The runtime supported by the serf server.
#[derive(Debug, Default, Copy, Clone, ValueEnum, derive_more::Display)]
#[clap(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum Runtime {
  /// The `async-std` runtime.
  #[cfg(feature = "async-std")]
  #[cfg_attr(docsrs, doc(cfg(feature = "async-std")))]
  #[cfg_attr(all(feature = "async-std", not(feature = "tokio")), default)]
  #[display("async-std")]
  AsyncStd,
  /// The `tokio` runtime.
  #[cfg(feature = "tokio")]
  #[cfg_attr(docsrs, doc(cfg(feature = "tokio")))]
  #[cfg_attr(feature = "tokio", default)]
  #[display("tokio")]
  Tokio,
  /// The `smol` runtime.
  #[cfg(feature = "smol")]
  #[cfg_attr(docsrs, doc(cfg(feature = "smol")))]
  #[cfg_attr(
    all(feature = "smol", not(any(feature = "tokio", feature = "async-std"))),
    default
  )]
  #[display("smol")]
  Smol,
}

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

/// Tracing level is used to control the verbosity of the logs.
#[derive(
  Debug,
  Default,
  ValueEnum,
  PartialEq,
  Eq,
  Clone,
  Copy,
  Hash,
  serde::Serialize,
  serde::Deserialize,
  derive_more::Display,
)]
#[serde(rename_all = "snake_case")]
#[clap(rename_all = "snake_case")]
#[non_exhaustive]
pub enum TraceLevel {
  /// Trace
  #[display("trace")]
  Trace,
  /// Debug
  #[display("debug")]
  Debug,
  /// Info
  #[default]
  #[display("info")]
  Info,
  /// Warn
  #[display("warn")]
  Warn,
  /// Error
  #[display("error")]
  Error,
}

impl From<TraceLevel> for tracing::Level {
  #[inline]
  fn from(lvl: TraceLevel) -> Self {
    match lvl {
      TraceLevel::Trace => tracing::Level::TRACE,
      TraceLevel::Debug => tracing::Level::DEBUG,
      TraceLevel::Info => tracing::Level::INFO,
      TraceLevel::Warn => tracing::Level::WARN,
      TraceLevel::Error => tracing::Level::ERROR,
    }
  }
}

/// A command line interface for `Serf`.
#[derive(Parser)]
pub struct Cli {
  /// Commands for interacting with `Serf`.
  #[command(subcommand)]
  pub commands: Commands,
  /// Specify the runtime to use to execute the command.
  #[arg(short, long, default_value_t = Runtime::default())]
  pub runtime: Runtime,
  /// Log level of the agent.
  #[arg(short, long, default_value = "info")]
  pub log_level: TraceLevel,
}

impl Cli {
  /// Executes the command.
  pub fn exec(self) -> Result<(), Box<dyn Error + Send + Sync + 'static>> {
    let Self {
      commands,
      runtime,
      log_level,
    } = self;

    let filter = std::env::var("SERF_LOG").unwrap_or_else(|_| log_level.to_string());
    tracing::subscriber::set_global_default(
      tracing_subscriber::fmt::fmt()
        .without_time()
        .with_line_number(true)
        .with_env_filter(filter)
        .with_file(false)
        .with_target(true)
        .with_ansi(true)
        .finish(),
    )?;
    match runtime {
      #[cfg(feature = "async-std")]
      Runtime::AsyncStd => {
        async_std::task::block_on(commands.exec::<agnostic::async_std::AsyncStdRuntime>())
      }
      #[cfg(feature = "tokio")]
      Runtime::Tokio => tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("Failed building the Runtime")
        .block_on(commands.exec::<agnostic::tokio::TokioRuntime>()),
      #[cfg(feature = "smol")]
      Runtime::Smol => smol::block_on(commands.exec::<agnostic::smol::SmolRuntime>()),
    }
  }
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

impl Commands {
  async fn exec<R: agnostic::Runtime>(self) -> Result<(), Box<dyn Error + Send + Sync + 'static>> {
    match self {
      Self::Agent(args) => {
        todo!()
      }
      Self::Info(args) => {
        todo!()
      }
      Self::Event(args) => {
        todo!()
      }
      Self::Join(args) => {
        todo!()
      }
      Self::ForceLeave(args) => {
        todo!()
      }
      Self::Keygen(args) => {
        todo!()
      }
      Self::Keys(args) => {
        todo!()
      }
      Self::Leave(args) => {
        todo!()
      }
      Self::Members(args) => {
        todo!()
      }
      Self::Monitor(args) => {
        todo!()
      }
      Self::Query(args) => {
        todo!()
      }
      Self::Reachability(args) => {
        todo!()
      }
      Self::Rtt(args) => {
        todo!()
      }
      Self::Tags(args) => {
        todo!()
      }
    }
  }
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
