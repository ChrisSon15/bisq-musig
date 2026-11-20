//! Blackbox tests of the Rust side of the project, as planned in `BlackPlan.md` in this directory.
//!
//! A test plays a use case of the two users Alice and Bob. It uses the system as the users'
//! applications do: through the gRPC services of two `musigd` processes. The tests run on regtest.
//! The setup of the two users is in the module [`setup`].
//!
//! With the directory `MUSIGD_TEST_LOG_DIR` in the environment, every `musigd` process writes its
//! log to a file there, for the search for an error.

mod bt1;
mod bt2;
mod bt3;
mod bt4;
mod bt5;
mod bt6;
/// The code that these tests share with the tests of `test_musigd.rs`.
#[path = "../common/mod.rs"]
mod common;
mod setup;
mod trade;
