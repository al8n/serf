//! Emits the aggregate `encryption` cfg, set when any AEAD backend feature is
//! enabled, so encryption code gates on `#[cfg(encryption)]` instead of repeating
//! the backend list. serf's gossip plane carries no compression / checksum, so
//! only the encryption aggregate is emitted (mirroring `serf-embedded`).

fn any_feature(names: &[&str]) -> bool {
  names
    .iter()
    .any(|name| std::env::var_os(format!("CARGO_FEATURE_{name}")).is_some())
}

fn main() {
  println!("cargo::rustc-check-cfg=cfg(encryption)");
  if any_feature(&["AES_GCM", "CHACHA20_POLY1305"]) {
    println!("cargo::rustc-cfg=encryption");
  }
}
