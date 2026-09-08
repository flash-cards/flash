//! The crate's own test support, re-exported so every test binary keeps
//! its `mod common; use common::*;`.
#![allow(unused_imports)]

pub use flash_server::testing::*;
