#![doc = include_str!("../README.md")]
#![doc(html_logo_url = "https://raw.githubusercontent.com/al8n/serf/main/art/logo_72x72.png")]
#![forbid(unsafe_code)]
// #![deny(warnings, missing_docs)]
#![allow(clippy::type_complexity)]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![cfg_attr(docsrs, allow(unused_attributes))]

pub(crate) mod broadcast;

mod coalesce;

/// Events for [`Serf`]
pub mod event;

/// Errors for `serf`.
pub mod error;

/// Delegate traits and its implementations.
pub mod delegate;

mod options;
pub use options::*;

/// The types used in `serf`.
pub mod types;
pub use types::coordinate;

/// Secret key management.
#[cfg(feature = "encryption")]
#[cfg_attr(docsrs, doc(cfg(feature = "encryption")))]
pub mod key_manager;

mod serf;
pub use serf::*;

mod snapshot;
pub use snapshot::*;

fn invalid_data_io_error<E: std::error::Error + Send + Sync + 'static>(e: E) -> std::io::Error {
  std::io::Error::new(std::io::ErrorKind::InvalidData, e)
}

/// All unit test fns are exported in the `tests` module.
/// This module is used for users want to use other async runtime,
/// and want to use the test if memberlist also works with their runtime.
#[cfg(feature = "test")]
#[cfg_attr(docsrs, doc(cfg(feature = "test")))]
pub mod tests {
  pub use memberlist_core::tests::{AnyError, next_socket_addr_v4, next_socket_addr_v6};
  pub use paste;

  pub use super::serf::base::tests::{serf::*, *};

  /// Add `test` prefix to the predefined unit test fn with a given [`Runtime`](memberlist_core::agnostic_lite::RuntimeLite)
  #[cfg(any(feature = "test", test))]
  #[cfg_attr(docsrs, doc(cfg(any(feature = "test", test))))]
  #[macro_export]
  macro_rules! unit_tests {
    ($runtime:ty => $run:ident($($fn:ident), +$(,)?)) => {
      $(
        ::serf_core::tests::paste::paste! {
          #[test]
          fn [< test_ $fn >] () {
            $run($fn::<$runtime>());
          }
        }
      )*
    };
  }

  /// Add `test` prefix to the predefined unit test fn with a given [`Runtime`](memberlist_core::agnostic_lite::RuntimeLite)
  #[cfg(any(feature = "test", test))]
  #[cfg_attr(docsrs, doc(cfg(any(feature = "test", test))))]
  #[macro_export]
  macro_rules! unit_tests_with_expr {
    ($run:ident($(
      $(#[$outer:meta])*
      $fn:ident( $expr:expr )
    ), +$(,)?)) => {
      $(
        ::serf_core::tests::paste::paste! {
          #[test]
          $(#[$outer])*
          fn [< test_ $fn >] () {
            $run(async move {
              $expr
            });
          }
        }
      )*
    };
  }

  /// Initialize the tracing for the unit tests.
  pub fn initialize_tests_tracing() {
    use std::sync::Once;
    static TRACE: Once = Once::new();
    TRACE.call_once(|| {
      let filter = std::env::var("SERF_TESTING_LOG")
        .unwrap_or_else(|_| "serf_core=debug,memberlist_core=info".to_owned());
      memberlist_core::tracing::subscriber::set_global_default(
        tracing_subscriber::fmt::fmt()
          .without_time()
          .with_line_number(true)
          .with_env_filter(filter)
          .with_file(false)
          .with_target(true)
          .with_ansi(true)
          .finish(),
      )
      .unwrap();
    });
  }

  /// Run the unit test with a given async runtime sequentially.
  pub fn run<B, F>(block_on: B, fut: F)
  where
    B: FnOnce(F) -> F::Output,
    F: std::future::Future<Output = ()>,
  {
    // initialize_tests_tracing();
    block_on(fut);
  }
}
