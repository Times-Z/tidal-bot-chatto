use ab_glyph::{Font, FontArc, PxScale, ScaleFont, point};
use arc_swap::ArcSwap;
use image::{Rgba, RgbaImage};
use std::sync::Arc;
use tidal::LyricLine;

pub async fn fetch_lyrics(
    tidal: &std::sync::Arc<tokio::sync::Mutex<tidal::Client>>,
    track_id: u64,
    title: &str,
    artist: &str,
) -> Option<Arc<Vec<LyricLine>>> {
    // Try Tidal first
    match tidal.lock().await.get_lyrics(track_id).await {
        Ok(lyrics) if !lyrics.lines.is_empty() => {
            tracing::info!(
                track_id,
                lines = lyrics.lines.len(),
                "got lyrics from Tidal"
            );
            return Some(Arc::new(lyrics.lines));
        }
        Ok(_) => tracing::info!(track_id, "Tidal has no synced lyrics"),
        Err(e) => tracing::warn!(track_id, error = %e, "Tidal lyrics fetch failed"),
    }

    // Fallback: LRCLib
    match fetch_lrclib(title, artist).await {
        Some(lines) => {
            tracing::info!(title, artist, lines = lines.len(), "got lyrics from LRCLib");
            Some(Arc::new(lines))
        }
        None => {
            tracing::info!(title, artist, "no lyrics found on LRCLib");
            None
        }
    }
}

async fn fetch_lrclib(title: &str, artist: &str) -> Option<Vec<LyricLine>> {
    let url = format!(
        "https://lrclib.net/api/get?track_name={}&artist_name={}",
        urlencoding(title),
        urlencoding(artist),
    );

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .user_agent("chatto-tidal-bot/1.0")
        .build()
        .ok()?;

    let resp = client.get(&url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }

    let text = resp.text().await.ok()?;
    let synced_lyrics: Option<String> =
        serde_json::from_str(&text)
            .ok()
            .and_then(|v: serde_json::Value| {
                v.get("syncedLyrics")
                    .and_then(|s| s.as_str().map(String::from))
            });
    let raw = synced_lyrics?;
    Some(tidal::LyricLine::parse_lrc(&raw))
}

fn urlencoding(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' | '.' | '~' => c.to_string(),
            _ => format!("%{:02X}", c as u8),
        })
        .collect()
}

pub struct KaraokeRenderer {
    font: FontArc,
    font_bold: FontArc,
    /// Hot-swappable so screensharing can start immediately with a gradient
    /// and pick up the real cover art once it has been fetched and blurred.
    bg: Arc<ArcSwap<RgbaImage>>,
    /// Hot-swappable: lyrics arrive after the first frames are already
    /// rendered (Tidal, then LRCLib fallback can take seconds).
    lyrics: Arc<ArcSwap<Vec<LyricLine>>>,
    title: String,
    artist: String,
    duration_secs: f64,
}

