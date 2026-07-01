//! Emits the aggregate `encryption` cfg when any AEAD cipher backend feature is enabled,
//! so serf-reactor code gates on `#[cfg(encryption)]` instead of repeating the full
//! backend list.

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
