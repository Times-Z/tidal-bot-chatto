pub mod commands;
pub mod karaoke;
pub mod queue;
pub mod runtime;

pub use commands::{Command, ParsedCommand, parse_command};
pub use karaoke::KaraokeRenderer;
pub use queue::{Queue, Track};
pub use runtime::{Bot, BotConfig, Error};

pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