impl KaraokeRenderer {
    pub fn new(
        bg: RgbaImage,
        lyrics: Arc<Vec<LyricLine>>,
        title: String,
        artist: String,
        duration_secs: f64,
    ) -> Result<Self, String> {
        let font_data = include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../assets/NotoSans-Regular.ttf"
        ));
        let font_bold_data = include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../assets/NotoSans-Bold.ttf"
        ));

        let font = FontArc::try_from_slice(font_data as &[u8])
            .map_err(|e| format!("failed to load font: {e}"))?;
        let font_bold = FontArc::try_from_slice(font_bold_data as &[u8])
            .map_err(|e| format!("failed to load bold font: {e}"))?;

        Ok(Self {
            font,
            font_bold,
            bg: Arc::new(ArcSwap::from_pointee(bg)),
            lyrics: Arc::new(ArcSwap::from_pointee(Arc::unwrap_or_clone(lyrics))),
            title,
            artist,
            duration_secs,
        })
    }

    pub fn set_background(&self, bg: RgbaImage) {
        self.bg.store(Arc::new(bg));
    }

    pub fn set_lyrics(&self, lyrics: Vec<LyricLine>) {
        self.lyrics.store(Arc::new(lyrics));
    }

    pub fn render_frame(&self, elapsed_ms: u64, _width: u32, _height: u32) -> RgbaImage {
        let mut frame = self.bg.load_full().as_ref().clone();

        let overlay_color = Rgba([0, 0, 0, 140]);
        for pixel in frame.pixels_mut() {
            let src = *pixel;
            let a = overlay_color[3] as u32;
            let inv = 255 - a;
            *pixel = Rgba([
                ((src[0] as u32 * inv + overlay_color[0] as u32 * a) / 255) as u8,
                ((src[1] as u32 * inv + overlay_color[1] as u32 * a) / 255) as u8,
                ((src[2] as u32 * inv + overlay_color[2] as u32 * a) / 255) as u8,
                255,
            ]);
        }

        let w = frame.width() as f32;
        let h = frame.height() as f32;

        // Progress bar
        if self.duration_secs > 0.0 {
            let progress = (elapsed_ms as f64 / (self.duration_secs * 1000.0)).min(1.0) as f32;
            let bar_y = h - 6.0;
            let bar_h = 4.0;
            let bar_w = w * 0.8;
            let bar_x = (w - bar_w) / 2.0;
            let fill_w = bar_w * progress;

            // Background bar
            draw_rect(
                &mut frame,
                bar_x as u32,
                bar_y as u32,
                bar_w as u32,
                bar_h as u32,
                Rgba([255, 255, 255, 40]),
            );
            // Fill bar
            if fill_w > 0.0 {
                draw_rect(
                    &mut frame,
                    bar_x as u32,
                    bar_y as u32,
                    fill_w as u32,
                    bar_h as u32,
                    Rgba([255, 255, 255, 200]),
                );
            }
        }

        // Title + artist
        draw_text_centered(
            &mut frame,
            &self.font_bold,
            &self.title,
            PxScale::from(28.0),
            Rgba([255, 255, 255, 200]),
            w / 2.0,
            40.0,
        );
        draw_text_centered(
            &mut frame,
            &self.font,
            &self.artist,
            PxScale::from(18.0),
            Rgba([200, 200, 200, 180]),
            w / 2.0,
            72.0,
        );

        // Current time / duration
        let elapsed_str = format_duration_ms(elapsed_ms);
        let dur_str = format_duration_secs(self.duration_secs as u64);
        let time_str = format!("{elapsed_str} / {dur_str}");
        draw_text_centered(
            &mut frame,
            &self.font,
            &time_str,
            PxScale::from(14.0),
            Rgba([180, 180, 180, 150]),
            w / 2.0,
            h - 18.0,
        );

        // Lyrics
        let lyrics = self.lyrics.load_full();
        if !lyrics.is_empty() {
            let current_idx = find_current_line(&lyrics, elapsed_ms);
            let mid = h * 0.5;

            // Previous line
            if current_idx > 0 && current_idx < lyrics.len() {
                let prev = &lyrics[current_idx - 1];
                draw_text_centered(
                    &mut frame,
                    &self.font,
                    &prev.text,
                    PxScale::from(28.0),
                    Rgba([180, 180, 180, 120]),
                    w / 2.0,
                    mid - 56.0,
                );
            }

            // Current line (large)
            if current_idx < lyrics.len() {
                let curr = &lyrics[current_idx];
                draw_text_centered(
                    &mut frame,
                    &self.font_bold,
                    &curr.text,
                    PxScale::from(44.0),
                    Rgba([255, 255, 255, 255]),
                    w / 2.0,
                    mid,
                );
            }

            // Next line
            if current_idx + 1 < lyrics.len() {
                let next = &lyrics[current_idx + 1];
                draw_text_centered(
                    &mut frame,
                    &self.font,
                    &next.text,
                    PxScale::from(28.0),
                    Rgba([180, 180, 180, 120]),
                    w / 2.0,
                    mid + 56.0,
                );
            }
        } else {
            // No lyrics available
            draw_text_centered(
                &mut frame,
                &self.font,
                "♪ No synced lyrics available",
                PxScale::from(32.0),
                Rgba([200, 200, 200, 150]),
                w / 2.0,
                h * 0.5,
            );
        }

        frame
    }
}

fn find_current_line(lyrics: &[LyricLine], elapsed_ms: u64) -> usize {
    let mut idx = 0;
    for (i, line) in lyrics.iter().enumerate() {
        if line.timestamp_ms <= elapsed_ms {
            idx = i;
        } else {
            break;
        }
    }
    idx
}

