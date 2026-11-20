//! Code that the test binaries `test_musigd` and `blackbox` share: `musigd` processes with the
//! clients of their services ([`musigd`]), and the steps of a trade ([`trade`]).
//!
//! Each binary compiles this module as a part of itself. An item that only one of them uses is
//! dead code in the other one, so it belongs in the binary that uses it.

pub mod musigd;
pub mod trade;
