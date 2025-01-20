use async_channel::Sender;
use clap::ValueEnum;

pub use args::*;
pub use config::*;
use memberlist::net::{AddressResolver, Transport};
use serf_core::{delegate::Delegate, error::Error, Options, Serf};

mod args;
mod config;
mod event_handler;

/// Profile is used to control the timing profiles used in `Serf`.
#[derive(
  Debug, Default, ValueEnum, PartialEq, Eq, Clone, Copy, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum Profile {
  /// Lan is used for local area networks.
  #[default]
  Lan,
  /// Wan is used for wide area networks.
  Wan,
  /// Local is used for local.
  Local,
}

/// Tracing level is used to control the verbosity of the logs.
#[derive(
  Debug, Default, ValueEnum, PartialEq, Eq, Clone, Copy, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum TraceLevel {
  /// Trace
  Trace,
  /// Debug
  Debug,
  /// Info
  #[default]
  Info,
  /// Warn
  Warn,
  /// Error
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

/// Agent starts and manages a [`Serf`] instance, adding some niceties
/// on top of [`Serf`] such as storing logs that you can later retrieve,
/// and invoking EventHandlers when events occur.
pub struct Agent<T, D>
where
  D: Delegate<Id = T::Id, Address = <T::Resolver as AddressResolver>::ResolvedAddress>,
  T: Transport,
{
  // Stores the serf configuration
  conf: Options,

  // Stores the agent configuration
  agent_conf: Config,

  serf: Serf<T, D>,

  shutdown_tx: Sender<()>,
}

impl<T, D> Agent<T, D>
where
  D: Delegate<Id = T::Id, Address = <T::Resolver as AddressResolver>::ResolvedAddress>,
  T: Transport,
{
  // /// Creates a new agent, potentially returning an error
  // pub async fn new(
  //   agent_conf: Config,
  //   conf: Options,
  //   transport_opts: T::Options,
  // ) -> Result<Self, Error<T, D>> {
  //   // if agent_conf.enable_compression {

  //   // }

  //   todo!()
  // }
}
