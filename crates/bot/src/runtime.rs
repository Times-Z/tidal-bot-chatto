use crate::commands::{Command, is_addressed, parse_command};
use crate::karaoke::{self, KaraokeRenderer};
use crate::queue::{Queue, Track};
use chatto::{Client as ChattoClient, RoomTimelineEvent, UserProfile};
use chrono::{DateTime, NaiveDateTime, Utc};
use livekit_audio::Player as LivekitPlayer;
use livekit_video::NativeVideoSource;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration, Instant};
use tidal::Client as TidalClient;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tracing::{error, info, warn};

#[derive(Debug)]
pub struct BotConfig {
    pub rooms: Vec<String>,
    pub poll_interval: Duration,
    pub bot_name: String,
    pub volume: u8,
    pub sample_rate: u32,
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

#[derive(Debug)]
struct VoiceConnection {
    handle: JoinHandle<()>,
    cancel: Arc<AtomicBool>,
    song_cancel: Arc<AtomicBool>,
    next_url: Arc<Mutex<Option<String>>>,
    video_source: Arc<Mutex<Option<NativeVideoSource>>>,
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
}

impl Default for RoomState {
    fn default() -> Self {
        Self {
            queue: Queue::default(),
            current: None,
            cursor: String::new(),
            muted: Arc::new(AtomicBool::new(false)),
            voice: None,
            lyrics_enabled: false,
            karaoke_cancel: None,
        }
    }
}

pub struct Bot {
    cfg: BotConfig,
    livekit_url: String,
    chatto: ChattoClient,
    tidal: Arc<Mutex<TidalClient>>,
    rooms: Arc<Mutex<HashMap<String, RoomState>>>,
    volume: Arc<Mutex<f64>>,
    volume_pct: Arc<AtomicU32>,
    shutdown: Arc<AtomicBool>,
    started_at: DateTime<Utc>,
    seen_events: Arc<Mutex<HashSet<String>>>,
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
            .map(|room_id| (room_id.clone(), RoomState::default()))
            .collect();

        Self {
            volume: Arc::new(Mutex::new(f64::from(cfg.volume) / 100.0)),
            volume_pct: Arc::new(AtomicU32::new(cfg.volume as u32)),
            cfg,
            livekit_url,
            chatto,
            tidal: Arc::new(Mutex::new(tidal)),
            rooms: Arc::new(Mutex::new(rooms)),
            shutdown: Arc::new(AtomicBool::new(false)),
            started_at: Utc::now(),
            seen_events: Arc::new(Mutex::new(HashSet::new())),
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

        let mut poll_tick = tokio::time::interval(self.cfg.poll_interval);
        let mut presence_tick = tokio::time::interval(Duration::from_secs(45));

        info!(rooms = ?self.cfg.rooms, "bot runtime started");

        loop {
            if self.shutdown.load(Ordering::SeqCst) {
                info!("bot runtime shutdown requested");
                return Ok(());
            }

            tokio::select! {
                _ = poll_tick.tick() => {
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
        let after = {
            let rooms = self.rooms.lock().await;
            rooms
                .get(room_id)
                .map(|rs| rs.cursor.clone())
                .unwrap_or_default()
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
                        self.process_event(room_id, event, users).await?;
                    }
                }
                Ok(())
            }
            Err(err)
                if chatto::is_not_member_error(&err)
                    || chatto::is_permission_denied_error(&err) =>
            {
                warn!(room = room_id, error = %err, "bot not a member or lacks permission");
                Ok(())
            }
            Err(err) => Err(Error::Chatto(err)),
        }
    }

