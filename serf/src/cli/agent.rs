use clap::ValueEnum;

pub use args::*;
pub use config::*;

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

/// Agent starts and manages a `Serf` instance, adding some niceties
/// on top of `Serf` such as storing logs that you can later retrieve,
/// and invoking EventHandlers when events occur.
pub struct Agent {

}