fn draw_text_centered(
    image: &mut RgbaImage,
    font: &FontArc,
    text: &str,
    scale: PxScale,
    color: Rgba<u8>,
    center_x: f32,
    center_y: f32,
) {
    let scaled = font.as_scaled(scale);
    let mut min_x = f32::MAX;
    let mut max_x = f32::MIN;
    let mut min_y = f32::MAX;
    let mut max_y = f32::MIN;
    let mut cx = 0.0f32;

    for c in text.chars() {
        let gid = font.glyph_id(c);
        let glyph = gid.with_scale_and_position(scale, point(cx, 0.0));
        if let Some(outline) = scaled.outline_glyph(glyph) {
            let bounds = outline.px_bounds();
            min_x = min_x.min(bounds.min.x);
            max_x = max_x.max(bounds.max.x);
            min_y = min_y.min(bounds.min.y);
            max_y = max_y.max(bounds.max.y);
        }
        cx += scaled.h_advance(gid);
    }

    if min_x.is_infinite() {
        return;
    }

    let text_w = (max_x - min_x).max(1.0);
    let text_h = (max_y - min_y).max(1.0);
    let offset_x = center_x - text_w / 2.0 - min_x;
    let offset_y = center_y - text_h / 2.0 - min_y;

    let mut cx = offset_x;
    for c in text.chars() {
        let gid = font.glyph_id(c);
        let glyph = gid.with_scale_and_position(scale, point(cx, offset_y));
        if let Some(outline) = scaled.outline_glyph(glyph) {
            let bounds = outline.px_bounds();
            outline.draw(|gx, gy, coverage| {
                let px = (bounds.min.x as i32 + gx as i32) as u32;
                let py = (bounds.min.y as i32 + gy as i32) as u32;
                if px < image.width() && py < image.height() {
                    let pixel = image.get_pixel_mut(px, py);
                    let alpha = (coverage * color[3] as f32) as u8;
                    if alpha >= 254 {
                        *pixel = Rgba([color[0], color[1], color[2], 255]);
                    } else if alpha > 0 {
                        let inv = 255 - alpha;
                        pixel[0] = ((color[0] as u32 * alpha as u32 + pixel[0] as u32 * inv as u32)
                            / 255) as u8;
                        pixel[1] = ((color[1] as u32 * alpha as u32 + pixel[1] as u32 * inv as u32)
                            / 255) as u8;
                        pixel[2] = ((color[2] as u32 * alpha as u32 + pixel[2] as u32 * inv as u32)
                            / 255) as u8;
                    }
                }
            });
        }
        cx += scaled.h_advance(gid);
    }
}

fn draw_rect(image: &mut RgbaImage, x: u32, y: u32, w: u32, h: u32, color: Rgba<u8>) {
    for py in y..y + h {
        if py >= image.height() {
            break;
        }
        for px in x..x + w {
            if px >= image.width() {
                break;
            }
            let pixel = image.get_pixel_mut(px, py);
            let a = color[3] as u32;
            let inv = 255 - a;
            pixel[0] = ((color[0] as u32 * a + pixel[0] as u32 * inv) / 255) as u8;
            pixel[1] = ((color[1] as u32 * a + pixel[1] as u32 * inv) / 255) as u8;
            pixel[2] = ((color[2] as u32 * a + pixel[2] as u32 * inv) / 255) as u8;
        }
    }
}

fn format_duration_ms(ms: u64) -> String {
    let total_secs = ms / 1000;
    let mins = total_secs / 60;
    let secs = total_secs % 60;
    format!("{mins}:{secs:02}")
}

fn format_duration_secs(secs: u64) -> String {
    let mins = secs / 60;
    let s = secs % 60;
    format!("{mins}:{s:02}")
}

pub fn rgba_to_i420(rgba: &RgbaImage) -> Vec<u8> {
    let w = rgba.width() as usize;
    let h = rgba.height() as usize;
    let y_size = w * h;
    let uv_size = w.div_ceil(2) * h.div_ceil(2);
    let mut y_plane = vec![0u8; y_size];
    let mut u_plane = vec![0u8; uv_size];
    let mut v_plane = vec![0u8; uv_size];

    for (src, y_pixel) in rgba.pixels().zip(y_plane.iter_mut()) {
        let r = src[0] as f32;
        let g = src[1] as f32;
        let b = src[2] as f32;
        *y_pixel = (0.299 * r + 0.587 * g + 0.114 * b) as u8;
    }

    for j in 0..h.div_ceil(2) {
        for i in 0..w.div_ceil(2) {
            let mut r_acc = 0f32;
            let mut g_acc = 0f32;
            let mut b_acc = 0f32;
            let mut count = 0u32;

            for dy in 0..2 {
                for dx in 0..2 {
                    let px = i * 2 + dx;
                    let py = j * 2 + dy;
                    if px < w && py < h {
                        let p = rgba.get_pixel(px as u32, py as u32);
                        r_acc += p[0] as f32;
                        g_acc += p[1] as f32;
                        b_acc += p[2] as f32;
                        count += 1;
                    }
                }
            }

            if count > 0 {
                let r = r_acc / count as f32;
                let g = g_acc / count as f32;
                let b = b_acc / count as f32;
                u_plane[j * w.div_ceil(2) + i] = (-0.169 * r - 0.331 * g + 0.500 * b + 128.0) as u8;
                v_plane[j * w.div_ceil(2) + i] = (0.500 * r - 0.419 * g - 0.081 * b + 128.0) as u8;
            }
        }
    }

    let mut i420 = Vec::with_capacity(y_size + uv_size * 2);
    i420.extend_from_slice(&y_plane);
    i420.extend_from_slice(&u_plane);
    i420.extend_from_slice(&v_plane);
    i420
}
