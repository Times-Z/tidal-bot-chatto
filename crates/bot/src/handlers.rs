//! Command handlers. Each `cmd_*` method implements one slash command:
//! they mutate `RoomState` under the rooms lock and answer through
//! `Bot::reply`, which routes the text to the command's thread when thread
//! replies are on.

use crate::card::{card, format_duration, help_message};
use crate::commands::{command_help, parse_command};
use crate::karaoke;
use crate::queue::{RepeatMode, Track};
use crate::runtime::{Bot, CmdCtx, QueuePlacement, RoomState};
use livekit_audio::Player as LivekitPlayer;
use std::sync::atomic::Ordering;
use std::time::Duration;

/// Search results listed by `play` before `pick`.
const SEARCH_PICK_LIMIT: usize = 5;

/// Pending tracks listed by `queue` before truncation.
const QUEUE_DISPLAY_LIMIT: usize = 15;

/// Parse a 1-based index argument (first whitespace-separated token).
fn parse_index(s: &str) -> Option<usize> {
    s.split_whitespace()
        .next()?
        .parse::<usize>()
        .ok()
        .filter(|n| *n > 0)
}

impl Bot {
    /// Search (or resolve a link) and enqueue. When a text search returns
    /// several candidates, they are listed and the user confirms one with
    /// `pick <n>`.
    pub(crate) async fn cmd_queue(
        &self,
        ctx: &CmdCtx,
        query: &str,
    ) -> Result<(), crate::runtime::Error> {
        let room_id = ctx.room_id.as_str();
        if query.trim().is_empty() {
            self.print_queue(ctx).await;
            return Ok(());
        }

        if let Some(url) = tidal::extract_tidal_url(query) {
            return self.cmd_queue_link(ctx, url, false).await;
        }

        let tracks = {
            let tidal = self.tidal.lock().await;
            tidal.search(query, SEARCH_PICK_LIMIT).await?
        };

        if tracks.is_empty() {
            self.reply(
                ctx,
                &card("Not Found", &format!("No results for: {}", query)),
            )
            .await;
            return Ok(());
        }

        if tracks.len() == 1 {
            return self
                .enqueue_tracks(ctx, &tracks, None, QueuePlacement::Tail)
                .await;
        }

        // Several candidates: remember them and let `pick` choose.
        let listing = tracks
            .iter()
            .enumerate()
            .map(|(idx, t)| {
                format!(
                    "{}. {} · {} ({})",
                    idx + 1,
                    t.title,
                    t.artist,
                    format_duration(t.duration)
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        {
            let mut rooms = self.rooms.lock().await;
            let rs = rooms
                .entry(room_id.to_owned())
                .or_insert_with(|| RoomState::new(&self.cfg));
            rs.pending_search = tracks;
        }
        self.reply(
            ctx,
            &card(
                "Search",
                &format!("{listing}\n\npick one with: /chatto-tidal pick <n>"),
            ),
        )
        .await;
        Ok(())
    }

    /// Choose result `n` from the last search and enqueue it.
    pub(crate) async fn cmd_pick(
        &self,
        ctx: &CmdCtx,
        args: &str,
    ) -> Result<(), crate::runtime::Error> {
        let room_id = ctx.room_id.as_str();
        let Some(n) = parse_index(args) else {
            self.reply(ctx, &card("Pick", "Usage: pick <n>")).await;
            return Ok(());
        };

        let chosen = {
            let mut rooms = self.rooms.lock().await;
            match rooms.get_mut(room_id) {
                Some(rs) if n <= rs.pending_search.len() => Some(rs.pending_search.remove(n - 1)),
                _ => None,
            }
        };

        match chosen {
            Some(track) => {
                self.enqueue_tracks(
                    ctx,
                    std::slice::from_ref(&track),
                    None,
                    QueuePlacement::Tail,
                )
                .await
            }
            None => {
                self.reply(
                    ctx,
                    &card("Pick", "No pending search result at that number."),
                )
                .await;
                Ok(())
            }
        }
    }

    /// Queue a track so it plays immediately after the current one.
    pub(crate) async fn cmd_playnext(
        &self,
        ctx: &CmdCtx,
        query: &str,
    ) -> Result<(), crate::runtime::Error> {
        if query.trim().is_empty() {
            self.reply(
                ctx,
                &card("Play Next", "Usage: playnext <track | tidal link>"),
            )
            .await;
            return Ok(());
        }

        if let Some(url) = tidal::extract_tidal_url(query) {
            return self.cmd_queue_link(ctx, url, true).await;
        }

        let tracks = {
            let tidal = self.tidal.lock().await;
            tidal.search(query, 1).await?
        };

        if tracks.is_empty() {
            self.reply(
                ctx,
                &card("Not Found", &format!("No results for: {}", query)),
            )
            .await;
            return Ok(());
        }

        self.enqueue_tracks(ctx, &tracks[0..1], None, QueuePlacement::Next)
            .await
    }

    /// Resolve a Tidal link (track / album / playlist) and enqueue what it
    /// points to.
    pub(crate) async fn cmd_queue_link(
        &self,
        ctx: &CmdCtx,
        url: &str,
        next: bool,
    ) -> Result<(), crate::runtime::Error> {
        let (content_type, id) = match tidal::parse_tidal_url(url) {
            Ok(parsed) => parsed,
            Err(err) => {
                self.reply(
                    ctx,
                    &card("Link Error", &format!("Could not parse that link: {err}")),
                )
                .await;
                return Ok(());
            }
        };

        let numeric_id = || id.parse::<u64>().ok();

        let tidal = self.tidal.lock().await;
        let outcome = match content_type {
            tidal::TidalContentType::Track => match numeric_id() {
                Some(tid) => tidal.track_by_id(tid).await.map(|track| vec![track]),
                None => Err(tidal::Error::UnrecognizedUrlPath(id.clone())),
            },
            tidal::TidalContentType::Album => match numeric_id() {
                Some(album_id) => tidal.album_tracks(album_id).await,
                None => Err(tidal::Error::UnrecognizedUrlPath(id.clone())),
            },
            tidal::TidalContentType::Playlist => tidal.playlist_tracks(&id).await,
            tidal::TidalContentType::Artist => {
                drop(tidal);
                self.reply(
                    ctx,
                    &card("Link Error", "Artist links are not supported yet."),
                )
                .await;
                return Ok(());
            }
        };
        drop(tidal);

        let tracks = match outcome {
            Ok(tracks) if tracks.is_empty() => {
                self.reply(ctx, &card("Not Found", "That link has no playable tracks."))
                    .await;
                return Ok(());
            }
            Ok(tracks) => tracks,
            Err(err) => {
                self.reply(
                    ctx,
                    &card("Link Error", &format!("Tidal lookup failed: {err}")),
                )
                .await;
                return Ok(());
            }
        };

        let label = match content_type {
            tidal::TidalContentType::Album => Some("album"),
            tidal::TidalContentType::Playlist => Some("playlist"),
            _ => None,
        };
        let placement = if next {
            QueuePlacement::Next
        } else {
            QueuePlacement::Tail
        };
        self.enqueue_tracks(ctx, &tracks, label, placement).await
    }

    /// Add resolved tracks to the room queue and report what was added.
    /// With `QueuePlacement::Next` the first track plays right after the
    /// current one; the rest (album/playlist) still go to the tail.
    async fn enqueue_tracks(
        &self,
        ctx: &CmdCtx,
        tracks: &[tidal::SearchResult],
        label: Option<&str>,
        placement: QueuePlacement,
    ) -> Result<(), crate::runtime::Error> {
        if tracks.is_empty() {
            return Ok(());
        }

        let added: Vec<Track> = tracks
            .iter()
            .map(|first| Track {
                tid: first.id,
                title: first.title.clone(),
                artist: first.artist.clone(),
                duration: first.duration,
                requestor: ctx.actor_id.clone(),
                requestor_name: ctx.actor_display.clone(),
                cover_url: first.cover_url.clone(),
            })
            .collect();

        let position = {
            let mut rooms = self.rooms.lock().await;
            let rs = rooms
                .entry(ctx.room_id.clone())
                .or_insert_with(|| RoomState::new(&self.cfg));
            match placement {
                QueuePlacement::Next => {
                    rs.queue.insert_next(added[0].clone());
                    for track in &added[1..] {
                        rs.queue.add(track.clone());
                    }
                    "next".to_owned()
                }
                QueuePlacement::Tail => {
                    for track in &added {
                        rs.queue.add(track.clone());
                    }
                    format!("#{}", rs.queue.total_len() - added.len() + 1)
                }
            }
        };

        let first = &added[0];
        let where_line = if position == "next" {
            "plays next".to_owned()
        } else {
            format!("position {position}")
        };
        let body = match label {
            Some(label) => format!(
                "{count} {label} tracks\nNext: {} · {} ({})\n↳ {where_line}",
                first.title,
                first.artist,
                format_duration(first.duration),
                count = added.len(),
            ),
            None => format!(
                "{} · {}\n↳ {} · {where_line}",
                first.title,
                first.artist,
                format_duration(first.duration),
            ),
        };

        self.reply(ctx, &card("Added", &body)).await;
        Ok(())
    }

    async fn print_queue(&self, ctx: &CmdCtx) {
        let message = {
            let rooms = self.rooms.lock().await;
            let Some(rs) = rooms.get(&ctx.room_id) else {
                return;
            };

            let list = rs.queue.list();
            if rs.current.is_none() && list.is_empty() {
                card("Queue", "Queue is empty.")
            } else {
                let mut body = String::new();
                if let Some(current) = &rs.current {
                    body.push_str(&format!(
                        "▶ {} · {} ({})\n",
                        current.track.title,
                        current.track.artist,
                        format_duration(current.track.duration),
                    ));
                    if !current.track.requestor_name.is_empty() {
                        body.push_str(&format!(
                            "↳ requested by {}\n",
                            current.track.requestor_name
                        ));
                    }
                    if rs.queue.repeat() != RepeatMode::Off {
                        body.push_str(&format!("↳ repeat: {}\n", rs.queue.repeat().label()));
                    }
                    body.push('\n');
                }
                for (idx, track) in list.iter().take(QUEUE_DISPLAY_LIMIT).enumerate() {
                    body.push_str(&format!(
                        "{}. {} · {} ({}) — {}\n",
                        idx + 1,
                        track.title,
                        track.artist,
                        format_duration(track.duration),
                        track.requestor_name
                    ));
                }
                if list.len() > QUEUE_DISPLAY_LIMIT {
                    body.push_str(&format!(
                        "… and {} more\n",
                        list.len() - QUEUE_DISPLAY_LIMIT
                    ));
                }
                card("Queue", &body)
            }
        };

        self.reply(ctx, &message).await;
    }

    pub(crate) async fn cmd_now_playing(&self, ctx: &CmdCtx) {
        let message = {
            let rooms = self.rooms.lock().await;
            let Some(rs) = rooms.get(&ctx.room_id) else {
                return;
            };

            match &rs.current {
                Some(current) => {
                    let position = rs
                        .voice
                        .as_ref()
                        .and_then(|v| v.play_start.load_full())
                        .map(|started| {
                            karaoke::format_duration_ms(started.elapsed().as_millis() as u64)
                        })
                        .unwrap_or_else(|| "0:00".to_owned());
                    let request = if current.track.requestor_name.is_empty() {
                        String::new()
                    } else {
                        format!("\n↳ requested by {}", current.track.requestor_name)
                    };
                    let audio = if current.audio_info.is_empty() {
                        String::new()
                    } else {
                        format!(" · {}", current.audio_info)
                    };
                    card(
                        "Now Playing",
                        &format!(
                            "{} · {}\n{} / {}{audio}{request}",
                            current.track.title,
                            current.track.artist,
                            position,
                            format_duration(current.track.duration),
                        ),
                    )
                }
                None => card("Now Playing", "Nothing currently prepared."),
            }
        };

        self.reply(ctx, &message).await;
    }

    pub(crate) async fn cmd_skip(&self, ctx: &CmdCtx) {
        let room_id = ctx.room_id.as_str();
        let (title, voice_dead) = {
            let mut rooms = self.rooms.lock().await;
            let Some(rs) = rooms.get_mut(room_id) else {
                return;
            };
            let title = rs.current.as_ref().map(|c| c.track.title.clone());
            let voice_dead = rs.voice.as_ref().is_none_or(|v| v.handle.is_finished());

            if let Some(ref v) = rs.voice {
                v.song_cancel.store(true, Ordering::SeqCst);
            }
            if let Some(ref cancel) = rs.karaoke_cancel {
                cancel.store(true, Ordering::SeqCst);
            }
            rs.karaoke_cancel = None;
            if voice_dead {
                rs.current = None;
            }
            (title, voice_dead)
        };

        if voice_dead {
            self.reply(ctx, &card("Skipped", "Not connected to voice."))
                .await;
            self.ensure_voice(room_id).await;
            return;
        }

        match title {
            Some(t) => {
                self.reply(ctx, &card("Skipped", &t)).await;
            }
            None => {
                self.reply(ctx, &card("Skipped", "Nothing playing.")).await;
            }
        }
    }

    pub(crate) async fn cmd_stop(&self, ctx: &CmdCtx) {
        let room_id = ctx.room_id.as_str();
        let mut rooms = self.rooms.lock().await;
        let Some(rs) = rooms.get_mut(room_id) else {
            return;
        };

        if let Some(v) = rs.voice.take() {
            v.cancel.store(true, Ordering::SeqCst);
            v.song_cancel.store(true, Ordering::SeqCst);
        }
        if let Some(cancel) = rs.karaoke_cancel.take() {
            cancel.store(true, Ordering::SeqCst);
        }

        let count = rs.queue.len();
        rs.queue.clear();
        rs.current = None;
        rs.pending_search.clear();

        let msg = card("Stopped", &format!("Removed {count} queued track(s)."));
        drop(rooms);
        self.reply(ctx, &msg).await;
    }

    pub(crate) async fn cmd_volume(&self, ctx: &CmdCtx, args: &str) {
        let room_id = ctx.room_id.as_str();
        let volume = {
            let mut rooms = self.rooms.lock().await;
            let rs = rooms
                .entry(room_id.to_owned())
                .or_insert_with(|| RoomState::new(&self.cfg));
            rs.volume.clone()
        };

        if args.trim().is_empty() {
            let current = volume.load(Ordering::SeqCst);
            self.reply(ctx, &card("Volume", &format!("Current: {current}%")))
                .await;
            return;
        }

        let Ok(pct) = args.trim().parse::<u16>() else {
            self.reply(ctx, &card("Volume", "Usage: volume <0-200>"))
                .await;
            return;
        };

        if pct > 200 {
            self.reply(ctx, &card("Volume", "Usage: volume <0-200>"))
                .await;
            return;
        }

        volume.store(u32::from(pct), Ordering::SeqCst);
        self.reply(ctx, &card("Volume", &format!("Set to {}%", pct)))
            .await;
    }

    pub(crate) async fn cmd_mute(&self, ctx: &CmdCtx) {
        let state = {
            let mut rooms = self.rooms.lock().await;
            let Some(rs) = rooms.get_mut(&ctx.room_id) else {
                self.reply(ctx, &card("Mute", "No active room.")).await;
                return;
            };
            let new_state = !rs.muted.load(Ordering::SeqCst);
            rs.muted.store(new_state, Ordering::SeqCst);
            new_state
        };

        let msg = if state {
            card("Mute", "Microphone muted.")
        } else {
            card("Unmute", "Microphone unmuted.")
        };
        self.reply(ctx, &msg).await;
    }

    pub(crate) async fn cmd_remove(&self, ctx: &CmdCtx, args: &str) {
        let room_id = ctx.room_id.as_str();
        let Some(n) = parse_index(args) else {
            self.reply(ctx, &card("Remove", "Usage: remove <n>")).await;
            return;
        };

        let removed = {
            let mut rooms = self.rooms.lock().await;
            match rooms.get_mut(room_id) {
                Some(rs) => rs.queue.remove_pending(n),
                None => None,
            }
        };

        match removed {
            Some(track) => {
                self.reply(
                    ctx,
                    &card("Removed", &format!("{} · {}", track.title, track.artist)),
                )
                .await;
            }
            None => {
                self.reply(
                    ctx,
                    &card(
                        "Remove",
                        &format!("No pending track #{n}. Use `queue` to list."),
                    ),
                )
                .await;
            }
        }
    }

    pub(crate) async fn cmd_move(&self, ctx: &CmdCtx, args: &str) {
        let room_id = ctx.room_id.as_str();
        let mut parts = args.split_whitespace();
        let (Some(from), Some(to)) = (
            parts.next().and_then(|s| s.parse::<usize>().ok()),
            parts.next().and_then(|s| s.parse::<usize>().ok()),
        ) else {
            self.reply(ctx, &card("Move", "Usage: move <from> <to>"))
                .await;
            return;
        };

        let moved = {
            let mut rooms = self.rooms.lock().await;
            match rooms.get_mut(room_id) {
                Some(rs) => rs.queue.move_pending(from, to),
                None => false,
            }
        };

        let msg = if moved {
            card("Moved", &format!("#{from} -> #{to}"))
        } else {
            card("Move", "Both numbers must be pending queue positions.")
        };
        self.reply(ctx, &msg).await;
    }

    pub(crate) async fn cmd_shuffle(&self, ctx: &CmdCtx) {
        let room_id = ctx.room_id.as_str();
        let count = {
            let mut rooms = self.rooms.lock().await;
            let Some(rs) = rooms.get_mut(room_id) else {
                return;
            };
            let count = rs.queue.list().len();
            rs.queue.shuffle_pending();
            count
        };
        self.reply(
            ctx,
            &card("Shuffle", &format!("Shuffled {count} pending tracks.")),
        )
        .await;
    }

    pub(crate) async fn cmd_repeat(&self, ctx: &CmdCtx, args: &str) {
        let room_id = ctx.room_id.as_str();
        let mode = {
            let mut rooms = self.rooms.lock().await;
            let rs = rooms
                .entry(room_id.to_owned())
                .or_insert_with(|| RoomState::new(&self.cfg));
            let mode = match args.trim() {
                "" => rs.queue.repeat().cycle(),
                arg => match RepeatMode::parse(arg) {
                    Some(mode) => mode,
                    None => {
                        drop(rooms);
                        self.reply(ctx, &card("Repeat", "Usage: repeat [off|all|one]"))
                            .await;
                        return;
                    }
                },
            };
            rs.queue.set_repeat(mode);
            mode
        };
        self.reply(ctx, &card("Repeat", &format!("Repeat: {}", mode.label())))
            .await;
    }

    pub(crate) async fn cmd_stats(&self, ctx: &CmdCtx) {
        let uptime = chrono::Utc::now().signed_duration_since(self.started_at);
        let hours = uptime.num_hours();
        let minutes = uptime.num_minutes() % 60;
        let seconds = uptime.num_seconds() % 60;

        let quality = {
            let tidal = self.tidal.lock().await;
            tidal.selected_quality().to_owned()
        };

        let mut body =
            format!("Uptime: {hours}h {minutes:02}m {seconds:02}s\nQuality: {quality}\n");
        {
            let rooms = self.rooms.lock().await;
            for room in &self.cfg.rooms {
                let Some(rs) = rooms.get(room) else {
                    continue;
                };
                let playing = rs
                    .current
                    .as_ref()
                    .map(|c| format!("playing {}", c.track.title))
                    .unwrap_or_else(|| "idle".to_owned());
                let lyrics = if rs.lyrics_enabled { "on" } else { "off" };
                let volume = rs.volume.load(Ordering::SeqCst);
                body.push_str(&format!(
                    "Room {room}: {playing}, queue {}, lyrics {lyrics}, vol {volume}%\n",
                    rs.queue.len()
                ));
            }
        }
        self.reply(ctx, &card("Stats", body.trim_end())).await;
    }

    pub(crate) async fn cmd_help(&self, ctx: &CmdCtx, args: &str) {
        let topic = args.split_whitespace().next();
        let message = match topic.map(|t| parse_command(t, "")) {
            Some(Some(parsed)) => card("Help", command_help(parsed.command)),
            Some(None) => card(
                "Help",
                &format!(
                    "No such command: {}\nType help for the list.",
                    topic.unwrap_or_default()
                ),
            ),
            None => help_message(),
        };
        self.reply(ctx, &message).await;
    }

    /// Connect, publish 10s of silence, disconnect. Used to sanity-check
    /// the voice plumbing without a full queue.
    pub(crate) async fn cmd_test(&self, ctx: &CmdCtx) {
        let room_id = ctx.room_id.as_str();
        self.reply(ctx, &card("Test", "Publishing 10s of silence..."))
            .await;

        let token = match self.chatto.create_call_token(room_id).await {
            Ok(token) => token,
            Err(err) => {
                self.reply(
                    ctx,
                    &card("Test Error", &format!("Create call token: {err}")),
                )
                .await;
                return;
            }
        };

        let mut player = match LivekitPlayer::new(livekit_audio::Config {
            url: self.livekit_url.clone(),
            token: token.token,
            room: room_id.to_owned(),
            sample_rate: self.cfg.sample_rate,
        })
        .await
        {
            Ok(player) => player,
            Err(err) => {
                self.reply(ctx, &card("Test Error", &format!("Create player: {err}")))
                    .await;
                return;
            }
        };

        match player.play_silence_only(Duration::from_secs(10)).await {
            Ok(()) => {
                self.reply(ctx, &card("Test", "10s silence published OK."))
                    .await;
            }
            Err(err) => {
                self.reply(ctx, &card("Test Failed", &err.to_string()))
                    .await;
            }
        }

        player.disconnect().await;
    }

    pub(crate) async fn cmd_lyrics(&self, ctx: &CmdCtx) {
        let room_id = ctx.room_id.as_str();
        let enable = {
            let mut rooms = self.rooms.lock().await;
            let Some(rs) = rooms.get_mut(room_id) else {
                return;
            };
            rs.lyrics_enabled = !rs.lyrics_enabled;
            if !rs.lyrics_enabled {
                if let Some(cancel) = rs.karaoke_cancel.take() {
                    cancel.store(true, Ordering::SeqCst);
                }
                // Only the voice task has the Room; ask it to unpublish so
                // the last frame does not linger.
                if let Some(ref v) = rs.voice {
                    v.video_unpublish.store(true, Ordering::SeqCst);
                }
            }
            rs.lyrics_enabled
        };

        if enable {
            self.reply(ctx, &card("Lyrics", "Karaoke screenshare enabled."))
                .await;
            self.start_karaoke(room_id).await;
        } else {
            self.reply(ctx, &card("Lyrics", "Karaoke screenshare disabled."))
                .await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_index_requires_positive_first_token() {
        assert_eq!(parse_index("3"), Some(3));
        assert_eq!(parse_index(" 2 more words "), Some(2));
        assert_eq!(parse_index("0"), None);
        assert_eq!(parse_index("-1"), None);
        assert_eq!(parse_index("abc"), None);
        assert_eq!(parse_index(""), None);
    }
}
