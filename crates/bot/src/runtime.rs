use crate::card::card;
use crate::commands::{Command, is_addressed, parse_command};
use crate::queue::{Queue, Track};
use crate::voice::VoiceConnection;
use chatto::{Client as ChattoClient, RoomTimelineEvent, UserProfile};
use chrono::{DateTime, NaiveDateTime, Utc};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;
use tidal::Client as TidalClient;
use tokio::sync::Mutex;
use tracing::{error, info, warn};

/// Event IDs remembered for dedup; oldest fall out past the cap.
const SEEN_EVENTS_CAP: usize = 4096;

/// Thread timelines followed per room (LRU).
const MAX_WATCHED_THREADS: usize = 16;

/// Events fetched per thread poll.
const THREAD_POLL_LIMIT: i32 = 25;

#[derive(Debug)]
pub struct BotConfig {
    pub rooms: Vec<String>,
    pub poll_interval: Duration,
    pub bot_name: String,
    pub volume: u8,
    pub sample_rate: u32,
    /// Whether rooms start with the karaoke screenshare enabled.
    pub default_lyrics: bool,
    /// Reply to commands inside the thread of the requesting message.
    pub thread_replies: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("chatto error: {0}")]
    Chatto(#[from] chatto::Error),
    #[error("tidal error: {0}")]
    Tidal(#[from] tidal::Error),
    #[error("livekit audio error: {0}")]
    LivekitAudio(#[from] livekit_audio::Error),
}

#[derive(Debug, Clone)]
pub(crate) struct CurrentTrack {
    pub(crate) track: Track,
    pub(crate) audio_info: String,
    pub(crate) stream_url: String,
}

/// Whether a queued track goes to the tail or right after the current song.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum QueuePlacement {
    Tail,
    Next,
}

/// Thread timelines the bot follows, each with a poll cursor. Thread replies
/// do not show up in the room timeline, so followed threads are polled
/// separately. Capped per room, oldest dropped first.
#[derive(Debug, Default)]
pub(crate) struct ThreadWatch {
    cursors: HashMap<String, String>,
    /// Watch order, oldest first; the front is evicted past the cap.
    order: VecDeque<String>,
}

impl ThreadWatch {
    /// Start following a thread. True when newly added (follow it
    /// server-side).
    fn watch(&mut self, root: &str) -> bool {
        if self.cursors.contains_key(root) {
            return false;
        }
        self.cursors.insert(root.to_owned(), String::new());
        self.order.push_back(root.to_owned());
        while self.order.len() > MAX_WATCHED_THREADS {
            if let Some(old) = self.order.pop_front() {
                self.cursors.remove(&old);
            }
        }
        true
    }

    fn advance(&mut self, root: &str, cursor: String) {
        if cursor.is_empty() {
            return;
        }
        if let Some(current) = self.cursors.get_mut(root) {
            *current = cursor;
        }
    }

    fn unwatch(&mut self, root: &str) {
        self.cursors.remove(root);
        self.order.retain(|r| r != root);
    }

    /// (root, after-cursor) pairs in watch order.
    fn snapshot(&self) -> Vec<(String, String)> {
        self.order
            .iter()
            .map(|root| {
                (
                    root.clone(),
                    self.cursors.get(root).cloned().unwrap_or_default(),
                )
            })
            .collect()
    }
}

#[derive(Debug)]
pub(crate) struct RoomState {
    pub(crate) queue: Queue,
    pub(crate) current: Option<CurrentTrack>,
    pub(crate) cursor: String,
    pub(crate) muted: Arc<AtomicBool>,
    pub(crate) voice: Option<VoiceConnection>,
    pub(crate) lyrics_enabled: bool,
    pub(crate) karaoke_cancel: Option<Arc<AtomicBool>>,
    /// Results of the last ambiguous search, awaiting `/pick <n>`.
    pub(crate) pending_search: Vec<tidal::SearchResult>,
    /// Playback volume for this room only (0-200 percent).
    pub(crate) volume: Arc<AtomicU32>,
    /// Thread timelines polled for commands alongside the room timeline.
    pub(crate) threads: ThreadWatch,
}

impl RoomState {
    pub(crate) fn new(cfg: &BotConfig) -> Self {
        Self {
            queue: Queue::default(),
            current: None,
            cursor: String::new(),
            muted: Arc::new(AtomicBool::new(false)),
            voice: None,
            lyrics_enabled: cfg.default_lyrics,
            karaoke_cancel: None,
            pending_search: Vec::new(),
            volume: Arc::new(AtomicU32::new(u32::from(cfg.volume))),
            threads: ThreadWatch::default(),
        }
    }
}

/// Where a command's replies go. All-None means the room timeline.
#[derive(Debug, Clone, Default)]
pub(crate) struct ReplyCtx {
    pub(crate) thread_root: Option<String>,
    pub(crate) source_event: Option<String>,
}

/// Who sent the command and where the reply should land.
#[derive(Debug, Clone)]
pub(crate) struct CmdCtx {
    pub(crate) room_id: String,
    pub(crate) actor_id: String,
    pub(crate) actor_display: String,
    pub(crate) reply: ReplyCtx,
}

/// Reply target for a command message: an existing thread root wins, else
/// the message itself roots a new thread. Takes fields, not &Message: the
/// body has already been moved by dispatch time.
fn reply_ctx(msg_id: &str, thread_root: &str) -> ReplyCtx {
    let root = if thread_root.is_empty() {
        msg_id
    } else {
        thread_root
    };
    if root.is_empty() {
        return ReplyCtx::default();
    }
    ReplyCtx {
        thread_root: Some(root.to_owned()),
        source_event: (!msg_id.is_empty()).then(|| msg_id.to_owned()),
    }
}

/// Seen event IDs with a hard cap, so the duplicate filter cannot grow
/// without bounds.
#[derive(Debug)]
struct RecentEvents {
    seen: HashSet<String>,
    order: VecDeque<String>,
}

impl RecentEvents {
    fn mark_seen(&mut self, id: String) -> bool {
        if !self.seen.insert(id.clone()) {
            return true;
        }
        self.order.push_back(id);
        while self.order.len() > SEEN_EVENTS_CAP {
            if let Some(old) = self.order.pop_front() {
                self.seen.remove(&old);
            }
        }
        false
    }
}

pub struct Bot {
    pub(crate) cfg: BotConfig,
    pub(crate) livekit_url: String,
    pub(crate) chatto: ChattoClient,
    pub(crate) tidal: Arc<Mutex<TidalClient>>,
    pub(crate) rooms: Arc<Mutex<HashMap<String, RoomState>>>,
    shutdown: Arc<AtomicBool>,
    pub(crate) started_at: DateTime<Utc>,
    seen_events: Arc<Mutex<RecentEvents>>,
    bot_user_id: Arc<Mutex<Option<String>>>,
    bot_login: Arc<Mutex<String>>,
}

impl Bot {
    pub fn new(
        cfg: BotConfig,
        livekit_url: String,
        chatto: ChattoClient,
        tidal: TidalClient,
    ) -> Self {
        let rooms = cfg
            .rooms
            .iter()
            .map(|room_id| (room_id.clone(), RoomState::new(&cfg)))
            .collect();

        Self {
            cfg,
            livekit_url,
            chatto,
            tidal: Arc::new(Mutex::new(tidal)),
            rooms: Arc::new(Mutex::new(rooms)),
            shutdown: Arc::new(AtomicBool::new(false)),
            started_at: Utc::now(),
            seen_events: Arc::new(Mutex::new(RecentEvents {
                seen: HashSet::new(),
                order: VecDeque::new(),
            })),
            bot_user_id: Arc::new(Mutex::new(None)),
            bot_login: Arc::new(Mutex::new(String::new())),
        }
    }

    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
    }

    pub async fn run(&self) -> Result<(), Error> {
        self.set_presence().await;

        match self.chatto.get_profile().await {
            Ok(profile) => {
                *self.bot_user_id.lock().await = Some(profile.id);
                let login = profile.login.unwrap_or_default();
                info!(login = %login, "authenticated as bot");
                *self.bot_login.lock().await = login;
            }
            Err(err) => {
                warn!(
                    error = %err,
                    "could not get bot profile, own messages will not be filtered and mentions may not be detected"
                );
            }
        }

        self.setup_avatar().await;
        self.join_rooms().await;

        let mut presence_tick = tokio::time::interval(Duration::from_secs(45));

        info!(rooms = ?self.cfg.rooms, "bot runtime started");

        loop {
            if self.shutdown.load(Ordering::SeqCst) {
                info!("bot runtime shutdown requested");
                self.shutdown_all().await;
                return Ok(());
            }

            let poll_dur = self.current_poll_interval().await;

            tokio::select! {
                _ = tokio::time::sleep(poll_dur) => {
                    self.reap_voice_tasks().await;
                    self.poll_all_rooms().await;
                    self.auto_prepare_playback().await;
                }
                _ = presence_tick.tick() => {
                    self.set_presence().await;
                }
            }
        }
    }

    /// Poll faster while a call is live so commands feel instant.
    async fn current_poll_interval(&self) -> Duration {
        let rooms = self.rooms.lock().await;
        let active = rooms
            .values()
            .any(|rs| rs.voice.as_ref().is_some_and(|v| !v.handle.is_finished()));
        if active {
            self.cfg.poll_interval.min(Duration::from_secs(1))
        } else {
            self.cfg.poll_interval
        }
    }

    /// Cancel karaoke and voice tasks and wait for them to leave the call.
    async fn shutdown_all(&self) {
        let handles = {
            let mut rooms = self.rooms.lock().await;
            let mut handles = Vec::new();
            for rs in rooms.values_mut() {
                if let Some(cancel) = rs.karaoke_cancel.take() {
                    cancel.store(true, Ordering::SeqCst);
                }
                if let Some(voice) = rs.voice.take() {
                    voice.cancel.store(true, Ordering::SeqCst);
                    voice.song_cancel.store(true, Ordering::SeqCst);
                    handles.push(voice.handle);
                }
            }
            handles
        };

        for handle in handles {
            if tokio::time::timeout(Duration::from_secs(5), handle)
                .await
                .is_err()
            {
                warn!("voice task did not finish within shutdown timeout");
            }
        }
    }

    async fn setup_avatar(&self) {
        let profile = match self.chatto.get_profile().await {
            Ok(p) => p,
            Err(err) => {
                warn!(error = %err, "failed to get profile");
                return;
            }
        };

        if profile.avatar_url.is_some() {
            info!("avatar already set");
            return;
        }

        let path = std::env::current_dir()
            .unwrap_or_default()
            .join("assets")
            .join("tidal.jpg");

        let data = match tokio::fs::read(&path).await {
            Ok(d) => d,
            Err(err) => {
                warn!(path = %path.display(), error = %err, "failed to read avatar image");
                return;
            }
        };

        // Chatto 0.5: UserService.UploadAvatar requires a target user ID and
        // a bot key can only target its own account.
        match self.chatto.upload_avatar(&profile.id, &data).await {
            Ok(()) => info!("avatar set successfully"),
            Err(err) => warn!(error = %err, "failed to set avatar"),
        }
    }

    async fn join_rooms(&self) {
        // Chatto 0.5 bots self-join through RoomService.JoinRoom once the bot
        // has an effective room.join grant for the room (Server Admin -> Bots).
        for room_id in &self.cfg.rooms {
            match self.chatto.join_room(room_id).await {
                Ok(()) => info!(room = room_id, "joined room"),
                Err(err) => warn!(
                    room = room_id,
                    error = %err,
                    "failed to join room (check the bot's room.join permission)"
                ),
            }
        }
    }

    /// Sets the bot's live presence. Custom status is not used: Chatto 0.5
    /// only allows `SetCustomStatus` for human accounts.
    async fn set_presence(&self) {
        if let Err(err) = self
            .chatto
            .set_presence("PRESENCE_STATUS_ONLINE", true)
            .await
        {
            warn!(error = %err, "failed to set online presence");
        }
    }

    async fn poll_all_rooms(&self) {
        for room_id in &self.cfg.rooms {
            if let Err(err) = self.poll_room(room_id).await {
                error!(room = room_id, error = %err, "room poll failed");
            }
        }
    }

    async fn poll_room(&self, room_id: &str) -> Result<(), Error> {
        let (after, watched_threads) = {
            let rooms = self.rooms.lock().await;
            match rooms.get(room_id) {
                Some(rs) => (rs.cursor.clone(), rs.threads.snapshot()),
                None => return Ok(()),
            }
        };

        match self.chatto.get_room_events(room_id, &after, 50).await {
            Ok(resp) => {
                if let Some(page) = resp.page {
                    let users = page.includes.as_ref().map(|inc| &inc.users);

                    {
                        let mut rooms = self.rooms.lock().await;
                        if let Some(rs) = rooms.get_mut(room_id) {
                            rs.cursor = page.end_cursor;
                        }
                    }

                    for event in page.events {
                        self.process_event(room_id, event, users, None).await?;
                    }
                }
            }
            Err(err)
                if chatto::is_not_member_error(&err)
                    || chatto::is_permission_denied_error(&err) =>
            {
                warn!(room = room_id, error = %err, "bot not a member or lacks permission");
                return Ok(());
            }
            Err(err) => return Err(Error::Chatto(err)),
        }

        // Poll the threads the bot follows; room events do not include
        // their replies.
        for (root, cursor) in watched_threads {
            match self
                .chatto
                .get_thread_events(room_id, &root, &cursor, THREAD_POLL_LIMIT)
                .await
            {
                Ok(resp) => {
                    if let Some(page) = resp.page {
                        let users = page.includes.as_ref().map(|inc| &inc.users);
                        {
                            let mut rooms = self.rooms.lock().await;
                            if let Some(rs) = rooms.get_mut(room_id) {
                                rs.threads.advance(&root, page.end_cursor);
                            }
                        }
                        for event in page.events {
                            // The first page repeats the root message; it was
                            // handled through the room timeline.
                            if event.id == root {
                                continue;
                            }
                            if let Err(err) =
                                self.process_event(room_id, event, users, Some(&root)).await
                            {
                                error!(
                                    room = room_id,
                                    thread = %root,
                                    error = %err,
                                    "thread event failed"
                                );
                            }
                        }
                    }
                }
                Err(err)
                    if chatto::is_not_member_error(&err)
                        || chatto::is_permission_denied_error(&err) =>
                {
                    // Thread deleted or access lost: stop polling it.
                    let mut rooms = self.rooms.lock().await;
                    if let Some(rs) = rooms.get_mut(room_id) {
                        rs.threads.unwatch(&root);
                    }
                }
                Err(err) => {
                    warn!(room = room_id, thread = %root, error = %err, "thread poll failed");
                }
            }
        }

        Ok(())
    }

    async fn process_event(
        &self,
        room_id: &str,
        event: RoomTimelineEvent,
        users: Option<&HashMap<String, UserProfile>>,
        thread_hint: Option<&str>,
    ) -> Result<(), Error> {
        let Some(message_posted) = event.message_posted else {
            return Ok(());
        };

        // Channel echo of a thread reply; the original comes through the
        // thread poll.
        if !message_posted.message.echo_of_event_id.is_empty() {
            return Ok(());
        }

        {
            let mut seen = self.seen_events.lock().await;
            if seen.mark_seen(event.id.clone()) {
                return Ok(());
            }
        }

        {
            let uid = self.bot_user_id.lock().await;
            if uid.as_deref() == Some(&message_posted.message.actor_id) {
                return Ok(());
            }
        }

        if let Some(event_time) = parse_event_time(&event.created_at)
            && event_time < self.started_at
        {
            return Ok(());
        }

        let Some(body) = message_posted.message.body else {
            return Ok(());
        };
        if body.trim().is_empty() {
            return Ok(());
        }

        // A good bot stays out of normal conversation: only react when it is
        // addressed, i.e. mentioned (@bot) or via an explicit "/command".
        let addressed = {
            let login = self.bot_login.lock().await;
            is_addressed(&body, &[login.as_str(), self.cfg.bot_name.as_str()])
        };
        if !addressed {
            return Ok(());
        }

        let Some(parsed) = parse_command(&body, &self.cfg.bot_name) else {
            self.send_message(
                room_id,
                &card(
                    "Help",
                    "Unknown command.\nTry /chatto-tidal help for the full list.",
                ),
            )
            .await;
            return Ok(());
        };

        let actor_id = &message_posted.message.actor_id;
        let actor_display = users
            .and_then(|u| u.get(actor_id))
            .and_then(|p| p.display_name.as_deref())
            .unwrap_or(actor_id);

        info!(
            cmd = ?parsed.command,
            args = parsed.args,
            actor = format!("{}[{}]", actor_display, actor_id),
            "processing command"
        );

        // Reply in the thread the command came from, or root a new thread
        // at the command itself.
        let reply = if self.cfg.thread_replies {
            let mut rc = reply_ctx(
                &message_posted.message.id,
                &message_posted.message.thread_root_event_id,
            );
            if rc.thread_root.is_none()
                && let Some(hint) = thread_hint
            {
                rc.thread_root = Some(hint.to_owned());
                rc.source_event = (!message_posted.message.id.is_empty())
                    .then(|| message_posted.message.id.clone());
            }
            rc
        } else {
            ReplyCtx::default()
        };

        // Keep following the thread the command came from, even when replies
        // themselves go to the room.
        let watch_root = thread_hint.map(str::to_owned).or_else(|| {
            (!message_posted.message.thread_root_event_id.is_empty())
                .then(|| message_posted.message.thread_root_event_id.clone())
        });
        if let Some(root) = watch_root.as_deref().or(reply.thread_root.as_deref()) {
            self.watch_thread(room_id, root).await;
        }

        let ctx = CmdCtx {
            room_id: room_id.to_owned(),
            actor_id: actor_id.clone(),
            actor_display: actor_display.to_owned(),
            reply,
        };

        match parsed.command {
            Command::Help => {
                self.cmd_help(&ctx, &parsed.args).await;
            }
            Command::Play | Command::Queue => {
                self.cmd_queue(&ctx, &parsed.args).await?;
            }
            Command::Pick => {
                self.cmd_pick(&ctx, &parsed.args).await?;
            }
            Command::PlayNext => {
                self.cmd_playnext(&ctx, &parsed.args).await?;
            }
            Command::NowPlaying => {
                self.cmd_now_playing(&ctx).await;
            }
            Command::Skip => {
                self.cmd_skip(&ctx).await;
            }
            Command::Stop => {
                self.cmd_stop(&ctx).await;
            }
            Command::Remove => {
                self.cmd_remove(&ctx, &parsed.args).await;
            }
            Command::Move => {
                self.cmd_move(&ctx, &parsed.args).await;
            }
            Command::Shuffle => {
                self.cmd_shuffle(&ctx).await;
            }
            Command::Repeat => {
                self.cmd_repeat(&ctx, &parsed.args).await;
            }
            Command::Volume => {
                self.cmd_volume(&ctx, &parsed.args).await;
            }
            Command::Mute => {
                self.cmd_mute(&ctx).await;
            }
            Command::Test => {
                self.cmd_test(&ctx).await;
            }
            Command::Lyrics => {
                self.cmd_lyrics(&ctx).await;
            }
            Command::Stats => {
                self.cmd_stats(&ctx).await;
            }
            Command::Version => {
                self.reply(&ctx, &card("Version", crate::version())).await;
            }
        }

        Ok(())
    }

    async fn auto_prepare_playback(&self) {
        for room_id in &self.cfg.rooms {
            let (track, has_current) = {
                let mut rooms = self.rooms.lock().await;
                let Some(rs) = rooms.get_mut(room_id) else {
                    continue;
                };
                let has_current = rs.current.is_some();
                if has_current {
                    (None, true)
                } else {
                    (rs.queue.dequeue(), false)
                }
            };

            if has_current {
                continue;
            }

            let Some(track) = track else {
                continue;
            };

            let stream = {
                let tidal = self.tidal.lock().await;
                match tidal.stream_track(track.tid).await {
                    Ok(stream) => stream,
                    Err(err) => {
                        error!(room = room_id, track_id = track.tid, error = %err, "failed to resolve stream");
                        self.send_message(room_id, &card("Stream Error", &err.to_string()))
                            .await;
                        continue;
                    }
                }
            };

            let need_new_voice = {
                let mut rooms = self.rooms.lock().await;
                let Some(rs) = rooms.get_mut(room_id) else {
                    continue;
                };
                rs.current = Some(CurrentTrack {
                    track: track.clone(),
                    audio_info: stream.format_audio_info(),
                    stream_url: stream.stream_url.clone(),
                });
                if rs.lyrics_enabled {
                    if let Some(ref cancel) = rs.karaoke_cancel {
                        cancel.store(true, Ordering::SeqCst);
                    }
                    rs.karaoke_cancel = None;
                }
                let voice_alive = rs.voice.as_ref().is_some_and(|v| !v.handle.is_finished());
                if voice_alive && let Some(ref v) = rs.voice {
                    *v.next_url.lock().await = Some(stream.stream_url.clone());
                }
                !voice_alive
            };

            if need_new_voice {
                self.ensure_voice(room_id).await;
            }

            {
                let rooms = self.rooms.lock().await;
                let should_start = rooms.get(room_id).is_some_and(|rs| {
                    rs.lyrics_enabled && rs.current.is_some() && rs.karaoke_cancel.is_none()
                });
                if should_start {
                    drop(rooms);
                    self.start_karaoke(room_id).await;
                }
            }
        }
    }

    pub(crate) async fn send_message(&self, room_id: &str, text: &str) {
        if let Err(err) = self.chatto.create_message(room_id, text).await {
            error!(room = room_id, error = %err, "failed to send message");
        }
    }

    /// Send a command response to its thread; room timeline if thread
    /// replies are off or the post fails (e.g. missing
    /// `message.post-in-thread`).
    pub(crate) async fn reply(&self, ctx: &CmdCtx, text: &str) {
        if let Some(root) = &ctx.reply.thread_root {
            match self
                .chatto
                .create_thread_reply(&ctx.room_id, text, root, ctx.reply.source_event.as_deref())
                .await
            {
                Ok(()) => return,
                Err(err) => warn!(
                    room = ctx.room_id,
                    error = %err,
                    "thread reply failed, posting to the room timeline instead"
                ),
            }
        }
        self.send_message(&ctx.room_id, text).await;
    }

    /// Track a thread for polling; follow it on the server the first time.
    async fn watch_thread(&self, room_id: &str, root: &str) {
        let newly = {
            let mut rooms = self.rooms.lock().await;
            match rooms.get_mut(room_id) {
                Some(rs) => rs.threads.watch(root),
                None => false,
            }
        };
        if newly && let Err(err) = self.chatto.follow_thread(room_id, root).await {
            // The follow only feeds notifications; polling works without it.
            warn!(room = room_id, thread = root, error = %err, "follow thread failed");
        }
    }
}