    async fn process_event(
        &self,
        room_id: &str,
        event: RoomTimelineEvent,
        users: Option<&HashMap<String, UserProfile>>,
    ) -> Result<(), Error> {
        let Some(message_posted) = event.message_posted else {
            return Ok(());
        };

        {
            let mut seen = self.seen_events.lock().await;
            if !seen.insert(event.id.clone()) {
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
                "Unknown command. Type `/chatto-tidal help` for a list of available commands.",
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

        match parsed.command {
            Command::Help => {
                self.send_message(room_id, &help_message()).await;
            }
            Command::Play | Command::Queue => {
                self.cmd_queue(room_id, &message_posted.message.actor_id, &parsed.args)
                    .await?;
            }
            Command::NowPlaying => {
                self.cmd_now_playing(room_id).await;
            }
            Command::Skip => {
                self.cmd_skip(room_id).await;
            }
            Command::Stop => {
                self.cmd_stop(room_id).await;
            }
            Command::Volume => {
                self.cmd_volume(room_id, &parsed.args).await;
            }
            Command::Mute => {
                self.cmd_mute(room_id).await;
            }
            Command::Test => {
                self.cmd_test(room_id).await;
            }
            Command::Lyrics => {
                self.cmd_lyrics(room_id).await;
            }
        }

        Ok(())
    }

    async fn cmd_queue(&self, room_id: &str, actor_id: &str, query: &str) -> Result<(), Error> {
        if query.trim().is_empty() {
            self.print_queue(room_id).await;
            return Ok(());
        }

        let tidal = self.tidal.lock().await;
        let results = tidal.search(query, 5).await?;
        drop(tidal);

        if results.is_empty() {
            self.send_message(
                room_id,
                &card("Not Found", &format!("No results for: {}", query)),
            )
            .await;
            return Ok(());
        }

        let first = &results[0];
        let track = Track {
            tid: first.id,
            title: first.title.clone(),
            artist: first.artist.clone(),
            duration: first.duration,
            requestor: actor_id.to_owned(),
            cover_url: first.cover_url.clone(),
        };

        let pos = {
            let mut rooms = self.rooms.lock().await;
            let rs = rooms.entry(room_id.to_owned()).or_default();
            rs.queue.add(track.clone());
            rs.queue.total_len()
        };

        self.send_message(
            room_id,
            &card(
                "Added",
                &format!(
                    "{} · {} ({})\nPosition: #{}",
                    track.title,
                    track.artist,
                    format_duration(track.duration),
                    pos
                ),
            ),
        )
        .await;

        Ok(())
    }

    async fn print_queue(&self, room_id: &str) {
        let message = {
            let rooms = self.rooms.lock().await;
            let Some(rs) = rooms.get(room_id) else {
                return;
            };

            let list = rs.queue.list();
            if rs.current.is_none() && list.is_empty() {
                card("Queue", "Queue is empty.")
            } else {
                let mut body = String::new();
                if let Some(current) = &rs.current {
                    body.push_str(&format!(
                        "{} · {} ({})\n\n",
                        current.track.title,
                        current.track.artist,
                        format_duration(current.track.duration),
                    ));
                }
                for (idx, track) in list.iter().enumerate() {
                    body.push_str(&format!(
                        "{}. {} · {} ({})\n",
                        idx + 1,
                        track.title,
                        track.artist,
                        format_duration(track.duration)
                    ));
                }
                card("Queue", &body)
            }
        };

        self.send_message(room_id, &message).await;
    }

    async fn cmd_now_playing(&self, room_id: &str) {
        let message = {
            let rooms = self.rooms.lock().await;
            let Some(rs) = rooms.get(room_id) else {
                return;
            };

            match &rs.current {
                Some(current) => card(
                    "Now Playing",
                    &format!(
                        "{} · {} ({})\n{}",
                        current.track.title,
                        current.track.artist,
                        format_duration(current.track.duration),
                        current.audio_info
                    ),
                ),
                None => card("Now Playing", "Nothing currently prepared."),
            }
        };

        self.send_message(room_id, &message).await;
    }

    async fn cmd_skip(&self, room_id: &str) {
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
            self.send_message(room_id, &card("Skipped", "Not connected to voice."))
                .await;
            self.ensure_voice(room_id).await;
            return;
        }

        match title {
            Some(t) => {
                self.send_message(room_id, &card("Skipped", &t)).await;
            }
            None => {
                self.send_message(room_id, &card("Skipped", "Nothing playing."))
                    .await;
            }
        }
    }

    async fn cmd_stop(&self, room_id: &str) {
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

        let msg = card("Stopped", &format!("Removed {count} queued track(s)."));
        drop(rooms);
        self.send_message(room_id, &msg).await;
    }

    async fn cmd_volume(&self, room_id: &str, args: &str) {
        if args.trim().is_empty() {
            let current = *self.volume.lock().await;
            self.send_message(
                room_id,
                &card("Volume", &format!("Current: {:.0}%", current * 100.0)),
            )
            .await;
            return;
        }

        let Ok(pct) = args.trim().parse::<u16>() else {
            self.send_message(room_id, &card("Volume", "Usage: volume <0-200>"))
                .await;
            return;
        };

        if pct > 200 {
            self.send_message(room_id, &card("Volume", "Usage: volume <0-200>"))
                .await;
            return;
        }

        let mut volume = self.volume.lock().await;
        *volume = f64::from(pct) / 100.0;
        self.volume_pct.store(pct as u32, Ordering::SeqCst);
        self.send_message(room_id, &card("Volume", &format!("Set to {}%", pct)))
            .await;
    }

    async fn cmd_mute(&self, room_id: &str) {
        let state = {
            let mut rooms = self.rooms.lock().await;
            let Some(rs) = rooms.get_mut(room_id) else {
                self.send_message(room_id, &card("Mute", "No active room."))
                    .await;
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
        self.send_message(room_id, &msg).await;
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
        let (stream_url, muted) = {
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
            (current.stream_url.clone(), Arc::clone(&rs.muted))
        };

        let room_id_owned = room_id.to_owned();
        let livekit_url = self.livekit_url.clone();
        let sample_rate = self.cfg.sample_rate;
        let chatto = self.chatto.clone();
        let voice_cancel = Arc::new(AtomicBool::new(false));
        let song_cancel = Arc::new(AtomicBool::new(false));
        let next_url: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(Some(stream_url)));

        let video_source: Arc<Mutex<Option<NativeVideoSource>>> = Arc::new(Mutex::new(None));
        let vs_task = video_source.clone();

        let vc = Arc::clone(&voice_cancel);
        let sc = Arc::clone(&song_cancel);
        let nu = Arc::clone(&next_url);
        let mu = Arc::clone(&muted);
        let vp = Arc::clone(&self.volume_pct);
        let db = Arc::clone(&self.rooms);
        let rid = room_id_owned.clone();
        let ch = chatto.clone();

        let handle = tokio::spawn(async move {
            let joined = match ch.join_call(&rid).await {
                Ok(j) => j,
                Err(e) => {
                    warn!(room = rid, error = %e, "join call failed");
                    let mut rooms = db.lock().await;
                    if let Some(rs) = rooms.get_mut(&rid) {
                        rs.current = None;
                        rs.voice = None;
                    }
                    return;
                }
            };
            if !joined {
                warn!(room = rid, "voice channel required");
                let mut rooms = db.lock().await;
                if let Some(rs) = rooms.get_mut(&rid) {
                    rs.current = None;
                    rs.voice = None;
                }
                return;
            }

            let token = match ch.create_call_token(&rid).await {
                Ok(t) => t,
                Err(e) => {
                    warn!(room = rid, error = %e, "get call token failed");
                    let mut rooms = db.lock().await;
                    if let Some(rs) = rooms.get_mut(&rid) {
                        rs.current = None;
                        rs.voice = None;
                    }
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
                    let mut rooms = db.lock().await;
                    if let Some(rs) = rooms.get_mut(&rid) {
                        rs.current = None;
                        rs.voice = None;
                    }
                    return;
                }
            };

            let source = match player.publish_track("mic").await {
                Ok(s) => s,
                Err(e) => {
                    warn!(room = rid, error = %e, "publish track failed");
                    player.disconnect().await;
                    let mut rooms = db.lock().await;
                    if let Some(rs) = rooms.get_mut(&rid) {
                        rs.current = None;
                        rs.voice = None;
                    }
                    return;
                }
            };

            // Publish video track if lyrics is currently enabled
            {
                let rooms = db.lock().await;
                let lyrics_enabled = rooms.get(&rid).is_some_and(|rs| rs.lyrics_enabled);
                if lyrics_enabled
                    && let Ok((src, _)) = livekit_video::publish_video_track(
                        player.room(),
                        "screenshare",
                        livekit_video::DEFAULT_WIDTH,
                        livekit_video::DEFAULT_HEIGHT,
                    )
                    .await
                {
                    *vs_task.lock().await = Some(src);
                }
            }

            info!(room = rid, "voice connected");

            let sample_rate_p = player.sample_rate;

            loop {
                if vc.load(Ordering::SeqCst) {
                    break;
                }

                // If lyrics was enabled after connect, publish video track now
                if vs_task.lock().await.is_none() {
                    let rooms = db.lock().await;
                    if rooms.get(&rid).is_some_and(|rs| rs.lyrics_enabled) {
                        drop(rooms);
                        if let Ok((src, _)) = livekit_video::publish_video_track(
                            player.room(),
                            "screenshare",
                            livekit_video::DEFAULT_WIDTH,
                            livekit_video::DEFAULT_HEIGHT,
                        )
                        .await
                        {
                            *vs_task.lock().await = Some(src);
                        }
                    }
                }

                let url = nu.lock().await.take();

                match url {
                    Some(u) => {
                        sc.store(false, Ordering::SeqCst);

                        let result = LivekitPlayer::play_url_on_source(
                            &source,
                            &u,
                            sample_rate_p,
                            &vp,
                            &sc,
                            &mu,
                        )
                        .await;

                        let mut rooms = db.lock().await;
                        if let Some(rs) = rooms.get_mut(&rid) {
                            rs.current = None;
                            if let Some(ref cancel) = rs.karaoke_cancel {
                                cancel.store(true, Ordering::SeqCst);
                            }
                            rs.karaoke_cancel = None;
                        }
                        drop(rooms);

                        match result {
                            Ok(()) => {}
                            Err(livekit_audio::Error::Cancelled) => {}
                            Err(e) => {
                                error!(room = rid, error = %e, "playback error");
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
                    video_source,
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

    async fn cmd_test(&self, room_id: &str) {
        self.send_message(room_id, &card("Test", "Publishing 10s of silence..."))
            .await;

        let token = match self.chatto.create_call_token(room_id).await {
            Ok(token) => token,
            Err(err) => {
                self.send_message(
                    room_id,
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
                self.send_message(
                    room_id,
                    &card("Test Error", &format!("Create player: {err}")),
                )
                .await;
                return;
            }
        };

        match player.play_silence_only(Duration::from_secs(10)).await {
            Ok(()) => {
                self.send_message(room_id, &card("Test", "10s silence published OK."))
                    .await;
            }
            Err(err) => {
                self.send_message(room_id, &card("Test Failed", &err.to_string()))
                    .await;
            }
        }

        player.disconnect().await;
    }

    async fn cmd_lyrics(&self, room_id: &str) {
        let enable = {
            let mut rooms = self.rooms.lock().await;
            let Some(rs) = rooms.get_mut(room_id) else {
                return;
            };
            rs.lyrics_enabled = !rs.lyrics_enabled;
            if !rs.lyrics_enabled
                && let Some(cancel) = rs.karaoke_cancel.take()
            {
                cancel.store(true, Ordering::SeqCst);
            }
            rs.lyrics_enabled
        };

        if enable {
            self.send_message(room_id, &card("Lyrics", "Karaoke screenshare enabled."))
                .await;
            self.start_karaoke(room_id).await;
        } else {
            self.send_message(room_id, &card("Lyrics", "Karaoke screenshare disabled."))
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
            let source = loop {
                if cancel.load(Ordering::SeqCst) {
                    return;
                }
                let r = rooms.lock().await;
                if let Some(rs) = r.get(&rid)
                    && let Some(ref v) = rs.voice
                {
                    let guard = v.video_source.lock().await;
                    if let Some(ref src) = *guard {
                        break src.clone();
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
                    let elapsed = start.elapsed().as_millis() as u64;
                    let frame = renderer.render_frame(elapsed, w, h);
                    frame.into_raw()
                },
            )
            .await;

            tracing::info!(room = rid, "karaoke screenshare ended");
        });
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

fn card(title: &str, body: &str) -> String {
    const WIDTH: usize = 52;

    let mut out = String::with_capacity(WIDTH * (body.lines().count() + 3));

    let title_len = title.chars().count();
    let dashes = WIDTH.saturating_sub(title_len + 5);
    out.push_str("┌─ ");
    out.push_str(title);
    out.push(' ');
    for _ in 0..dashes {
        out.push('─');
    }
    out.push_str("┐\n");

    for line in body.lines() {
        out.push_str("│  ");
        out.push_str(line);
        out.push('\n');
    }

    out.push('└');
    for _ in 0..(WIDTH - 2) {
        out.push('─');
    }
    out.push('┘');
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
        "Use /chatto-tidal <command> or mention me\nplay <track>\nqueue <track>\nqueue\nskip\nstop\nnowplaying\nvolume <0-200>\nmute / unmute\nlyrics\ntest\nhelp",
    )
}
