//! Pull-mode reconciliation workers that complement the live tailer.
//!
//! Each module here owns one upstream contract, mirroring the in-tree
//! `providers/*` files but reserved for periodic HTTP pulls that
//! truth-up cost data after the local-tail rows already exist.

pub mod copilot_chat_billing;
