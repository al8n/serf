use async_channel::Sender;
use clap::ValueEnum;

pub use args::*;
pub use config::*;
use memberlist::net::Transport;
use serf_core::{Options, Serf, delegate::Delegate};

mod args;
mod config;

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

/// Agent starts and manages a [`Serf`] instance, adding some niceties
/// on top of [`Serf`] such as storing logs that you can later retrieve,
/// and invoking EventHandlers when events occur.
pub struct Agent<T, D>
where
  D: Delegate<Id = T::Id, Address = T::ResolvedAddress>,
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
  D: Delegate<Id = T::Id, Address = T::ResolvedAddress>,
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
