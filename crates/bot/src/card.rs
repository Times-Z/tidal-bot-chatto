pub(crate) fn card(title: &str, body: &str) -> String {
    let upper = title.to_ascii_uppercase();

    let mut out = String::with_capacity(32 * (body.lines().count() + 2));
    if upper.is_empty() {
        out.push_str("┃\n");
    } else {
        out.push_str("┃ ");
        out.push_str(&upper);
        out.push('\n');
    }

    for line in body.lines() {
        if line.trim().is_empty() {
            out.push_str("┃\n");
        } else {
            out.push_str("┃ ");
            out.push_str(line);
            out.push('\n');
        }
    }

    while out.ends_with('\n') {
        out.pop();
    }
    out
}

pub(crate) fn format_duration(seconds: i32) -> String {
    let minutes = seconds / 60;
    let seconds = seconds % 60;
    format!("{minutes}:{seconds:02}")
}

pub(crate) fn help_message() -> String {
    card(
        "Commands",
        "Use /chatto-tidal <command> or mention me\n\n\
         • play <track | link> — best match or pick\n\
         • queue — list pending tracks\n\
         • pick <n> — choose from the last search\n\
         • playnext <track | link> — after the current song\n\
         • skip  • stop  • nowplaying\n\
         • remove <n>  • move <a> <b>  • shuffle\n\
         • repeat [off|all|one]\n\
         • volume [0-200]  • mute\n\
         • lyrics — karaoke screenshare\n\
         • stats  • test  • version\n\
         • help <command> — details for one",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_duration_basic() {
        assert_eq!(format_duration(0), "0:00");
        assert_eq!(format_duration(5), "0:05");
        assert_eq!(format_duration(59), "0:59");
        assert_eq!(format_duration(60), "1:00");
        assert_eq!(format_duration(65), "1:05");
        assert_eq!(format_duration(3661), "61:01");
    }

    #[test]
    fn card_renders_embed_style() {
        let out = card("Some Title", "line1\nline2");
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 3, "header + 2 body, got {out:?}");

        assert!(lines[0].starts_with("┃ "), "header: {:?}", lines[0]);
        assert!(lines[0].ends_with("SOME TITLE"));
        assert_eq!(lines[1], "┃ line1");
        assert_eq!(lines[2], "┃ line2");
        assert!(!out.ends_with('\n'), "no trailing newline expected");
    }

    #[test]
    fn card_with_empty_body_is_just_the_header() {
        let out = card("Empty", "");
        assert_eq!(out.lines().count(), 1);
        assert!(out.ends_with("EMPTY"));
    }

    #[test]
    fn card_with_empty_title_shows_bare_bar() {
        let out = card("", "x");
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[0], "┃");
        assert_eq!(lines[1], "┃ x");
    }

    #[test]
    fn card_blank_lines_keep_the_bar_and_long_titles_pass_through() {
        let out = card("Q", "a\n\nb");
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[1], "┃ a");
        assert_eq!(lines[2], "┃");
        assert_eq!(lines[3], "┃ b");

        let long = "a very very very very very very long title indeed";
        let out = card(long, "body");
        assert!(
            out.lines()
                .next()
                .unwrap()
                .contains(&long.to_ascii_uppercase())
        );
    }

    #[test]
    fn help_message_lists_commands_in_a_card() {
        let msg = help_message();
        assert!(msg.starts_with("┃ COMMANDS"), "got: {msg}");
        for needle in [
            "• play <track | link>",
            "• pick <n>",
            "• playnext <track | link>",
            "• remove <n>",
            "repeat [off|all|one]",
            "volume [0-200]",
            "lyrics",
            "stats",
            "help <command>",
        ] {
            assert!(msg.contains(needle), "missing {needle:?} in help");
        }
    }
}
