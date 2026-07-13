//! Floating-point transcendentals (`log10`/`ceil`): routed through `std` on std
//! targets and `libm` on no_std, since `core` has no float math. Centralised so
//! std builds keep bit-matching the historical std results the timing tests pin,
//! while no_std gets a pure-Rust implementation.

#[cfg(feature = "std")]
#[inline]
pub(crate) fn log10(x: f64) -> f64 {
  x.log10()
}
#[cfg(not(feature = "std"))]
#[inline]
pub(crate) fn log10(x: f64) -> f64 {
  libm::log10(x)
}

#[cfg(feature = "std")]
#[inline]
pub(crate) fn ceil(x: f64) -> f64 {
  x.ceil()
}
#[cfg(not(feature = "std"))]
#[inline]
pub(crate) fn ceil(x: f64) -> f64 {
  libm::ceil(x)
}
