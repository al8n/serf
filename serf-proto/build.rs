//! Build-time codegen for the `messages` module.
//!
//! Uses `buffa-build` to invoke `protoc` against
//! `proto/serf/v1/messages.proto` and produce Rust types via
//! `buffa-codegen`. The output is written to `OUT_DIR` and pulled in by
//! `src/messages/mod.rs` via `include!`.

fn main() {
  println!("cargo:rerun-if-changed=build.rs");
  println!("cargo:rerun-if-changed=proto");

  buffa_build::Config::new()
    .files(&["proto/serf/v1/messages.proto"])
    .includes(&["proto"])
    .use_bytes_type()
    .include_file("serf_wire_generated.rs")
    .compile()
    .expect("buffa codegen failed");
}
