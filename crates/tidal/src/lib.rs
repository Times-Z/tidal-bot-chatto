#![deny(unsafe_code)]

//! Thin adapter over the [`tidalrs`] crate.
//!
//! `tidalrs` handles OAuth device-flow authentication, automatic token
//! refresh, search and streaming. Lyrics are fetched with a small hand-rolled
//! request here because `tidalrs` does not expose that Tidal endpoint yet.
//! Token persistence to `tidal_token.json` is wired through the client's
//! refresh callback so long-running services keep a valid credential on disk.

use arc_swap::ArcSwapOption;
use base64::Engine;
use reqwest::header::AUTHORIZATION;
use reqwest::{Client as HttpClient, StatusCode};
use serde::Deserialize;
use std::io::{self, Write};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;
use tidalrs::{AudioQuality, Authz, Error as TidalrsError, ResourceType, SearchQuery, TidalClient};
use tokio::time::sleep;
use url::Url;

const TIDAL_API_BASE: &str = "https://api.tidal.com/v1";
const DEFAULT_COUNTRY_CODE: &str = "US";
const DEVICE_POLL_INTERVAL: Duration = Duration::from_secs(5);
const DEFAULT_ENCODED_CLIENT: &str =
    "NE4zbjZRMXg5NUxMNUs3cDtvS09YZkpXMzcxY1g2eGFaMFB5aGdHTkJkTkxsQlpkNEFLS1lvdWdNamlrPQ==";

pub const QUALITY_LOW: &str = "LOW";
pub const QUALITY_HIGH: &str = "HIGH";
pub const QUALITY_LOSSLESS: &str = "LOSSLESS";
pub const QUALITY_HI_RES_LOSSLESS: &str = "HI_RES_LOSSLESS";

