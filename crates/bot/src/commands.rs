#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Play,
    Queue,
    Pick,
    Skip,
    Stop,
    NowPlaying,
    Volume,
    Mute,
    Remove,
    Move,
    PlayNext,
    Shuffle,
    Repeat,
    Lyrics,
    Stats,
    Test,
    Help,
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
        "play" | "p" => Command::Play,
        "queue" | "q" => Command::Queue,
        "pick" => Command::Pick,
        "skip" => Command::Skip,
        "stop" => Command::Stop,
        "nowplaying" | "np" => Command::NowPlaying,
        "volume" | "vol" => Command::Volume,
        "mute" | "unmute" => Command::Mute,
        "remove" | "rm" => Command::Remove,
        "move" => Command::Move,
        "playnext" | "next" => Command::PlayNext,
        "shuffle" => Command::Shuffle,
        "repeat" => Command::Repeat,
        "lyrics" => Command::Lyrics,
        "stats" => Command::Stats,
        "test" => Command::Test,
        "help" => Command::Help,
        "version" => Command::Version,
        _ => return None,
    };

    Some(ParsedCommand { command, args })
}

/// One-line usage shown by `/chatto-tidal help <command>`.
pub fn command_help(command: Command) -> &'static str {
    match command {
        Command::Play => {
            "play <track | tidal link> — search and queue the best match; several results asks for pick"
        }
        Command::Queue => "queue — list what is queued; queue <track | link> is an alias for play",
        Command::Pick => "pick <n> — choose result n from the last search",
        Command::Skip => "skip — end the current track and play the next one",
        Command::Stop => "stop — stop playback, clear the queue and disable repeat",
        Command::NowPlaying => "nowplaying — current track with position, duration and requestor",
        Command::Volume => "volume [0-200] — show or set this room's playback volume",
        Command::Mute => "mute / unmute — pause sending audio without losing position",
        Command::Remove => "remove <n> — drop pending track n from the queue",
        Command::Move => "move <from> <to> — reorder pending tracks",
        Command::PlayNext => {
            "playnext <track | tidal link> — insert the track right after the current one"
        }
        Command::Shuffle => "shuffle — randomize the pending queue",
        Command::Repeat => "repeat [off|all|one] — show or set repeat mode",
        Command::Lyrics => "lyrics — toggle the karaoke screenshare",
        Command::Stats => "stats — uptime, rooms, queues and Tidal quality",
        Command::Test => "test — publish 10s of silence (diagnostic)",
        Command::Help => "help [command] — this list, or details for one command",
        Command::Version => "version — bot version",
    }
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

    #[test]
    fn parse_all_command_variants() {
        let cases = [
            ("play x", Command::Play),
            ("p x", Command::Play),
            ("queue y", Command::Queue),
            ("q", Command::Queue),
            ("pick 2", Command::Pick),
            ("skip", Command::Skip),
            ("stop", Command::Stop),
            ("nowplaying", Command::NowPlaying),
            ("np", Command::NowPlaying),
            ("volume 80", Command::Volume),
            ("vol 80", Command::Volume),
            ("mute", Command::Mute),
            ("unmute", Command::Mute),
            ("remove 3", Command::Remove),
            ("rm 3", Command::Remove),
            ("move 4 1", Command::Move),
            ("playnext x", Command::PlayNext),
            ("next x", Command::PlayNext),
            ("shuffle", Command::Shuffle),
            ("repeat one", Command::Repeat),
            ("lyrics", Command::Lyrics),
            ("stats", Command::Stats),
            ("test", Command::Test),
            ("help", Command::Help),
            ("version", Command::Version),
        ];
        for (body, expected) in cases {
            let cmd = parse_command(body, "tidal_bot")
                .unwrap_or_else(|| panic!("failed to parse {body:?}"));
            assert_eq!(cmd.command, expected, "for {body:?}");
        }
    }

    #[test]
    fn every_command_has_help_text() {
        let all = [
            Command::Play,
            Command::Queue,
            Command::Pick,
            Command::Skip,
            Command::Stop,
            Command::NowPlaying,
            Command::Volume,
            Command::Mute,
            Command::Remove,
            Command::Move,
            Command::PlayNext,
            Command::Shuffle,
            Command::Repeat,
            Command::Lyrics,
            Command::Stats,
            Command::Test,
            Command::Help,
            Command::Version,
        ];
        for cmd in all {
            let text = command_help(cmd);
            assert!(!text.is_empty(), "missing help for {cmd:?}");
            assert!(text.contains(' '), "help for {cmd:?} should be a sentence");
        }
    }

    #[test]
    fn parse_collapse_whitespace_around_args() {
        let cmd = parse_command("  /chatto-tidal   play   some  song  ", "tidal_bot").unwrap();
        assert_eq!(cmd.command, Command::Play);
        assert_eq!(cmd.args, "some  song");
    }

    #[test]
    fn parse_slash_only_returns_none() {
        assert!(parse_command("/", "tidal_bot").is_none());
        assert!(parse_command(" / ", "tidal_bot").is_none());
    }

    #[test]
    fn parse_with_empty_bot_name_ignores_mentions() {
        assert!(parse_command("@tidal_bot play x", "").is_none());
        assert_eq!(
            parse_command("/chatto-tidal play x", "").unwrap().command,
            Command::Play
        );
    }

    #[test]
    fn mention_stripping_is_case_insensitive() {
        let cmd = parse_command("@TIDAL_BOT skip", "tidal_bot").unwrap();
        assert_eq!(cmd.command, Command::Skip);
        let cmd = parse_command("@Tidal_Bot /stop", "tidal_bot").unwrap();
        assert_eq!(cmd.command, Command::Stop);
    }

    #[test]
    fn mid_text_mention_is_not_parsed_as_command() {
        // is_addressed accepts mentions anywhere, but parsing requires the
        // mention to lead the message.
        assert!(parse_command("hey @tidal_bot skip", "tidal_bot").is_none());
    }

    #[test]
    fn prefix_without_boundary_is_not_ours() {
        assert!(parse_command("/chatto-tidalxyz play", "tidal_bot").is_none());
    }

    #[test]
    fn namespaced_prefix_is_case_insensitive() {
        let cmd = parse_command("/CHATTO-TIDAL PLAY Daft Punk", "tidal_bot").unwrap();
        assert_eq!(cmd.command, Command::Play);
        // The args keep their original case.
        assert_eq!(cmd.args, "Daft Punk");
    }

    #[test]
    fn parse_args_keep_unicode() {
        let cmd = parse_command("play ÆØÅ Café – 世界", "tidal_bot").unwrap();
        assert_eq!(cmd.args, "ÆØÅ Café – 世界");
    }

    #[test]
    fn addressed_with_slash_separator_after_prefix() {
        assert!(is_addressed("/chatto-tidal/play x", &["tidal_bot"]));
        assert_eq!(
            parse_command("/chatto-tidal/play x", "tidal_bot")
                .unwrap()
                .command,
            Command::Play
        );
    }

    #[test]
    fn addressed_by_display_name_mention() {
        assert!(is_addressed("@Tidal Bot play something", &["Tidal Bot"]));
        assert!(is_addressed("yo, ask @tidal_bot first", &["tidal_bot"]));
    }

    #[test]
    fn is_addressed_ignores_bare_prefix_lookalikes() {
        assert!(!is_addressed("/chatto-tidal-not-a-command", &[]));
        assert!(!is_addressed(
            "tidal_bot without the at sign",
            &["tidal_bot"]
        ));
    }
}
