//! Tests of the SQLite database.

use super::messages::quote_fts_query;
use super::*;
use crate::irc::Message;
use crate::msg;
use rusqlite::Connection;

#[expect(
    clippy::unwrap_used,
    reason = "Fixture setup and assertions fail immediately on invalid test data."
)]
fn db() -> Database {
    Database::open_in_memory().unwrap()
}

mod accounts;
mod messages;
mod migrations;
mod receipts;
mod search;
mod storage;
mod targets;
