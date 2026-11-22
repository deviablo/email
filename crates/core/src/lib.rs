// SPDX-License-Identifier: GPL-3.0-or-later

//! Everything that does not need a user interface: talking to the mail
//! server, keeping a local copy and preparing messages for display.

pub mod accounts;
pub mod check;
pub mod compose;
pub mod discovery;
pub mod folder;
pub mod html;
pub mod imap;
pub mod model;
pub mod notify;
pub mod parse;
pub mod search;
pub mod server;
pub mod smtp;
pub mod store;
pub mod sync;
pub mod thread;
pub mod tls;
pub mod utf7;
