// use core::future::Future;
// use serf_core::tests::run as run_unit_test;

// #[cfg(feature = "net")]
// #[path = "./main/net.rs"]
// mod net;

// #[cfg(feature = "tokio")]
// fn tokio_run(fut: impl Future<Output = ()>) {
//   let runtime = ::tokio::runtime::Builder::new_multi_thread()
//     .worker_threads(32)
//     .enable_all()
//     .build()
//     .unwrap();
//   run_unit_test(|fut| runtime.block_on(fut), fut)
// }

// #[cfg(feature = "smol")]
// fn smol_run(fut: impl Future<Output = ()>) {
//   use serf::agnostic::{RuntimeLite, smol::SmolRuntime};
//   run_unit_test(SmolRuntime::block_on, fut);
// }

// #[cfg(feature = "async-std")]
// fn async_std_run(fut: impl Future<Output = ()>) {
//   use serf::agnostic::{RuntimeLite, async_std::AsyncStdRuntime};
//   run_unit_test(AsyncStdRuntime::block_on, fut);
// }
