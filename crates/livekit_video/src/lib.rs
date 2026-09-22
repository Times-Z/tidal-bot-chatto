#![deny(unsafe_code)]

use libwebrtc::prelude::*;
use livekit::options::TrackPublishOptions;
use livekit::prelude::{LocalTrack, TrackSource};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use thiserror::Error;

pub use libwebrtc::prelude::{I420Buffer, VideoFrame, VideoResolution, VideoRotation};
pub use libwebrtc::video_source::native::NativeVideoSource;
pub use livekit::prelude::{LocalVideoTrack, Room};

pub const DEFAULT_WIDTH: u32 = 1920;
pub const DEFAULT_HEIGHT: u32 = 1080;
pub const DEFAULT_FPS: u32 = 15;

#[derive(Debug, Error)]
pub enum Error {
    #[error("livekit room error: {0}")]
    Room(#[from] livekit::RoomError),
}

/// Publish a video track on an existing LiveKit Room.
/// Returns the `NativeVideoSource` for sending frames and the `LocalVideoTrack`.
pub async fn publish_video_track(
    room: &Room,
    name: &str,
    width: u32,
    height: u32,
) -> Result<(NativeVideoSource, LocalVideoTrack), Error> {
    let resolution = VideoResolution { width, height };
    let source = NativeVideoSource::new(resolution, true);
    let track = LocalVideoTrack::create_video_track(name, RtcVideoSource::Native(source.clone()));

    let publish_options = TrackPublishOptions {
        source: TrackSource::Screenshare,
        ..Default::default()
    };

    room.local_participant()
        .publish_track(LocalTrack::Video(track.clone()), publish_options)
        .await?;

    Ok((source, track))
}

/// Run a frame loop that calls `frame_gen` for each frame.
/// `frame_gen` receives (timestamp_us, width, height) and must return RGBA pixel data.
pub async fn run_frame_loop<F>(
    source: &NativeVideoSource,
    fps: u32,
    width: u32,
    height: u32,
    cancel: &AtomicBool,
    mut frame_gen: F,
) where
    F: FnMut(u64, u32, u32) -> Vec<u8>,
{
    let frame_interval = Duration::from_micros(1_000_000 / fps as u64);

    loop {
        if cancel.load(Ordering::SeqCst) {
            break;
        }

        let frame_start = Instant::now();

        let timestamp_us = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_micros() as i64;

        let rgba = frame_gen(timestamp_us as u64, width, height);
        let mut buffer = I420Buffer::new(width, height);
        fill_i420_from_rgba(&rgba, &mut buffer, width, height);

        let frame = VideoFrame::new(VideoRotation::VideoRotation0, buffer);
        source.capture_frame(&frame);

        let elapsed = frame_start.elapsed();
        if elapsed < frame_interval {
            tokio::time::sleep(Duration::from_millis(1)).await;
            while Instant::now() - frame_start < frame_interval {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        }
    }
}

fn fill_i420_from_rgba(rgba: &[u8], buffer: &mut I420Buffer, w: u32, h: u32) {
    let (y_data, u_data, v_data) = buffer.data_mut();
    let frame_size = (w * h) as usize;

    for (i, chunk) in rgba.as_chunks::<4>().0.iter().enumerate() {
        if i >= frame_size {
            break;
        }
        let [r, g, b, _a] = *chunk;
        let (r, g, b) = (r as f32, g as f32, b as f32);
        y_data[i] = (0.299 * r + 0.587 * g + 0.114 * b) as u8;
    }

    let uw = w.div_ceil(2);
    let uh = h.div_ceil(2);
    for j in 0..uh {
        for i in 0..uw {
            let mut r_sum = 0f32;
            let mut g_sum = 0f32;
            let mut b_sum = 0f32;
            let mut count = 0u32;

            for dy in 0..2 {
                for dx in 0..2 {
                    let px = i * 2 + dx;
                    let py = j * 2 + dy;
                    if px < w && py < h {
                        let offset = (py * w + px) as usize * 4;
                        if offset + 3 < rgba.len() {
                            r_sum += rgba[offset] as f32;
                            g_sum += rgba[offset + 1] as f32;
                            b_sum += rgba[offset + 2] as f32;
                            count += 1;
                        }
                    }
                }
            }

            if count > 0 {
                let r = r_sum / count as f32;
                let g = g_sum / count as f32;
                let b = b_sum / count as f32;
                let idx = (j * uw + i) as usize;
                if idx < u_data.len() && idx < v_data.len() {
                    u_data[idx] = (-0.169 * r - 0.331 * g + 0.500 * b + 128.0) as u8;
                    v_data[idx] = (0.500 * r - 0.419 * g - 0.081 * b + 128.0) as u8;
                }
            }
        }
    }
}