fn parse_event_time(s: &str) -> Option<DateTime<Utc>> {
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Some(dt.with_timezone(&Utc));
    }
    let naive = NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f").ok()?;
    Some(DateTime::from_naive_utc_and_offset(naive, Utc))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_event_time_accepts_rfc3339() {
        let got = parse_event_time("2026-01-02T03:04:05Z").unwrap();
        let want = DateTime::parse_from_rfc3339("2026-01-02T03:04:05Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(got, want);
    }

    #[test]
    fn parse_event_time_converts_offsets_to_utc() {
        let got = parse_event_time("2026-01-02T05:04:05+02:00").unwrap();
        let want = DateTime::parse_from_rfc3339("2026-01-02T03:04:05Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(got, want);
    }

    #[test]
    fn parse_event_time_assumes_naive_is_utc() {
        let got = parse_event_time("2026-01-02T03:04:05").unwrap();
        let want = DateTime::parse_from_rfc3339("2026-01-02T03:04:05Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(got, want);

        // Fractional seconds are accepted by the fallback parser.
        let got = parse_event_time("2026-01-02T03:04:05.500").unwrap();
        let want = DateTime::parse_from_rfc3339("2026-01-02T03:04:05.500Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(got, want);
    }

    #[test]
    fn parse_event_time_rejects_garbage() {
        assert!(parse_event_time("").is_none());
        assert!(parse_event_time("garbage").is_none());
        assert!(parse_event_time("2026-01-02").is_none());
    }

    #[test]
    fn recent_events_dedups_within_cap_and_evicts_oldest() {
        let mut recent = RecentEvents {
            seen: HashSet::new(),
            order: VecDeque::new(),
        };
        assert!(!recent.mark_seen("a".into()));
        assert!(recent.mark_seen("a".into()));
        assert!(!recent.mark_seen("b".into()));
        assert_eq!(recent.seen.len(), 2);

        // Fill past the cap: old ids drop out and become reusable.
        for i in 0..SEEN_EVENTS_CAP + 10 {
            recent.mark_seen(format!("id-{i}"));
        }
        assert!(recent.seen.len() <= SEEN_EVENTS_CAP);
        assert!(recent.order.len() <= SEEN_EVENTS_CAP);
        assert!(!recent.mark_seen("a".into())); // long evicted
    }

    #[test]
    fn reply_ctx_threads_root_level_messages() {
        let rc = reply_ctx("evt_1", "");
        assert_eq!(rc.thread_root.as_deref(), Some("evt_1"));
        assert_eq!(rc.source_event.as_deref(), Some("evt_1"));
    }

    #[test]
    fn reply_ctx_reuses_existing_thread_root() {
        let rc = reply_ctx("evt_reply", "evt_root");
        assert_eq!(rc.thread_root.as_deref(), Some("evt_root"));
        assert_eq!(rc.source_event.as_deref(), Some("evt_reply"));
    }

    #[test]
    fn reply_ctx_without_ids_falls_back_to_room() {
        let rc = reply_ctx("", "");
        assert!(rc.thread_root.is_none());
        assert!(rc.source_event.is_none());
    }

    #[test]
    fn thread_watch_is_idempotent_and_reports_newness() {
        let mut watch = ThreadWatch::default();
        assert!(watch.watch("t1"));
        assert!(!watch.watch("t1"));
        assert_eq!(watch.snapshot(), vec![("t1".to_owned(), String::new())]);
    }

    #[test]
    fn thread_watch_advances_cursor_and_snapshots_order() {
        let mut watch = ThreadWatch::default();
        watch.watch("t1");
        watch.watch("t2");
        watch.advance("t1", "c1".to_owned());
        // Empty cursors (page without one yet) must not reset progress.
        watch.advance("t2", String::new());

        let snap = watch.snapshot();
        assert_eq!(snap[0], ("t1".to_owned(), "c1".to_owned()));
        assert_eq!(snap[1], ("t2".to_owned(), String::new()));
    }

    #[test]
    fn thread_watch_evicts_oldest_past_cap() {
        let mut watch = ThreadWatch::default();
        for i in 0..MAX_WATCHED_THREADS + 5 {
            watch.watch(&format!("t{i}"));
        }
        assert_eq!(watch.cursors.len(), MAX_WATCHED_THREADS);
        assert_eq!(watch.order.len(), MAX_WATCHED_THREADS);
        // The first five are gone; the last registered stays.
        assert!(!watch.cursors.contains_key("t0"));
        assert!(
            watch
                .cursors
                .contains_key(&format!("t{}", MAX_WATCHED_THREADS + 4))
        );
    }

    #[test]
    fn thread_watch_unwatch_drops_cursor_and_order() {
        let mut watch = ThreadWatch::default();
        watch.watch("t1");
        watch.watch("t2");
        watch.advance("t1", "c1".to_owned());
        watch.unwatch("t1");
        assert_eq!(watch.snapshot().len(), 1);
        assert!(watch.snapshot()[0].0 == "t2");
        // Re-watching starts from scratch again.
        assert!(watch.watch("t1"));
        assert_eq!(watch.snapshot()[1], ("t1".to_owned(), String::new()));
    }
}
