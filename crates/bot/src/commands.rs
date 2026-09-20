#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Play,
    Queue,
    Skip,
    Stop,
    NowPlaying,
    Volume,
    Mute,
    Test,
    Help,
    Lyrics,
    Version,
}

/// Namespaced slash command handled by this bot. Plain `/play` style messages
/// are shared chat surface and must not hijack other clients' commands.
pub const COMMAND_PREFIX: &str = "/chatto-tidal";

/// Returns the remainder after the bot command prefix when `text` starts with
/// it followed by a boundary (end of text, whitespace or another slash).
fn strip_command_prefix(text: &str) -> Option<&str> {
    let matched = text
        .get(..COMMAND_PREFIX.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(COMMAND_PREFIX));
    if !matched {
        return None;
    }

    let rest = &text[COMMAND_PREFIX.len()..];
    if rest.is_empty() || rest.starts_with(char::is_whitespace) || rest.starts_with('/') {
        Some(rest.trim_start())
    } else {
        None
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedCommand {
    pub command: Command,
    pub args: String,
}

pub fn parse_command(body: &str, bot_name: &str) -> Option<ParsedCommand> {
    let mut text = body.trim();
    if text.is_empty() {
        return None;
    }

    let (stripped, _) = strip_mention(text, bot_name);
    text = stripped.trim();
    if let Some(rest) = strip_command_prefix(text) {
        text = rest;
    }
    text = text.strip_prefix('/').unwrap_or(text);

    if text.is_empty() {
        return None;
    }

    let mut parts = text.splitn(2, char::is_whitespace);
    let raw_command = parts.next()?.to_ascii_lowercase();
    let args = parts.next().map(str::trim).unwrap_or_default().to_owned();

    let command = match raw_command.as_str() {
        "play" => Command::Play,
        "queue" => Command::Queue,
        "skip" => Command::Skip,
        "stop" => Command::Stop,
        "nowplaying" => Command::NowPlaying,
        "volume" => Command::Volume,
        "mute" | "unmute" => Command::Mute,
        "test" => Command::Test,
        "help" => Command::Help,
        "lyrics" => Command::Lyrics,
        "version" => Command::Version,
        _ => return None,
    };

    Some(ParsedCommand { command, args })
}

fn strip_mention<'a>(body: &'a str, bot_name: &str) -> (&'a str, bool) {
    if bot_name.is_empty() {
        return (body, false);
    }

    let prefix = format!("@{bot_name}");
    if body
        .get(..prefix.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(prefix.as_str()))
    {
        (&body[prefix.len()..], true)
    } else {
        (body, false)
    }
}

/// True when a message is addressed to the bot: it either starts with the
/// namespaced `/chatto-tidal` command prefix or mentions the bot (`@name`)
/// anywhere in the body. `names` typically holds the bot login and its
/// configured display name.
pub fn is_addressed(body: &str, names: &[&str]) -> bool {
    let text = body.trim();
    if strip_command_prefix(text).is_some() {
        return true;
    }

    let lower = text.to_ascii_lowercase();
    names
        .iter()
        .filter(|name| !name.is_empty())
        .any(|name| lower.contains(&format!("@{}", name.to_ascii_lowercase())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_plain_command() {
        let cmd = parse_command("play Around The World", "tidal_bot").unwrap();
        assert_eq!(cmd.command, Command::Play);
        assert_eq!(cmd.args, "Around The World");
    }

    #[test]
    fn parse_slash_command() {
        let cmd = parse_command("/queue Daft Punk", "tidal_bot").unwrap();
        assert_eq!(cmd.command, Command::Queue);
        assert_eq!(cmd.args, "Daft Punk");
    }

    #[test]
    fn parse_mention_and_slash() {
        let cmd = parse_command("@tidal_bot /skip", "tidal_bot").unwrap();
        assert_eq!(cmd.command, Command::Skip);
        assert_eq!(cmd.args, "");
    }

    #[test]
    fn parse_mention_and_plain() {
        let cmd = parse_command("@tidal_bot nowplaying", "tidal_bot").unwrap();
        assert_eq!(cmd.command, Command::NowPlaying);
        assert_eq!(cmd.args, "");
    }

    #[test]
    fn parse_case_insensitive_command() {
        let cmd = parse_command("VoLuMe 120", "tidal_bot").unwrap();
        assert_eq!(cmd.command, Command::Volume);
        assert_eq!(cmd.args, "120");
    }

    #[test]
    fn parse_empty_body() {
        assert!(parse_command("   ", "tidal_bot").is_none());
    }

    #[test]
    fn parse_unknown_command() {
        assert!(parse_command("dance now", "tidal_bot").is_none());
    }

    #[test]
    fn parse_only_mention() {
        assert!(parse_command("@tidal_bot", "tidal_bot").is_none());
    }

    #[test]
    fn parse_version_command() {
        let cmd = parse_command("/chatto-tidal version", "tidal_bot").unwrap();
        assert_eq!(cmd.command, Command::Version);
        assert_eq!(cmd.args, "");
    }

    #[test]
    fn addressed_by_slash_command() {
        assert!(is_addressed("/chatto-tidal play Daft Punk", &["tidal_bot"]));
        assert!(is_addressed("/CHATTO-TIDAL skip", &["tidal_bot"]));
    }

    #[test]
    fn addressed_by_mention_anywhere() {
        assert!(is_addressed(
            "hey @tidal_bot play something",
            &["tidal_bot", ""]
        ));
    }

    #[test]
    fn mention_is_case_insensitive() {
        assert!(is_addressed("@Tidal_Bot skip", &["tidal_bot"]));
    }

    #[test]
    fn plain_chat_is_not_addressed() {
        assert!(!is_addressed(
            "Ah tient je t'ai pas parler mec",
            &["tidal_bot", "tidal_bot"]
        ));
        assert!(!is_addressed("play it loud", &["tidal_bot"]));
    }

    #[test]
    fn foreign_slash_commands_are_not_addressed() {
        assert!(!is_addressed("/play daft punk", &["tidal_bot"]));
        assert!(!is_addressed("/shrug", &["tidal_bot"]));
        assert!(!is_addressed("/chatto-tidalx skip", &["tidal_bot"]));
    }

    #[test]
    fn no_names_configured_needs_prefix() {
        assert!(!is_addressed("play daft punk", &[""]));
        assert!(is_addressed("/chatto-tidal play daft punk", &[""]));
    }

    #[test]
    fn parse_namespaced_slash_command() {
        let cmd = parse_command("/chatto-tidal play Around The World", "tidal_bot").unwrap();
        assert_eq!(cmd.command, Command::Play);
        assert_eq!(cmd.args, "Around The World");
    }

    #[test]
    fn parse_namespaced_double_slash_command() {
        let cmd = parse_command("/chatto-tidal /queue", "tidal_bot").unwrap();
        assert_eq!(cmd.command, Command::Queue);
    }

    #[test]
    fn parse_namespaced_alone_is_not_a_command() {
        assert!(parse_command("/chatto-tidal", "tidal_bot").is_none());
        assert!(parse_command("/chatto-tidal dance", "tidal_bot").is_none());
    }
}
