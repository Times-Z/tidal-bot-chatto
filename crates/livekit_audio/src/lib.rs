#![deny(unsafe_code)]

use libwebrtc::audio_source::native::NativeAudioSource;
use libwebrtc::prelude::{AudioFrame, AudioSourceOptions, RtcAudioSource, RtcError};
use livekit::options::TrackPublishOptions;
use livekit::prelude::{LocalTrack, Room, RoomOptions, TrackSource};
use livekit::track::LocalAudioTrack;
use std::io::ErrorKind;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration, Instant};
use thiserror::Error;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

#[derive(Debug, Clone)]
pub struct Config {
    pub url: String,
    pub token: String,
    pub room: String,
    pub sample_rate: u32,
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("livekit room error: {0}")]
    Room(#[from] livekit::RoomError),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("webrtc error: {0}")]
    Rtc(#[from] RtcError),
    #[error("invalid sample rate {0}, expected 44100 or 48000")]
    InvalidSampleRate(u32),
    #[error("ffmpeg failed: {0}")]
    FfmpegFailed(String),
    #[error("playback cancelled")]
    Cancelled,
}

pub struct Player {
    room: Room,
    pub sample_rate: u32,
    volume: f32,
    track: Option<LocalAudioTrack>,
}

impl Player {
    pub async fn new(cfg: Config) -> Result<Self, Error> {
        if cfg.sample_rate != 44_100 && cfg.sample_rate != 48_000 {
            return Err(Error::InvalidSampleRate(cfg.sample_rate));
        }

        let mut room_options = RoomOptions::default();
        room_options.auto_subscribe = false;
        room_options.single_peer_connection = true;
        let (room, _events) = Room::connect(&cfg.url, &cfg.token, room_options).await?;

        Ok(Self {
            room,
            sample_rate: cfg.sample_rate,
            volume: 1.0,
            track: None,
        })
    }

    pub fn set_volume(&mut self, value: f32) {
        self.volume = value.clamp(0.0, 2.0);
    }

    pub fn volume(&self) -> f32 {
        self.volume
    }

    pub fn room(&self) -> &Room {
        &self.room
    }

    pub async fn publish_track(&mut self, track_name: &str) -> Result<NativeAudioSource, Error> {
        self.unpublish_track().await;

        let source =
            NativeAudioSource::new(AudioSourceOptions::default(), self.sample_rate, 2, 1000);
        let track =
            LocalAudioTrack::create_audio_track(track_name, RtcAudioSource::Native(source.clone()));

        let publish_options = TrackPublishOptions {
            source: TrackSource::Microphone,
            ..Default::default()
        };

        self.room
            .local_participant()
            .publish_track(LocalTrack::Audio(track.clone()), publish_options)
            .await?;

        self.track = Some(track);
        Ok(source)
    }

    pub async fn play_url_on_source(
        source: &NativeAudioSource,
        stream_url: &str,
        sample_rate: u32,
        volume_pct: &AtomicU32,
        cancel: &AtomicBool,
        muted: &AtomicBool,
    ) -> Result<(), Error> {
        let mut ffmpeg = Command::new("ffmpeg");
        ffmpeg
            .kill_on_drop(true)
            .arg("-i")
            .arg(stream_url)
            .arg("-f")
            .arg("s16le")
            .arg("-ac")
            .arg("2")
            .arg("-ar")
            .arg(sample_rate.to_string())
            .arg("-loglevel")
            .arg("warning")
            .arg("pipe:1")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = ffmpeg.spawn().map_err(|err| {
            Error::FfmpegFailed(format!(
                "could not start ffmpeg ({err}); is it installed and on the service PATH?"
            ))
        })?;
        let mut stdout = child
            .stdout
            .take()
            .ok_or_else(|| std::io::Error::other("missing ffmpeg stdout"))?;
        let mut stderr = child
            .stderr
            .take()
            .ok_or_else(|| std::io::Error::other("missing ffmpeg stderr"))?;

        let stderr_task = tokio::spawn(async move {
            let mut bytes = Vec::new();
            let _ = stderr.read_to_end(&mut bytes).await;
            String::from_utf8_lossy(&bytes).trim().to_owned()
        });

        let samples_per_channel = (sample_rate / 100) as usize;
        let mut frame_buf = vec![0_u8; samples_per_channel * 2 * 2];

        loop {
            if cancel.load(Ordering::SeqCst) {
                let _ = child.start_kill();
                let _ = child.wait().await;
                return Err(Error::Cancelled);
            }

            match stdout.read_exact(&mut frame_buf).await {
                Ok(_) => {
                    if !muted.load(Ordering::SeqCst) {
                        let mut pcm = bytes_to_pcm16(&frame_buf);
                        let vol = volume_pct.load(Ordering::Relaxed) as f32 / 100.0;
                        if (vol - 1.0).abs() > f32::EPSILON {
                            for sample in &mut pcm {
                                *sample = apply_volume(*sample, vol);
                            }
                        }

                        let frame = AudioFrame {
                            data: pcm.as_slice().into(),
                            sample_rate,
                            num_channels: 2,
                            samples_per_channel: samples_per_channel as u32,
                        };
                        source.capture_frame(&frame).await?;
                    }
                }
                Err(err) if err.kind() == ErrorKind::UnexpectedEof => {
                    break;
                }
                Err(err) => return Err(Error::Io(err)),
            }
        }

        let status = child.wait().await?;
        let stderr_output = stderr_task.await.unwrap_or_default();

        if !status.success() {
            return Err(Error::FfmpegFailed(stderr_output));
        }

        Ok(())
    }

    pub async fn send_silence_frame(
        source: &NativeAudioSource,
        sample_rate: u32,
    ) -> Result<(), Error> {
        let samples_per_channel = (sample_rate / 50) as usize;
        let samples = vec![0_i16; samples_per_channel * 2];
        let frame = AudioFrame {
            data: samples.as_slice().into(),
            sample_rate,
            num_channels: 2,
            samples_per_channel: samples_per_channel as u32,
        };
        source.capture_frame(&frame).await?;
        Ok(())
    }

    pub async fn play_silence_only(&mut self, duration: Duration) -> Result<(), Error> {
        let source = self.publish_track("mic").await?;
        let frame_duration = Duration::from_millis(20);
        let samples_per_channel = (self.sample_rate / 50) as usize;
        let samples = vec![0_i16; samples_per_channel * 2];
        let deadline = Instant::now() + duration;

        while Instant::now() < deadline {
            let frame = AudioFrame {
                data: samples.as_slice().into(),
                sample_rate: self.sample_rate,
                num_channels: 2,
                samples_per_channel: samples_per_channel as u32,
            };
            source.capture_frame(&frame).await?;
            tokio::time::sleep(frame_duration).await;
        }

        self.unpublish_track().await;
        Ok(())
    }

    pub async fn disconnect(&mut self) {
        self.unpublish_track().await;
        let _ = self.room.close().await;
    }

    async fn unpublish_track(&mut self) {
        if let Some(track) = self.track.take() {
            let _ = self
                .room
                .local_participant()
                .unpublish_track(&track.sid())
                .await;
        }
    }
}

fn bytes_to_pcm16(data: &[u8]) -> Vec<i16> {
    data.as_chunks::<2>()
        .0
        .iter()
        .map(|chunk| i16::from_le_bytes(*chunk))
        .collect()
}

fn apply_volume(sample: i16, volume: f32) -> i16 {
    let amplified = (sample as f32) * volume;
    let clamped = amplified.clamp(i16::MIN as f32, i16::MAX as f32);
    clamped as i16
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn volume_is_clamped() {
        assert_eq!(apply_volume(10_000, 0.0), 0);
        assert_eq!(apply_volume(10_000, 1.0), 10_000);
        assert_eq!(apply_volume(20_000, 2.0), 32_767);
        assert_eq!(apply_volume(-20_000, 2.0), -32_768);
    }

    #[test]
    fn bytes_to_pcm16_parses_le() {
        let bytes = [0x10, 0x27, 0x00, 0x80];
        let pcm = bytes_to_pcm16(&bytes);
        assert_eq!(pcm, vec![10_000, -32_768]);
    }

    #[test]
    fn bytes_to_pcm16_empty_and_odd_len() {
        assert!(bytes_to_pcm16(&[]).is_empty());
        // A trailing odd byte is not a complete sample and gets dropped.
        assert_eq!(bytes_to_pcm16(&[0x01]), Vec::<i16>::new());
        assert_eq!(bytes_to_pcm16(&[0x01, 0x00, 0x99]), vec![1]);
    }

    #[test]
    fn apply_volume_scales_and_clamps() {
        assert_eq!(apply_volume(1000, 0.5), 500);
        assert_eq!(apply_volume(-20_000, 0.5), -10_000);
        assert_eq!(apply_volume(i16::MAX, 1.0), i16::MAX);
        assert_eq!(apply_volume(i16::MIN, 1.0), i16::MIN);
        // Boosting past the range clamps instead of wrapping.
        assert_eq!(apply_volume(i16::MAX, 2.0), i16::MAX);
        assert_eq!(apply_volume(i16::MIN, 2.0), i16::MIN);
        assert_eq!(apply_volume(123, 0.0), 0);
    }

    #[test]
    fn error_display_strings() {
        assert_eq!(
            Error::InvalidSampleRate(96_000).to_string(),
            "invalid sample rate 96000, expected 44100 or 48000"
        );
        assert_eq!(Error::Cancelled.to_string(), "playback cancelled");
        assert_eq!(
            Error::FfmpegFailed("boom".to_owned()).to_string(),
            "ffmpeg failed: boom"
        );
    }

    #[test]
    fn config_is_clonable() {
        let cfg = Config {
            url: "wss://lk.example.com".to_owned(),
            token: "jwt".to_owned(),
            room: "call_1".to_owned(),
            sample_rate: 48_000,
        };
        let cloned = cfg.clone();
        assert_eq!(cloned.url, cfg.url);
        assert_eq!(cloned.token, cfg.token);
        assert_eq!(cloned.room, cfg.room);
        assert_eq!(cloned.sample_rate, cfg.sample_rate);
    }
}