#[derive(Debug, Error)]
pub enum Error {
    #[error("tidal api: {0}")]
    Tidalrs(#[from] TidalrsError),
    #[error("http request: {0}")]
    Http(reqwest::Error),
    #[error("api error (status {status}): {body}")]
    Api { status: StatusCode, body: String },
    #[error("parse response: {0}")]
    ParseResponse(serde_json::Error),
    #[error("token read: {0}")]
    TokenRead(io::Error),
    #[error("token parse: {0}")]
    TokenParse(serde_json::Error),
    #[error("invalid default credentials")]
    InvalidDefaultCredentials,
    #[error("missing stream url in playback response")]
    MissingStreamUrl,
    #[error("not authenticated")]
    NotAuthenticated,
    #[error("parse URL: {0}")]
    ParseUrl(url::ParseError),
    #[error("empty URL")]
    EmptyUrl,
    #[error("not a tidal.com URL")]
    NotTidalUrl,
    #[error("unrecognized Tidal URL path: {0}")]
    UnrecognizedUrlPath(String),
    #[error("unsupported Tidal content type: {0}")]
    UnsupportedContentType(String),
}

pub fn cover_url(image_cover: &str) -> String {
    // Tidal cover IDs are UUID-like strings; the image CDN expects slashes.
    format!(
        "https://resources.tidal.com/images/{}/320x320.jpg",
        image_cover.replace('-', "/")
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchResult {
    pub id: u64,
    pub title: String,
    pub artist: String,
    pub duration: i32,
    pub cover_url: String,
}

#[derive(Debug, Clone)]
pub struct LyricLine {
    pub timestamp_ms: u64,
    pub text: String,
}

impl LyricLine {
    pub fn parse_lrc(text: &str) -> Vec<LyricLine> {
        parse_lrc(text)
    }
}

#[derive(Debug, Clone)]
pub struct Lyrics {
    pub lines: Vec<LyricLine>,
    pub plain: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackStream {
    pub stream_url: String,
    pub quality: String,
    pub codec: String,
    pub bit_depth: i32,
    pub sample_rate: i32,
}

impl TrackStream {
    pub fn format_audio_info(&self) -> String {
        let codec = match self.codec.as_str() {
            "flac" => "FLAC".to_owned(),
            "mp4a.40.5" => "HE-AAC".to_owned(),
            "mp4a.40.2" => "AAC-LC".to_owned(),
            "mp4a.40.34" => "AAC-LD".to_owned(),
            "mha1" => "MPEG-H".to_owned(),
            other if !other.is_empty() => self.quality.clone(),
            _ => String::new(),
        };

        if self.bit_depth > 0 && self.sample_rate > 0 {
            if self.sample_rate >= 1000 {
                return format!(
                    "{codec} {}bit {}kHz",
                    self.bit_depth,
                    self.sample_rate / 1000
                );
            }
            return format!("{codec} {}bit {}Hz", self.bit_depth, self.sample_rate);
        }

        if !codec.is_empty() {
            return codec;
        }

        self.quality.clone()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TidalContentType {
    Track,
    Album,
    Playlist,
    Artist,
}

impl std::fmt::Display for TidalContentType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TidalContentType::Track => f.write_str("track"),
            TidalContentType::Album => f.write_str("album"),
            TidalContentType::Playlist => f.write_str("playlist"),
            TidalContentType::Artist => f.write_str("artist"),
        }
    }
}

#[derive(Clone)]
pub struct Client {
    inner: Arc<TidalClient>,
    /// Mirror of the credential held by `tidalrs` (which does not expose a
    /// getter), kept in sync through the refresh callback and used for the
    /// hand-rolled lyrics request.
    authz: Arc<ArcSwapOption<Authz>>,
    http: HttpClient,
    quality: AudioQuality,
    quality_label: String,
}

impl Client {
    pub async fn new(token_path: impl AsRef<Path>, quality: &str) -> Result<Self, Error> {
        let token_path = token_path.as_ref().to_path_buf();
        let (client_id, client_secret) = default_credentials()?;
        let quality_label = parse_quality(quality).to_owned();
        let audio_quality = match quality_label.as_str() {
            QUALITY_LOW => AudioQuality::Low,
            QUALITY_HIGH => AudioQuality::High,
            QUALITY_LOSSLESS => AudioQuality::Lossless,
            QUALITY_HI_RES_LOSSLESS => AudioQuality::HiResLossless,
            // No (or unrecognized) configuration: request the best available.
            _ => AudioQuality::HiResLossless,
        };

        let authz_mirror: Arc<ArcSwapOption<Authz>> = Arc::new(ArcSwapOption::empty());

        // Persist every automatic token refresh so restarts reuse a valid
        // credential, and mirror it for the hand-rolled lyrics endpoint.
        let save_path = token_path.clone();
        let mirror = authz_mirror.clone();
        let mut inner = TidalClient::new(client_id).with_authz_refresh_callback(move |authz| {
            let _ = save_token(&save_path, &authz);
            mirror.store(Some(Arc::new(authz)));
        });

        match load_saved_token(&token_path)? {
            Some(authz) => {
                authz_mirror.store(Some(Arc::new(authz.clone())));
                inner = inner.with_authz(authz);
            }
            None => {
                let device_auth = inner.device_authorization().await?;
                print_device_prompt(&device_auth);
                let authz =
                    poll_device_auth(&inner, &device_auth.device_code, &client_secret).await?;
                save_token(&token_path, &authz)?;
                authz_mirror.store(Some(Arc::new(authz)));
            }
        }

        Ok(Self {
            inner: Arc::new(inner),
            authz: authz_mirror,
            http: HttpClient::builder()
                .timeout(Duration::from_secs(20))
                .build()
                .map_err(Error::Http)?,
            quality: audio_quality,
            quality_label,
        })
    }

    pub fn selected_quality(&self) -> &str {
        if self.quality_label.is_empty() {
            QUALITY_HI_RES_LOSSLESS
        } else {
            &self.quality_label
        }
    }

    pub async fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchResult>, Error> {
        let mut search = SearchQuery::new(query);
        search.limit = Some(limit as u32);
        search.search_types = Some(vec![ResourceType::Track]);

        let results = self.inner.search(search).await?;
        Ok(results
            .tracks
            .items
            .into_iter()
            .map(|track| SearchResult {
                id: track.id,
                title: track.title,
                artist: artist_display(&track.artists),
                duration: track.duration as i32,
                cover_url: track
                    .album
                    .cover
                    .as_deref()
                    .map(cover_url)
                    .unwrap_or_default(),
            })
            .collect())
    }

    /// Resolve a playable stream URL plus audio metadata for ffmpeg.
    ///
    /// `urlpostpaywall` (via `tidalrs`) gives the URL and codec; playback
    /// info adds bit depth / sample rate for the listening-quality line and
    /// is fetched best-effort so playback still works if it fails.
    pub async fn stream_track(&self, id: u64) -> Result<TrackStream, Error> {
        let stream = self.inner.track_stream(id, self.quality).await?;
        let stream_url = stream
            .urls
            .first()
            .cloned()
            .ok_or(Error::MissingStreamUrl)?;

        let info = self.inner.track_playback_info(id, self.quality).await.ok();

        Ok(TrackStream {
            stream_url,
            quality: info
                .as_ref()
                .map(|i| i.audio_quality.clone())
                .unwrap_or_else(|| stream.audio_quality.as_ref().to_owned()),
            codec: stream.codec,
            bit_depth: info
                .as_ref()
                .and_then(|i| i.bit_depth)
                .map(i32::from)
                .unwrap_or(0),
            sample_rate: info
                .as_ref()
                .and_then(|i| i.sample_rate)
                .map(|v| v as i32)
                .unwrap_or(0),
        })
    }

    /// Lyrics endpoint not covered by `tidalrs`; called directly with the
    /// current access token mirrored from the client.
    pub async fn get_lyrics(&self, id: u64) -> Result<Lyrics, Error> {
        let authz = self.authz.load_full().ok_or(Error::NotAuthenticated)?;
        let country = authz
            .country_code
            .clone()
            .unwrap_or_else(|| DEFAULT_COUNTRY_CODE.to_owned());

        let url = Url::parse_with_params(
            &format!("{TIDAL_API_BASE}/tracks/{id}/lyrics"),
            &[("countryCode", country)],
        )
        .map_err(Error::ParseUrl)?;

        let response = self
            .http
            .get(url)
            .header(AUTHORIZATION, format!("Bearer {}", authz.access_token))
            .send()
            .await
            .map_err(Error::Http)?;

        let status = response.status();
        let body = response.text().await.map_err(Error::Http)?;
        if !status.is_success() {
            return Err(Error::Api { status, body });
        }

        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct LyricsResponse {
            #[serde(default)]
            lyrics: Option<String>,
            #[serde(default)]
            plain_lyrics: Option<String>,
        }

        let parsed: LyricsResponse = serde_json::from_str(&body).map_err(Error::ParseResponse)?;

        let plain = parsed
            .plain_lyrics
            .or_else(|| parsed.lyrics.clone())
            .unwrap_or_default();

        let lines = parse_lrc(&parsed.lyrics.unwrap_or_default());

        Ok(Lyrics { lines, plain })
    }
}

fn artist_display(artists: &[tidalrs::ArtistSummary]) -> String {
    artists
        .iter()
        .map(|artist| artist.name.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

async fn poll_device_auth(
    inner: &TidalClient,
    device_code: &str,
    client_secret: &str,
) -> Result<Authz, Error> {
    loop {
        match inner.authorize(device_code, client_secret).await {
            Ok(token) => return token.authz().ok_or(Error::NotAuthenticated),
            // Tidal keeps answering "authorization_pending" until the user
            // completes the browser flow; keep polling until the device code
            // eventually expires (surfaced as a TidalApi error).
            Err(TidalrsError::AuthorizationPending) => sleep(DEVICE_POLL_INTERVAL).await,
            Err(err) => return Err(err.into()),
        }
    }
}

fn print_device_prompt(resp: &tidalrs::DeviceAuthorizationResponse) {
    let mut out = io::stdout();
    let _ = writeln!(out, "\n=== TIDAL DEVICE AUTH ===");
    let _ = writeln!(out, "1. Open: {}", resp.url);
    let _ = writeln!(out, "2. Enter code: {}", resp.user_code);
    let _ = writeln!(out, "==========================\n");
}

/// Load the saved token file. Accepts both the `Authz` JSON shape written by
/// this adapter and the older `OAuthToken` format (extra fields are ignored,
/// missing `user_id` defaults to 0).
fn load_saved_token(path: &Path) -> Result<Option<Authz>, Error> {
    match fs_read_to_string(path)? {
        Some(content) => {
            let value: serde_json::Value =
                serde_json::from_str(&content).map_err(Error::TokenParse)?;
            let field = |key: &str| {
                value
                    .get(key)
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
            };
            let (Some(access_token), Some(refresh_token)) =
                (field("access_token"), field("refresh_token"))
            else {
                return Ok(None);
            };
            Ok(Some(Authz {
                access_token,
                refresh_token,
                user_id: value.get("user_id").and_then(|v| v.as_u64()).unwrap_or(0),
                country_code: field("country_code"),
            }))
        }
        None => Ok(None),
    }
}

fn fs_read_to_string(path: &Path) -> Result<Option<String>, Error> {
    match std::fs::read_to_string(path) {
        Ok(content) => Ok(Some(content)),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(Error::TokenRead(err)),
    }
}

fn save_token(path: &Path, authz: &Authz) -> Result<(), Error> {
    let data = serde_json::to_string_pretty(authz).map_err(Error::TokenParse)?;
    std::fs::write(path, data).map_err(|err| Error::TokenRead(io::Error::other(err)))
}

fn default_credentials() -> Result<(String, String), Error> {
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(DEFAULT_ENCODED_CLIENT)
        .map_err(|_| Error::InvalidDefaultCredentials)?;
    let decoded = String::from_utf8(decoded).map_err(|_| Error::InvalidDefaultCredentials)?;
    let mut parts = decoded.splitn(2, ';');
    let client_id = parts.next().unwrap_or_default().to_owned();
    let client_secret = parts.next().unwrap_or_default().to_owned();
    if client_id.is_empty() || client_secret.is_empty() {
        return Err(Error::InvalidDefaultCredentials);
    }
    Ok((client_id, client_secret))
}

pub fn parse_quality(s: &str) -> &'static str {
    let normalized = s.trim().to_ascii_uppercase();
    match normalized.as_str() {
        QUALITY_LOW => QUALITY_LOW,
        QUALITY_HIGH => QUALITY_HIGH,
        QUALITY_LOSSLESS => QUALITY_LOSSLESS,
        QUALITY_HI_RES_LOSSLESS => QUALITY_HI_RES_LOSSLESS,
        _ => "",
    }
}

pub fn parse_tidal_url(raw_url: &str) -> Result<(TidalContentType, String), Error> {
    if raw_url.is_empty() {
        return Err(Error::EmptyUrl);
    }

    let normalized = if raw_url.contains("://") {
        raw_url.to_owned()
    } else {
        format!("https://{raw_url}")
    };

    let url = Url::parse(&normalized).map_err(Error::ParseUrl)?;
    let host = url.host_str().unwrap_or_default();
    if !host.ends_with("tidal.com") {
        return Err(Error::NotTidalUrl);
    }

    let mut parts = url
        .path()
        .trim_matches('/')
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>();

    if parts.len() >= 2 && parts[0] == "browse" {
        parts.remove(0);
    }

    if parts.len() < 2 {
        return Err(Error::UnrecognizedUrlPath(url.path().to_owned()));
    }

    let content_type = match parts[0] {
        "track" => TidalContentType::Track,
        "album" => TidalContentType::Album,
        "playlist" => TidalContentType::Playlist,
        "artist" => TidalContentType::Artist,
        other => return Err(Error::UnsupportedContentType(other.to_owned())),
    };

    Ok((content_type, parts[1].to_owned()))
}

fn parse_lrc(text: &str) -> Vec<LyricLine> {
    let mut lines = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some((ts, text)) = parse_lrc_line(line) {
            lines.push(LyricLine {
                timestamp_ms: ts,
                text,
            });
        }
    }
    lines.sort_by_key(|l| l.timestamp_ms);
    lines
}

fn parse_lrc_line(line: &str) -> Option<(u64, String)> {
    let line = line.trim_start();
    if !line.starts_with('[') {
        return None;
    }
    let close = line.find(']')?;
    let time_str = &line[1..close];
    let text = line[close + 1..].trim().to_owned();
    if text.is_empty() {
        return None;
    }
    let ts = parse_lrc_timestamp(time_str)?;
    Some((ts, text))
}

fn parse_lrc_timestamp(s: &str) -> Option<u64> {
    // [mm:ss.xx] or [mm:ss.xxx] or [mm:ss:xx]
    let s = s.trim();
    let colon = s.find(':')?;
    let minutes: u64 = s[..colon].parse().ok()?;
    let rest = &s[colon + 1..];
    let dot = rest.find(['.', ':']).unwrap_or(rest.len());
    let seconds: u64 = rest[..dot].parse().ok()?;
    let millis = if dot < rest.len() {
        let frac = &rest[dot + 1..];
        // Left-align with zero padding so ".5" -> "500", ".50" -> "500",
        // ".500" -> "500" (centi/milliiseconds).
        let padded = format!("{:0<3}", frac);
        padded[..3].parse::<u64>().unwrap_or(0)
    } else {
        0
    };
    Some(minutes * 60_000 + seconds * 1_000 + millis)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_audio_info() {
        let cases = vec![
            (
                TrackStream {
                    stream_url: String::new(),
                    quality: "HI_RES_LOSSLESS".to_owned(),
                    codec: "flac".to_owned(),
                    bit_depth: 24,
                    sample_rate: 48000,
                },
                "FLAC 24bit 48kHz",
            ),
            (
                TrackStream {
                    stream_url: String::new(),
                    quality: "HIGH".to_owned(),
                    codec: "mp4a.40.2".to_owned(),
                    bit_depth: 16,
                    sample_rate: 44100,
                },
                "AAC-LC 16bit 44kHz",
            ),
            (
                TrackStream {
                    stream_url: String::new(),
                    quality: "LOSSLESS".to_owned(),
                    codec: "unknown".to_owned(),
                    bit_depth: 0,
                    sample_rate: 0,
                },
                "LOSSLESS",
            ),
            (
                TrackStream {
                    stream_url: String::new(),
                    quality: "LOW".to_owned(),
                    codec: "flac".to_owned(),
                    bit_depth: 16,
                    sample_rate: 800,
                },
                "FLAC 16bit 800Hz",
            ),
        ];

        for (stream, want) in cases {
            assert_eq!(stream.format_audio_info(), want);
        }
    }

    #[test]
    fn parse_quality_tests() {
        assert_eq!(parse_quality("LOW"), "LOW");
        assert_eq!(parse_quality("low"), "LOW");
        assert_eq!(parse_quality("  high  "), "HIGH");
        assert_eq!(parse_quality("INVALID"), "");
    }

    #[test]
    fn parse_tidal_url_tests() {
        let (kind, id) = parse_tidal_url("https://tidal.com/track/123").unwrap();
        assert_eq!(kind, TidalContentType::Track);
        assert_eq!(id, "123");

        let (kind, id) = parse_tidal_url("tidal.com/browse/playlist/abc").unwrap();
        assert_eq!(kind, TidalContentType::Playlist);
        assert_eq!(id, "abc");

        assert!(matches!(parse_tidal_url(""), Err(Error::EmptyUrl)));
        assert!(matches!(
            parse_tidal_url("https://example.com/track/1"),
            Err(Error::NotTidalUrl)
        ));
    }

    #[test]
    fn cover_url_normalizes_dashes() {
        assert_eq!(
            cover_url("b8f29c4b-845c-41a9-bd4a-a7c2f4b8b9e5"),
            "https://resources.tidal.com/images/b8f29c4b/845c/41a9/bd4a/a7c2f4b8b9e5/320x320.jpg"
        );
    }

    #[test]
    fn artist_display_joins_names() {
        let artists = vec![
            tidalrs::ArtistSummary {
                id: 1,
                name: "Daft Punk".to_owned(),
                ..Default::default()
            },
            tidalrs::ArtistSummary {
                id: 2,
                name: "The Weeknd".to_owned(),
                ..Default::default()
            },
        ];
        assert_eq!(artist_display(&artists), "Daft Punk, The Weeknd");
        assert_eq!(artist_display(&[]), "");
    }

    #[test]
    fn parse_lrc_timestamps() {
        let lines = parse_lrc("[00:01.50] first\n[00:00.250] second\nnot a line");
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].timestamp_ms, 250);
        assert_eq!(lines[0].text, "second");
        assert_eq!(lines[1].timestamp_ms, 1500);
    }

    #[test]
    fn load_saved_token_reads_legacy_format() {
        let dir = std::env::temp_dir().join(format!("tidal-legacy-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("tidal_token.json");
        std::fs::write(
            &path,
            r#"{"access_token":"a","token_type":"Bearer","refresh_token":"r","expires_at":1,"scope":"x"}"#,
        )
        .unwrap();

        let authz = load_saved_token(&path).unwrap().unwrap();
        assert_eq!(authz.access_token, "a");
        assert_eq!(authz.refresh_token, "r");
        assert_eq!(authz.user_id, 0);
        assert_eq!(authz.country_code, None);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn save_token_roundtrips_authz() {
        let dir = std::env::temp_dir().join(format!("tidal-save-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("tidal_token.json");

        let authz = Authz {
            access_token: "a".to_owned(),
            refresh_token: "r".to_owned(),
            user_id: 42,
            country_code: Some("FR".to_owned()),
        };
        save_token(&path, &authz).unwrap();

        let loaded = load_saved_token(&path).unwrap().unwrap();
        assert_eq!(loaded.user_id, 42);
        assert_eq!(loaded.country_code.as_deref(), Some("FR"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn parse_lrc_timestamp_formats() {
        // No fractional part.
        let lines = parse_lrc("[00:01] hi");
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].timestamp_ms, 1000);

        // Colon used as fractional separator (centiseconds).
        let lines = parse_lrc("[00:01:50] hi");
        assert_eq!(lines[0].timestamp_ms, 1500);

        // Half second written with a single digit.
        let lines = parse_lrc("[00:01.5] hi");
        assert_eq!(lines[0].timestamp_ms, 1500);

        // Three-digit milliseconds pass through.
        let lines = parse_lrc("[00:01.123] hi");
        assert_eq!(lines[0].timestamp_ms, 1123);

        // Large minute values.
        let lines = parse_lrc("[60:00] epic");
        assert_eq!(lines[0].timestamp_ms, 3_600_000);
    }

    #[test]
    fn parse_lrc_skips_unusable_lines() {
        let input = "
            \n                        # comment-ish
            [00:00]
            [no timestamp colon] text
            [aa:bb] bad numbers
            [01:bb] bad seconds
            [00:05]  kept  ";
        let lines = parse_lrc(input);
        assert_eq!(lines.len(), 1, "got {lines:?}");
        assert_eq!(lines[0].text, "kept");
    }

    #[test]
    fn parse_lrc_sorts_by_timestamp() {
        let lines = parse_lrc("[00:03] c\n[00:01] a\n[00:02] b");
        let texts: Vec<&str> = lines.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(texts, vec!["a", "b", "c"]);
    }

    #[test]
    fn parse_lrc_multi_timestamp_line_keeps_first_tag() {
        // Documented behavior: only the first [mm:ss] tag of a line is used
        // as the timestamp; any later tags stay in the text.
        let lines = parse_lrc("[00:01][00:02] word");
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].timestamp_ms, 1000);
        assert_eq!(lines[0].text, "[00:02] word");
    }

    #[test]
    fn lyric_line_parse_lrc_delegates_to_parser() {
        let via_type = LyricLine::parse_lrc("[00:02] hey");
        assert_eq!(via_type.len(), 1);
        assert_eq!(via_type[0].timestamp_ms, 2000);
        assert_eq!(via_type[0].text, "hey");
    }

    #[test]
    fn parse_tidal_url_extra_cases() {
        // Query strings are ignored.
        let (kind, id) = parse_tidal_url("https://www.tidal.com/track/9?si=abc").unwrap();
        assert_eq!((kind, id), (TidalContentType::Track, "9".to_owned()));

        // Host matching is case-insensitive (Url::parse lowercases it).
        let (kind, id) = parse_tidal_url("https://TIDAL.COM/artist/2").unwrap();
        assert_eq!((kind, id), (TidalContentType::Artist, "2".to_owned()));

        // Trailing junk after the id is ignored.
        let (kind, id) = parse_tidal_url("https://tidal.com/browse/album/7/extra").unwrap();
        assert_eq!((kind, id), (TidalContentType::Album, "7".to_owned()));

        // Known error branches.
        assert!(matches!(
            parse_tidal_url("https://tidal.com/mix/1"),
            Err(Error::UnsupportedContentType(mix)) if mix == "mix"
        ));
        assert!(matches!(
            parse_tidal_url("https://tidal.com/browse"),
            Err(Error::UnrecognizedUrlPath(_))
        ));
        assert!(matches!(
            parse_tidal_url("https://tidal.com"),
            Err(Error::UnrecognizedUrlPath(_))
        ));
        assert!(matches!(
            parse_tidal_url("https://example.com"),
            Err(Error::NotTidalUrl)
        ));
        assert!(parse_tidal_url("not a url at all:(((").is_err());
    }

    #[test]
    fn content_type_display_names() {
        assert_eq!(TidalContentType::Track.to_string(), "track");
        assert_eq!(TidalContentType::Album.to_string(), "album");
        assert_eq!(TidalContentType::Playlist.to_string(), "playlist");
        assert_eq!(TidalContentType::Artist.to_string(), "artist");
    }

    #[test]
    fn parse_quality_edges() {
        assert_eq!(parse_quality(""), "");
        assert_eq!(parse_quality("   "), "");
        assert_eq!(parse_quality("Hi_Res_Lossless"), QUALITY_HI_RES_LOSSLESS);
        assert_eq!(parse_quality("lossless"), QUALITY_LOSSLESS);
    }

    #[test]
    fn cover_url_without_dashes_is_passthrough() {
        assert_eq!(
            cover_url("abc123"),
            "https://resources.tidal.com/images/abc123/320x320.jpg"
        );
    }

    #[test]
    fn default_credentials_are_decoded() {
        let (client_id, client_secret) = default_credentials().unwrap();
        assert!(!client_id.is_empty());
        assert!(!client_secret.is_empty());
        assert!(!client_id.contains(';'));
    }

    #[test]
    fn load_saved_token_missing_file_is_none() {
        let path = std::env::temp_dir().join("definitely-missing-token-file.json");
        let _ = std::fs::remove_file(&path);
        assert!(load_saved_token(&path).unwrap().is_none());
    }

    #[test]
    fn load_saved_token_rejects_bad_json_and_empty_fields() {
        let dir = std::env::temp_dir().join(format!("tidal-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        // Malformed JSON surfaces as a dedicated error.
        let path = dir.join("bad.json");
        std::fs::write(&path, "{ nope").unwrap();
        assert!(matches!(load_saved_token(&path), Err(Error::TokenParse(_))));

        // An empty access_token is treated like a missing one.
        std::fs::write(&path, r#"{"access_token":"","refresh_token":"r"}"#).unwrap();
        assert!(load_saved_token(&path).unwrap().is_none());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn load_saved_token_reads_full_authz_shape() {
        let dir = std::env::temp_dir().join(format!("tidal-full-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("token.json");
        std::fs::write(
            &path,
            r#"{"access_token":"a","refresh_token":"r","user_id":7,"country_code":"BE"}"#,
        )
        .unwrap();

        let authz = load_saved_token(&path).unwrap().unwrap();
        assert_eq!(authz.user_id, 7);
        assert_eq!(authz.country_code.as_deref(), Some("BE"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn format_audio_info_more_codecs() {
        let base = |codec: &str, quality: &str, bd: i32, sr: i32| TrackStream {
            stream_url: String::new(),
            quality: quality.to_owned(),
            codec: codec.to_owned(),
            bit_depth: bd,
            sample_rate: sr,
        };

        assert_eq!(
            base("mp4a.40.5", "HIGH", 16, 44100).format_audio_info(),
            "HE-AAC 16bit 44kHz"
        );
        assert_eq!(base("mha1", "HIGH", 0, 0).format_audio_info(), "MPEG-H");
        assert_eq!(
            base("mp4a.40.34", "LOW", 0, 0).format_audio_info(),
            "AAC-LD"
        );
        // Unknown codec with metadata falls back to the quality label.
        assert_eq!(
            base("xyz", "LOSSLESS", 16, 44100).format_audio_info(),
            "LOSSLESS 16bit 44kHz"
        );
        // No codec, no metadata: plain quality label.
        assert_eq!(base("", "HIGH", 0, 0).format_audio_info(), "HIGH");
        // No codec but with metadata: the leading space is current behavior.
        assert_eq!(
            base("", "HI_RES_LOSSLESS", 24, 96000).format_audio_info(),
            " 24bit 96kHz"
        );
    }

    #[test]
    fn error_displays_are_actionable() {
        assert_eq!(Error::NotAuthenticated.to_string(), "not authenticated");
        assert_eq!(
            Error::MissingStreamUrl.to_string(),
            "missing stream url in playback response"
        );
        assert!(
            Error::UnsupportedContentType("mix".to_owned())
                .to_string()
                .contains("mix")
        );
        assert!(
            Error::UnrecognizedUrlPath("/browse".to_owned())
                .to_string()
                .contains("/browse")
        );
    }
}
