//! Tests of the SQLite database.

use super::messages::quote_fts_query;
use super::*;
use crate::irc::Message;
use crate::msg;
use rusqlite::Connection;

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
