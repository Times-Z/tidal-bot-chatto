use crate::commands::{Command, command_help, is_addressed, parse_command};
use crate::karaoke::{self, KaraokeRenderer};
use crate::queue::{Queue, RepeatMode, Track};
use arc_swap::ArcSwapOption;
use chatto::{Client as ChattoClient, RoomTimelineEvent, UserProfile};
use chrono::{DateTime, NaiveDateTime, Utc};
use livekit_audio::Player as LivekitPlayer;
use livekit_video::{LocalVideoTrack, NativeVideoSource, Room as LivekitRoom};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration, Instant};
use tidal::Client as TidalClient;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tracing::{error, info, warn};

/// Event IDs remembered for dedup; oldest fall out past the cap.
const SEEN_EVENTS_CAP: usize = 4096;

/// Search results listed by `play` before `pick`.
const SEARCH_PICK_LIMIT: usize = 5;

/// Pending tracks listed by `queue` before truncation.
const QUEUE_DISPLAY_LIMIT: usize = 15;

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
struct CurrentTrack {
    track: Track,
    audio_info: String,
    stream_url: String,
}

/// Whether a queued track goes to the tail or right after the current song.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QueuePlacement {
    Tail,
    Next,
}

/// Parse a 1-based index argument (first whitespace-separated token).
fn parse_index(s: &str) -> Option<usize> {
    s.split_whitespace()
        .next()?
        .parse::<usize>()
        .ok()
        .filter(|n| *n > 0)
}

/// Published screenshare track plus its frame source. Both are kept so the
/// track can be unpublished later.
#[derive(Debug)]
struct VideoTrack {
    source: NativeVideoSource,
    track: LocalVideoTrack,
}

#[derive(Debug)]
struct VoiceConnection {
    handle: JoinHandle<()>,
    cancel: Arc<AtomicBool>,
    song_cancel: Arc<AtomicBool>,
    next_url: Arc<Mutex<Option<String>>>,
    video: Arc<Mutex<Option<VideoTrack>>>,
    /// Raised by `/lyrics`; the voice task unpublishes on it (only it holds
    /// the Room).
    video_unpublish: Arc<AtomicBool>,
    /// When the current track started playing; None between tracks. Karaoke
    /// reads this to stay in sync when enabled mid-song.
    play_start: Arc<ArcSwapOption<Instant>>,
}

