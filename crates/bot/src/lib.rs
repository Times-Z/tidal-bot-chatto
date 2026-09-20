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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_is_the_crate_version() {
        assert!(!version().is_empty());
        assert!(
            version()
                .split('.')
                .all(|part| part.chars().all(|c| c.is_ascii_digit())),
            "unexpected version {:?}",
            version()
        );
    }
}
