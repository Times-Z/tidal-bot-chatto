//! Voice side of the bot: joining a call, playing the queue, and the
//! karaoke screenshare task. All of it lives off `Bot` impl blocks here so
//! the playback loop stays out of the command/poll runtime.

use crate::card::{card, format_duration};
use crate::karaoke;
use crate::karaoke::KaraokeRenderer;
use crate::runtime::{Bot, RoomState};
use arc_swap::ArcSwapOption;
use livekit_audio::Player as LivekitPlayer;
use livekit_video::{LocalVideoTrack, NativeVideoSource, Room as LivekitRoom};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tracing::{error, info, warn};

/// Published screenshare track plus its frame source. Both are kept so the
/// track can be unpublished later.
#[derive(Debug)]
pub(crate) struct VideoTrack {
    source: NativeVideoSource,
    track: LocalVideoTrack,
}

#[derive(Debug)]
pub(crate) struct VoiceConnection {
    pub(crate) handle: JoinHandle<()>,
    pub(crate) cancel: Arc<AtomicBool>,
    pub(crate) song_cancel: Arc<AtomicBool>,
    pub(crate) next_url: Arc<Mutex<Option<String>>>,
    pub(crate) video: Arc<Mutex<Option<VideoTrack>>>,
    /// Raised by `/lyrics`; the voice task unpublishes on it (only it holds
    /// the Room).
    pub(crate) video_unpublish: Arc<AtomicBool>,
    /// When the current track started playing; None between tracks. Karaoke
    /// reads this to stay in sync when enabled mid-song.
    pub(crate) play_start: Arc<ArcSwapOption<Instant>>,
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

impl Bot {
    pub(crate) async fn ensure_voice(&self, room_id: &str) {
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

    /// Drop voice handles that died on their own and tell the room.
    pub(crate) async fn reap_voice_tasks(&self) {
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

    pub(crate) async fn start_karaoke(&self, room_id: &str) {
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
                karaoke::create_gradient_background(1920, 1080),
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
                    let bg = karaoke::load_background(&cover_url).await;
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