/// Thread timelines the bot follows, each with a poll cursor. Thread replies
/// do not show up in the room timeline, so followed threads are polled
/// separately. Capped per room, oldest dropped first.
#[derive(Debug, Default)]
struct ThreadWatch {
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
struct RoomState {
    queue: Queue,
    current: Option<CurrentTrack>,
    cursor: String,
    muted: Arc<AtomicBool>,
    voice: Option<VoiceConnection>,
    lyrics_enabled: bool,
    karaoke_cancel: Option<Arc<AtomicBool>>,
    /// Results of the last ambiguous search, awaiting `/pick <n>`.
    pending_search: Vec<tidal::SearchResult>,
    /// Playback volume for this room only (0-200 percent).
    volume: Arc<AtomicU32>,
    /// Thread timelines polled for commands alongside the room timeline.
    threads: ThreadWatch,
}

impl RoomState {
    fn new(cfg: &BotConfig) -> Self {
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
struct ReplyCtx {
    thread_root: Option<String>,
    source_event: Option<String>,
}

/// Who sent the command and where the reply should land.
#[derive(Debug, Clone)]
struct CmdCtx {
    room_id: String,
    actor_id: String,
    actor_display: String,
    reply: ReplyCtx,
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
    cfg: BotConfig,
    livekit_url: String,
    chatto: ChattoClient,
    tidal: Arc<Mutex<TidalClient>>,
    rooms: Arc<Mutex<HashMap<String, RoomState>>>,
    shutdown: Arc<AtomicBool>,
    started_at: DateTime<Utc>,
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

    /// Search (or resolve a link) and enqueue. When a text search returns
    /// several candidates, they are listed and the user confirms one with
    /// `pick <n>`.
    async fn cmd_queue(&self, ctx: &CmdCtx, query: &str) -> Result<(), Error> {
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
    async fn cmd_pick(&self, ctx: &CmdCtx, args: &str) -> Result<(), Error> {
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
    async fn cmd_playnext(&self, ctx: &CmdCtx, query: &str) -> Result<(), Error> {
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
    async fn cmd_queue_link(&self, ctx: &CmdCtx, url: &str, next: bool) -> Result<(), Error> {
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
    ) -> Result<(), Error> {
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

    async fn cmd_now_playing(&self, ctx: &CmdCtx) {
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

    async fn cmd_skip(&self, ctx: &CmdCtx) {
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

    async fn cmd_stop(&self, ctx: &CmdCtx) {
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

    async fn cmd_volume(&self, ctx: &CmdCtx, args: &str) {
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

    async fn cmd_mute(&self, ctx: &CmdCtx) {
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

    async fn cmd_remove(&self, ctx: &CmdCtx, args: &str) {
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

    async fn cmd_move(&self, ctx: &CmdCtx, args: &str) {
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

    async fn cmd_shuffle(&self, ctx: &CmdCtx) {
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

    async fn cmd_repeat(&self, ctx: &CmdCtx, args: &str) {
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

    async fn cmd_stats(&self, ctx: &CmdCtx) {
        let uptime = Utc::now().signed_duration_since(self.started_at);
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

    async fn cmd_help(&self, ctx: &CmdCtx, args: &str) {
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

    async fn ensure_voice(&self, room_id: &str) {
        let (stream_url, muted, volume) = {
            let rooms = self.rooms.lock().await;
            let Some(rs) = rooms.get(room_id) else {
                return;
            };
            let Some(ref current) = rs.current else {
                return;
            };
            if rs.voice.as_ref().is_some_and(|v| !v.handle.is_finished()) {
                return;
            }
            (
                current.stream_url.clone(),
                Arc::clone(&rs.muted),
                Arc::clone(&rs.volume),
            )
        };

        let room_id_owned = room_id.to_owned();
        let livekit_url = self.livekit_url.clone();
        let sample_rate = self.cfg.sample_rate;
        let chatto = self.chatto.clone();
        let voice_cancel = Arc::new(AtomicBool::new(false));
        let song_cancel = Arc::new(AtomicBool::new(false));
        let next_url: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(Some(stream_url)));

        let video: Arc<Mutex<Option<VideoTrack>>> = Arc::new(Mutex::new(None));
        let vs_task = video.clone();
        let video_unpublish = Arc::new(AtomicBool::new(false));
        let unpub_task = video_unpublish.clone();
        let play_start: Arc<ArcSwapOption<Instant>> = Arc::new(ArcSwapOption::empty());
        let ps_task = play_start.clone();

        let vc = Arc::clone(&voice_cancel);
        let sc = Arc::clone(&song_cancel);
        let nu = Arc::clone(&next_url);
        let mu = Arc::clone(&muted);
        let vp = volume;
        let db = Arc::clone(&self.rooms);
        let tidal = Arc::clone(&self.tidal);
        let rid = room_id_owned.clone();
        let ch = chatto.clone();

        let handle = tokio::spawn(async move {
            let joined = match ch.join_call(&rid).await {
                Ok(j) => j,
                Err(e) => {
                    warn!(room = rid, error = %e, "join call failed");
                    mark_voice_dead(&db, &rid).await;
                    return;
                }
            };
            if !joined {
                warn!(room = rid, "voice channel required");
                mark_voice_dead(&db, &rid).await;
                return;
            }

            let token = match ch.create_call_token(&rid).await {
                Ok(t) => t,
                Err(e) => {
                    warn!(room = rid, error = %e, "get call token failed");
                    mark_voice_dead(&db, &rid).await;
                    let _ = ch.leave_call(&rid).await;
                    return;
                }
            };

            let mut player = match LivekitPlayer::new(livekit_audio::Config {
                url: livekit_url,
                token: token.token,
                room: rid.clone(),
                sample_rate,
            })
            .await
            {
                Ok(p) => p,
                Err(e) => {
                    warn!(room = rid, error = %e, "livekit init failed");
                    mark_voice_dead(&db, &rid).await;
                    let _ = ch.leave_call(&rid).await;
                    return;
                }
            };

            let source = match player.publish_track("mic").await {
                Ok(s) => s,
                Err(e) => {
                    warn!(room = rid, error = %e, "publish track failed");
                    player.disconnect().await;
                    mark_voice_dead(&db, &rid).await;
                    let _ = ch.leave_call(&rid).await;
                    return;
                }
            };

            // Screenshare up front if lyrics is already on.
            let lyrics_enabled = {
                let rooms = db.lock().await;
                rooms.get(&rid).is_some_and(|rs| rs.lyrics_enabled)
            };
            sync_video_track(player.room(), &vs_task, &unpub_task, lyrics_enabled).await;

            info!(room = rid, "voice connected");

            let sample_rate_p = player.sample_rate;

            loop {
                if vc.load(Ordering::SeqCst) {
                    break;
                }

                // Catch `/lyrics` toggles that happened between tracks.
                {
                    let lyrics_enabled = {
                        let rooms = db.lock().await;
                        rooms.get(&rid).is_some_and(|rs| rs.lyrics_enabled)
                    };
                    sync_video_track(player.room(), &vs_task, &unpub_task, lyrics_enabled).await;
                }

                let url = nu.lock().await.take();

                match url {
                    Some(url) => {
                        sc.store(false, Ordering::SeqCst);

                        // Announce the track and who requested it.
                        let announce = {
                            let rooms = db.lock().await;
                            rooms
                                .get(&rid)
                                .and_then(|rs| rs.current.as_ref())
                                .map(|cur| {
                                    let who = if cur.track.requestor_name.is_empty() {
                                        String::new()
                                    } else {
                                        format!("\n↳ requested by {}", cur.track.requestor_name)
                                    };
                                    let audio = if cur.audio_info.is_empty() {
                                        String::new()
                                    } else {
                                        format!(" · {}", cur.audio_info)
                                    };
                                    card(
                                        "Now Playing",
                                        &format!(
                                            "{} · {}\n{}{audio}{who}",
                                            cur.track.title,
                                            cur.track.artist,
                                            format_duration(cur.track.duration),
                                        ),
                                    )
                                })
                        };
                        if let Some(msg) = announce
                            && let Err(e) = ch.create_message(&rid, &msg).await
                        {
                            warn!(room = rid, error = %e, "now playing announcement failed");
                        }

                        // Tidal URLs expire: re-resolve once on failure.
                        let mut attempt = 0u8;
                        let mut url = url;
                        let result = loop {
                            ps_task.store(Some(Arc::new(Instant::now())));

                            // Poll `/lyrics` while the track plays. The block
                            // also ends the url borrow before a retry can
                            // reassign it.
                            let res = {
                                let mut play = std::pin::pin!(LivekitPlayer::play_url_on_source(
                                    &source,
                                    &url,
                                    sample_rate_p,
                                    &vp,
                                    &sc,
                                    &mu,
                                ));
                                loop {
                                    {
                                        let lyrics_enabled = {
                                            let rooms = db.lock().await;
                                            rooms.get(&rid).is_some_and(|rs| rs.lyrics_enabled)
                                        };
                                        sync_video_track(
                                            player.room(),
                                            &vs_task,
                                            &unpub_task,
                                            lyrics_enabled,
                                        )
                                        .await;
                                    }
                                    tokio::select! {
                                        r = play.as_mut() => break r,
                                        _ = tokio::time::sleep(Duration::from_millis(200)) => {}
                                    }
                                }
                            };

                            match res {
                                Ok(()) => {
                                    // A natural finish honours the repeat mode.
                                    let mut rooms = db.lock().await;
                                    if let Some(rs) = rooms.get_mut(&rid)
                                        && let Some(cur) = &rs.current
                                    {
                                        rs.queue.on_track_ended(&cur.track);
                                    }
                                    break Ok(());
                                }
                                Err(livekit_audio::Error::Cancelled) => {
                                    break Err(livekit_audio::Error::Cancelled);
                                }
                                Err(e) => {
                                    if attempt > 0 || sc.load(Ordering::SeqCst) {
                                        break Err(e);
                                    }
                                    attempt = 1;
                                    warn!(
                                        room = rid,
                                        error = %e,
                                        "playback failed, re-resolving stream url"
                                    );
                                    let tid = {
                                        let rooms = db.lock().await;
                                        rooms
                                            .get(&rid)
                                            .and_then(|rs| rs.current.as_ref())
                                            .map(|c| c.track.tid)
                                    };
                                    let fresh = match tid {
                                        Some(tid) => {
                                            let client = tidal.lock().await;
                                            client.stream_track(tid).await.map(|s| s.stream_url)
                                        }
                                        None => break Err(e),
                                    };
                                    match fresh {
                                        Ok(fresh) => url = fresh,
                                        Err(e2) => {
                                            error!(
                                                room = rid,
                                                error = %e2,
                                                "stream re-resolve failed"
                                            );
                                            break Err(e);
                                        }
                                    }
                                }
                            }
                        };

                        ps_task.store(None);

                        let mut rooms = db.lock().await;
                        if let Some(rs) = rooms.get_mut(&rid) {
                            rs.current = None;
                            if let Some(ref cancel) = rs.karaoke_cancel {
                                cancel.store(true, Ordering::SeqCst);
                            }
                            rs.karaoke_cancel = None;
                        }
                        drop(rooms);

                        if let Err(e) = result
                            && !matches!(e, livekit_audio::Error::Cancelled)
                        {
                            error!(room = rid, error = %e, "playback error");
                            if let Err(err) = ch
                                .create_message(&rid, &card("Playback Error", &e.to_string()))
                                .await
                            {
                                warn!(room = rid, error = %err, "failed to report playback error");
                            }
                        }
                    }
                    None => {
                        LivekitPlayer::send_silence_frame(&source, sample_rate_p)
                            .await
                            .ok();
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                }
            }

            player.disconnect().await;
            if let Err(e) = ch.leave_call(&rid).await {
                warn!(room = rid, error = %e, "leave call failed");
            }
            info!(room = rid, "voice disconnected");
        });

        {
            let mut rooms = self.rooms.lock().await;
            if let Some(rs) = rooms.get_mut(room_id) {
                rs.voice = Some(VoiceConnection {
                    handle,
                    cancel: voice_cancel,
                    song_cancel,
                    next_url,
                    video,
                    video_unpublish,
                    play_start,
                });
            }
        }
    }

    async fn reap_voice_tasks(&self) {
        for room_id in &self.cfg.rooms {
            let dead = {
                let mut rooms = self.rooms.lock().await;
                let Some(rs) = rooms.get_mut(room_id) else {
                    continue;
                };
                let dead = rs.voice.as_ref().is_some_and(|v| v.handle.is_finished());
                if dead {
                    rs.voice = None;
                    rs.current = None;
                }
                dead
            };

            if dead {
                error!(room = room_id, "voice task died unexpectedly");
                self.send_message(room_id, &card("Voice Error", "Voice connection lost."))
                    .await;
            }
        }
    }

    async fn send_message(&self, room_id: &str, text: &str) {
        if let Err(err) = self.chatto.create_message(room_id, text).await {
            error!(room = room_id, error = %err, "failed to send message");
        }
    }

    /// Send a command response to its thread; room timeline if thread
    /// replies are off or the post fails (e.g. missing
    /// `message.post-in-thread`).
    async fn reply(&self, ctx: &CmdCtx, text: &str) {
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

    async fn cmd_test(&self, ctx: &CmdCtx) {
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

    async fn cmd_lyrics(&self, ctx: &CmdCtx) {
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

    async fn start_karaoke(&self, room_id: &str) {
        let (track, tidal, rooms) = {
            let r = self.rooms.lock().await;
            let Some(rs) = r.get(room_id) else {
                return;
            };
            let Some(ref current) = rs.current else {
                return;
            };
            (
                current.track.clone(),
                self.tidal.clone(),
                self.rooms.clone(),
            )
        };

        let cancel = Arc::new(AtomicBool::new(false));
        {
            let mut r = self.rooms.lock().await;
            if let Some(rs) = r.get_mut(room_id) {
                rs.karaoke_cancel = Some(cancel.clone());
            }
        }

        let rid = room_id.to_owned();
        tokio::spawn(async move {
            let start = Instant::now();

            // Start sharing immediately with a gradient background and no
            // lyrics; real content is hot-swapped in as each fetch completes
            // so the screenshare never waits on the network to appear.
            let renderer = match KaraokeRenderer::new(
                create_gradient_background(1920, 1080),
                Arc::new(Vec::new()),
                track.title.clone(),
                track.artist.clone(),
                track.duration as f64,
            ) {
                Ok(r) => Arc::new(r),
                Err(e) => {
                    tracing::error!(error = e, "failed to create karaoke renderer");
                    return;
                }
            };

            {
                let tidal = tidal.clone();
                let renderer = Arc::clone(&renderer);
                let title = track.title.clone();
                let artist = track.artist.clone();
                let rid = rid.clone();
                tokio::spawn(async move {
                    if let Some(lines) =
                        karaoke::fetch_lyrics(&tidal, track.tid, &title, &artist).await
                    {
                        renderer.set_lyrics(Arc::unwrap_or_clone(lines));
                        tracing::info!(room = %rid, "karaoke lyrics ready");
                    }
                });
            }

            if !track.cover_url.is_empty() {
                let renderer = Arc::clone(&renderer);
                let cover_url = track.cover_url.clone();
                tokio::spawn(async move {
                    let bg = load_background(&cover_url).await;
                    renderer.set_background(bg);
                });
            }

            // Wait for the video source from the voice connection
            let (source, play_start) = loop {
                if cancel.load(Ordering::SeqCst) {
                    return;
                }
                let r = rooms.lock().await;
                if let Some(rs) = r.get(&rid)
                    && let Some(ref v) = rs.voice
                {
                    let guard = v.video.lock().await;
                    if let Some(ref vt) = *guard {
                        break (vt.source.clone(), v.play_start.clone());
                    }
                }
                drop(r);
                tokio::time::sleep(Duration::from_millis(50)).await;
            };

            tracing::info!(room = rid, "karaoke screenshare started");

            livekit_video::run_frame_loop(
                &source,
                livekit_video::DEFAULT_FPS,
                livekit_video::DEFAULT_WIDTH,
                livekit_video::DEFAULT_HEIGHT,
                &cancel,
                |_timestamp_us, w, h| {
                    // Anchor to the real playback start so a mid-song
                    // `/lyrics` toggle lands on the correct lyric line.
                    let elapsed = match play_start.load_full() {
                        Some(playing) => playing.elapsed().as_millis() as u64,
                        None => start.elapsed().as_millis() as u64,
                    };
                    let frame = renderer.render_frame(elapsed, w, h);
                    frame.into_raw()
                },
            )
            .await;

            tracing::info!(room = rid, "karaoke screenshare ended");
        });
    }
}

/// Clear playback state after a voice task gives up, so `auto_prepare` can
/// rebuild a fresh connection.
async fn mark_voice_dead(db: &Arc<Mutex<HashMap<String, RoomState>>>, rid: &str) {
    let mut rooms = db.lock().await;
    if let Some(rs) = rooms.get_mut(rid) {
        rs.current = None;
        rs.voice = None;
    }
}

/// Publish/unpublish the screenshare to match the `/lyrics` toggle. Only the
/// voice task owns the `Room`, so unpublishing has to happen here.
async fn sync_video_track(
    room: &LivekitRoom,
    video: &Arc<Mutex<Option<VideoTrack>>>,
    unpublish: &AtomicBool,
    lyrics_enabled: bool,
) {
    if unpublish.swap(false, Ordering::SeqCst) {
        let taken = video.lock().await.take();
        if let Some(vt) = taken {
            let _ = room
                .local_participant()
                .unpublish_track(&vt.track.sid())
                .await;
        }
    }
    if lyrics_enabled
        && video.lock().await.is_none()
        && let Ok((source, track)) = livekit_video::publish_video_track(
            room,
            "screenshare",
            livekit_video::DEFAULT_WIDTH,
            livekit_video::DEFAULT_HEIGHT,
        )
        .await
    {
        *video.lock().await = Some(VideoTrack { source, track });
    }
}

async fn load_background(url: &str) -> image::RgbaImage {
    let bytes = match reqwest::get(url).await {
        Ok(resp) if resp.status().is_success() => match resp.bytes().await {
            Ok(b) => b,
            Err(_) => return create_gradient_background(1920, 1080),
        },
        _ => return create_gradient_background(1920, 1080),
    };

    // Decoding + blurring are CPU-heavy: run off the async runtime so the
    // voice loop stays responsive. Blur at 1/6 scale (radius scales with it)
    // then upscale; visually identical for a background, far cheaper.
    tokio::task::spawn_blocking(move || -> image::RgbaImage {
        match image::load_from_memory(&bytes) {
            Ok(img) => {
                let small = img.resize_exact(320, 180, image::imageops::FilterType::Lanczos3);
                let blurred = image::imageops::blur(&small.to_rgba8(), 4.0);
                image::imageops::resize(&blurred, 1920, 1080, image::imageops::FilterType::Triangle)
            }
            Err(_) => create_gradient_background(1920, 1080),
        }
    })
    .await
    .unwrap_or_else(|_| create_gradient_background(1920, 1080))
}

fn create_gradient_background(w: u32, h: u32) -> image::RgbaImage {
    use image::Rgba;
    let mut img = image::RgbaImage::new(w, h);
    for y in 0..h {
        let t = y as f32 / h as f32;
        let r = (26.0 * (1.0 - t) + 22.0 * t) as u8;
        let g = (26.0 * (1.0 - t) + 33.0 * t) as u8;
        let b = (46.0 * (1.0 - t) + 62.0 * t) as u8;
        for x in 0..w {
            img.put_pixel(x, y, Rgba([r, g, b, 255]));
        }
    }
    img
}

fn parse_event_time(s: &str) -> Option<DateTime<Utc>> {
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Some(dt.with_timezone(&Utc));
    }
    let naive = NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f").ok()?;
    Some(DateTime::from_naive_utc_and_offset(naive, Utc))
}

/// Discord-style embed: a left bar, capitalized title, one `┃ ` line per
/// body line. No right border, so it survives proportional fonts and wrap.
fn card(title: &str, body: &str) -> String {
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

fn format_duration(seconds: i32) -> String {
    let minutes = seconds / 60;
    let seconds = seconds % 60;
    format!("{minutes}:{seconds:02}")
}

fn help_message() -> String {
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
    fn gradient_background_is_row_uniform_and_opaque() {
        let img = create_gradient_background(8, 4);
        assert_eq!(img.width(), 8);
        assert_eq!(img.height(), 4);

        // Top row starts from the fixed base color.
        assert_eq!(*img.get_pixel(0, 0), image::Rgba([26, 26, 46, 255]));
        // Bottom row interpolates toward the end color.
        assert_eq!(*img.get_pixel(7, 3), image::Rgba([23, 31, 58, 255]));

        // Rows are uniform horizontally and fully opaque.
        for y in 0..4 {
            let first = *img.get_pixel(0, y);
            for x in 0..8 {
                assert_eq!(*img.get_pixel(x, y), first);
                assert_eq!(first[3], 255);
            }
        }
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
