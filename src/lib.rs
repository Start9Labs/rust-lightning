#![crate_name = "lightning"]

//! Rust-Lightning, not Rusty's Lightning!
//!
//! A full-featured but also flexible lightning implementation, in library form. This allows the
//! user (you) to decide how they wish to use it instead of being a fully self-contained daemon.
//! This means there is no built-in threading/execution environment and it's up to the user to
//! figure out how best to make networking happen/timers fire/things get written to disk/keys get
//! generated/etc. This makes it a good candidate for tight integration into an existing wallet
//! instead of having a rather-separate lightning appendage to a wallet.

#![cfg_attr(not(feature = "fuzztarget"), deny(missing_docs))]
#![forbid(unsafe_code)]

extern crate bitcoin;
extern crate bitcoin_hashes;
#[cfg(test)]
extern crate hex;
#[cfg(test)]
extern crate rand;
extern crate secp256k1;

#[macro_use]
pub mod util;
pub mod chain;
pub mod ln;
