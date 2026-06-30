use ab_glyph::{FontArc, PxScale};
use anyhow::Result;
use chrono::Local;
use image::codecs::png::{CompressionType, FilterType, PngEncoder};
use image::{ColorType, ImageBuffer, ImageEncoder, Rgba, RgbaImage};
use imageproc::drawing::{draw_filled_rect_mut, draw_hollow_rect_mut, draw_text_mut, text_size};
use imageproc::geometric_transformations::{Interpolation, rotate_about_center};
use imageproc::rect::Rect;
use std::fs::{self, File};
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::config::{ColorStop, KoipyConfig, SlaveConfigEntry, UserId, WatermarkConfig};
use crate::result::{TestResultRow, TestResultTable};

#[derive(Debug, Clone)]
pub struct RenderedResult {
    pub path: PathBuf,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone)]
pub enum RenderedMedia {
    Image(RenderedResult),
    Video {
        video: RenderedResult,
        source_image: RenderedResult,
    },
    FallbackImage {
        image: RenderedResult,
        reason: String,
    },
}

impl RenderedMedia {
    pub fn path(&self) -> &Path {
        match self {
            Self::Image(rendered) => &rendered.path,
            Self::Video { video, .. } => &video.path,
            Self::FallbackImage { image, .. } => &image.path,
        }
    }

    pub fn is_video(&self) -> bool {
        matches!(self, Self::Video { .. })
    }

    pub fn fallback_reason(&self) -> Option<&str> {
        match self {
            Self::FallbackImage { reason, .. } => Some(reason),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct RenderContext {
    pub uid: Option<i64>,
    pub slave: Option<SlaveConfigEntry>,
}

impl RenderContext {
    pub fn with_uid(uid: Option<i64>) -> Self {
        Self { uid, slave: None }
    }
}

#[derive(Debug, Clone)]
pub struct ResultRenderer {
    config: KoipyConfig,
}

impl ResultRenderer {
    pub fn new(config: KoipyConfig) -> Self {
        Self { config }
    }

    pub fn render_table(
        &self,
        table: &TestResultTable,
        dir: impl AsRef<Path>,
    ) -> Result<RenderedResult> {
        self.render_table_with_context(table, dir, RenderContext::default())
    }

    pub fn render_table_with_trace(
        &self,
        table: &TestResultTable,
        dir: impl AsRef<Path>,
        uid: Option<i64>,
    ) -> Result<RenderedResult> {
        self.render_table_with_context(table, dir, RenderContext::with_uid(uid))
    }

    pub fn render_table_with_context(
        &self,
        table: &TestResultTable,
        dir: impl AsRef<Path>,
        context: RenderContext,
    ) -> Result<RenderedResult> {
        let dir = dir.as_ref();
        fs::create_dir_all(dir)?;
        let row_h = 34_u32;
        let header_h = 70_u32;
        let footer_h = 50_u32;
        let columns = [
            ("#", 60_u32),
            ("Node", 350),
            ("Type", 100),
            ("HTTP", 90),
            ("RTT", 90),
            ("Avg", 120),
            ("Max", 120),
            ("UDP", 120),
            ("Curve", 240),
            ("Scripts", 150),
        ];
        let width = columns.iter().map(|(_, col_w)| *col_w).sum::<u32>();
        let height = header_h + footer_h + row_h * table.rows.len().max(1) as u32;
        let mut img: RgbaImage = ImageBuffer::from_pixel(
            width,
            height,
            color_stop_rgba(&self.config.image.color.background.script),
        );
        let title_bg = color_stop_rgba(&self.config.image.color.background.script_title);
        let line_color = color_stop_rgba(&self.config.image.color.yline);
        let font_color = self.font_color();

        draw_filled_rect_mut(&mut img, Rect::at(0, 0).of_size(width, header_h), title_bg);
        draw_filled_rect_mut(
            &mut img,
            Rect::at(0, (height - footer_h) as i32).of_size(width, footer_h),
            title_bg,
        );

        let mut x = 0_i32;
        for (_, col_w) in columns {
            draw_hollow_rect_mut(
                &mut img,
                Rect::at(x, header_h as i32).of_size(col_w, height - header_h - footer_h),
                line_color,
            );
            x += col_w as i32;
        }
        if let Some(font) = self.load_font() {
            let scale = PxScale::from(20.0);
            self.draw_text(
                &mut img,
                font_color,
                20,
                18,
                scale,
                &font,
                &format!("{} - koipy-rs result", self.config.image.title),
            );
            let mut label_x = 8_i32;
            for (label, col_w) in columns {
                self.draw_text(
                    &mut img,
                    font_color,
                    label_x,
                    (header_h - 26) as i32,
                    PxScale::from(16.0),
                    &font,
                    label,
                );
                label_x += col_w as i32;
            }
            for (idx, row) in table.rows.iter().enumerate() {
                let y = header_h + row_h * idx as u32 + 8;
                self.draw_text(
                    &mut img,
                    font_color,
                    18,
                    y as i32,
                    PxScale::from(14.0),
                    &font,
                    &(idx + 1).to_string(),
                );
                self.draw_text(
                    &mut img,
                    font_color,
                    70,
                    y as i32,
                    PxScale::from(14.0),
                    &font,
                    &truncate(&self.render_text(&row.node_name), 36),
                );
                self.draw_text(
                    &mut img,
                    font_color,
                    420,
                    y as i32,
                    PxScale::from(14.0),
                    &font,
                    &type_label(&row.node_type, self.config.image.logo),
                );
                self.draw_text(
                    &mut img,
                    font_color,
                    522,
                    y as i32,
                    PxScale::from(14.0),
                    &font,
                    &fmt_ms(row.http_latency_ms),
                );
                self.draw_text(
                    &mut img,
                    font_color,
                    612,
                    y as i32,
                    PxScale::from(14.0),
                    &font,
                    &fmt_ms(row.rtt_ms),
                );
                self.draw_text(
                    &mut img,
                    font_color,
                    700,
                    y as i32,
                    PxScale::from(14.0),
                    &font,
                    &self.fmt_speed(row.avg_speed_bytes),
                );
                self.draw_text(
                    &mut img,
                    font_color,
                    820,
                    y as i32,
                    PxScale::from(14.0),
                    &font,
                    &self.fmt_speed(row.max_speed_bytes),
                );
                self.draw_text(
                    &mut img,
                    font_color,
                    940,
                    y as i32,
                    PxScale::from(14.0),
                    &font,
                    &truncate(
                        &self.render_text(row.udp_type.as_deref().unwrap_or("N/A")),
                        13,
                    ),
                );
                let script_text = row
                    .script_results
                    .iter()
                    .map(|(_, text)| text.as_str())
                    .collect::<Vec<_>>()
                    .join("/");
                self.draw_text(
                    &mut img,
                    font_color,
                    1298,
                    y as i32,
                    PxScale::from(14.0),
                    &font,
                    &truncate(&self.render_text(&script_text), 16),
                );
            }
            self.draw_text(
                &mut img,
                font_color,
                20,
                (height - footer_h + 14) as i32,
                PxScale::from(15.0),
                &font,
                &format!(
                    "Rows: {}  Topology: {}  Time: {}{}",
                    table.rows.len(),
                    topology_summary(table),
                    Local::now().format("%Y-%m-%d %H:%M:%S"),
                    unsafe_tip_suffix(self.config.image.show_unsafe_tips, context.slave.as_ref())
                ),
            );
            if self.config.image.logo {
                self.draw_protocol_logos(&mut img, table, &font);
            }
            self.draw_watermark(&mut img, &font, context.uid);
        }

        for (idx, row) in table.rows.iter().enumerate() {
            let y = header_h + row_h * idx as u32;
            self.draw_row_blocks(&mut img, row, y, row_h);
        }

        if self.config.image.invert {
            invert_image(&mut img);
        }
        let file = dir.join(format!("{}.png", Local::now().format("%Y-%m-%dT%H-%M-%S")));
        save_png(&img, &file, self.config.image.compress)?;
        Ok(RenderedResult {
            path: file,
            width,
            height,
        })
    }

    pub fn render_json_snapshot(
        &self,
        result: &serde_json::Value,
        dir: impl AsRef<Path>,
    ) -> Result<RenderedResult> {
        let dir = dir.as_ref();
        fs::create_dir_all(dir)?;
        let file = dir.join(format!("{}.json", Local::now().format("%Y-%m-%dT%H-%M-%S")));
        let wrapped = serde_json::json!({
            "title": self.config.image.title,
            "render": "json-snapshot",
            "result": result,
        });
        fs::write(&file, serde_json::to_vec_pretty(&wrapped)?)?;
        Ok(RenderedResult {
            path: file,
            width: 0,
            height: 0,
        })
    }

    pub fn render_video_or_fallback(
        &self,
        table: &TestResultTable,
        dir: impl AsRef<Path>,
        uid: Option<i64>,
    ) -> Result<RenderedMedia> {
        self.render_video_or_fallback_with_context(table, dir, RenderContext::with_uid(uid))
    }

    pub fn render_video_or_fallback_with_context(
        &self,
        table: &TestResultTable,
        dir: impl AsRef<Path>,
        context: RenderContext,
    ) -> Result<RenderedMedia> {
        let image = self.render_table_with_context(table, &dir, context)?;
        if !table_has_speed(table) {
            return Ok(RenderedMedia::FallbackImage {
                image,
                reason: "video output requires speed test data".to_string(),
            });
        }
        match self.render_video_from_image(&image) {
            Ok(video) => Ok(RenderedMedia::Video {
                video,
                source_image: image,
            }),
            Err(err) => Ok(RenderedMedia::FallbackImage {
                image,
                reason: format!("{err:#}"),
            }),
        }
    }

    fn render_video_from_image(&self, image: &RenderedResult) -> Result<RenderedResult> {
        let video_path = image.path.with_extension("mp4");
        let status = Command::new("ffmpeg")
            .args([
                "-y",
                "-loop",
                "1",
                "-i",
                path_string(&image.path).as_str(),
                "-t",
                "3",
                "-vf",
                "format=yuv420p",
                "-movflags",
                "+faststart",
                path_string(&video_path).as_str(),
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map_err(|err| anyhow::anyhow!("ffmpeg unavailable for output=video: {err}"))?;
        if !status.success() {
            anyhow::bail!("ffmpeg failed for output=video with status {status}");
        }
        Ok(RenderedResult {
            path: video_path,
            width: image.width,
            height: image.height,
        })
    }

    fn draw_row_blocks(&self, img: &mut RgbaImage, row: &TestResultRow, y: u32, row_h: u32) {
        let http_color = self.latency_color_stop(row.http_latency_ms);
        self.draw_color_stop_block(img, Rect::at(500, y as i32).of_size(90, row_h), http_color);
        let rtt_color = self.latency_color_stop(row.rtt_ms);
        self.draw_color_stop_block(img, Rect::at(590, y as i32).of_size(90, row_h), rtt_color);
        let avg_color = self.speed_color_stop(row.avg_speed_bytes);
        self.draw_color_stop_block(img, Rect::at(680, y as i32).of_size(120, row_h), avg_color);
        let max_color = self.speed_color_stop(row.max_speed_bytes);
        self.draw_color_stop_block(img, Rect::at(800, y as i32).of_size(120, row_h), max_color);
        let udp_color = match row
            .udp_type
            .as_deref()
            .unwrap_or_default()
            .to_ascii_lowercase()
        {
            value if value.contains("full") || value.contains("open") => {
                &self.config.image.color.yes
            }
            value if value.is_empty() || value == "n/a" => &self.config.image.color.na,
            value if value.contains("blocked") || value.contains("unsupported") => {
                &self.config.image.color.no
            }
            _ => &self.config.image.color.warn,
        };
        self.draw_color_stop_block(img, Rect::at(920, y as i32).of_size(120, row_h), udp_color);
        self.draw_speed_curve(img, row, 1040, y, 240, row_h);
        let script_color = if row
            .script_results
            .iter()
            .any(|(_, text)| text.contains("unlock") || text.contains("解锁"))
        {
            &self.config.image.color.yes
        } else if row.script_results.is_empty() {
            &self.config.image.color.na
        } else {
            &self.config.image.color.no
        };
        self.draw_color_stop_block(
            img,
            Rect::at(1280, y as i32).of_size(160, row_h),
            script_color,
        );
    }

    fn draw_speed_curve(
        &self,
        img: &mut RgbaImage,
        row: &TestResultRow,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) {
        draw_filled_rect_mut(
            img,
            Rect::at(x as i32, y as i32).of_size(w, h),
            color_stop_rgba(&self.config.image.color.na),
        );
        let max = row.per_second_mb.iter().copied().fold(0.0_f64, f64::max);
        if max <= 0.0 {
            return;
        }
        let bar_count = row.per_second_mb.len().max(1) as u32;
        let bar_w = (w / bar_count).max(2);
        for (idx, speed) in row.per_second_mb.iter().enumerate() {
            let ratio = (speed / max).clamp(0.0, 1.0);
            let bar_h = ((h as f64 - 6.0) * ratio).max(2.0) as u32;
            let bar_x = x + idx as u32 * bar_w;
            let bar_y = y + h.saturating_sub(bar_h) - 3;
            draw_filled_rect_mut(
                img,
                Rect::at(bar_x as i32, bar_y as i32).of_size(bar_w.saturating_sub(1), bar_h),
                color_stop_rgba(self.speed_color_stop(Some(*speed * 1024.0 * 1024.0))),
            );
        }
    }

    fn draw_color_stop_block(&self, img: &mut RgbaImage, rect: Rect, stop: &ColorStop) {
        let color = color_stop_rgba(stop);
        if !self.config.image.end_colors_switch {
            draw_filled_rect_mut(img, rect, color);
            return;
        }
        let width = rect.width().max(1);
        let end = hex_rgba(&stop.end_color, stop.alpha).unwrap_or(color);
        for offset in 0..width {
            let ratio = if width <= 1 {
                0.0
            } else {
                offset as f32 / (width - 1) as f32
            };
            let mixed = mix_rgba(color, end, ratio);
            draw_filled_rect_mut(
                img,
                Rect::at(rect.left() + offset as i32, rect.top()).of_size(1, rect.height()),
                mixed,
            );
        }
    }

    fn load_font(&self) -> Option<FontArc> {
        let path = self.config.image.font.trim();
        if !path.is_empty() {
            if let Some(font) = read_font(path) {
                return Some(font);
            }
        }
        system_font_candidates().into_iter().find_map(read_font)
    }

    fn latency_color_stop(&self, value: Option<f64>) -> &ColorStop {
        let value = value.unwrap_or_default();
        if value <= 0.0 {
            return &self.config.image.color.na;
        }
        if let Some(stop) = pick_stop(&self.config.image.color.delay, value) {
            return stop;
        }
        match value as u64 {
            1..=300 => &self.config.image.color.wait,
            301..=1000 => &self.config.image.color.warn,
            1001.. => &self.config.image.color.no,
            _ => &self.config.image.color.na,
        }
    }

    fn speed_color_stop(&self, value: Option<f64>) -> &ColorStop {
        let value = value.unwrap_or_default();
        if value <= 0.0 {
            return &self.config.image.color.na;
        }
        let display_value = self.speed_display_value(value);
        if let Some(stop) = pick_stop(&self.config.image.color.speed, display_value) {
            return stop;
        }
        match value as u64 {
            1..=1_000_000 => &self.config.image.color.wait,
            1_000_001..=10_000_000 => &self.config.image.color.warn,
            10_000_001.. => &self.config.image.color.yes,
            _ => &self.config.image.color.na,
        }
    }

    fn font_color(&self) -> Rgba<u8> {
        color_stop_rgba(&self.config.image.color.font)
    }

    fn draw_protocol_logos(&self, img: &mut RgbaImage, table: &TestResultTable, font: &FontArc) {
        for (idx, row) in table.rows.iter().enumerate() {
            let Some(style) = protocol_logo_style(&row.node_type) else {
                continue;
            };
            let y = 70 + 34 * idx as u32;
            let rect = Rect::at(418, y as i32 + 5).of_size(70, 24);
            draw_filled_rect_mut(img, rect, style.background);
            draw_hollow_rect_mut(img, rect, style.border);
            self.draw_text(
                img,
                style.foreground,
                426,
                y as i32 + 10,
                PxScale::from(11.0),
                font,
                style.text,
            );
        }
    }

    fn draw_watermark(&self, img: &mut RgbaImage, font: &FontArc, uid: Option<i64>) {
        let watermark = self.watermark_for_uid(uid);
        if !watermark.enable || watermark.text.trim().is_empty() {
            return;
        }
        let text = watermark_text(watermark, uid);
        let scale = PxScale::from(watermark.size.max(1) as f32);
        let (text_w, text_h) = text_size(scale, font, &text);
        let tile_w = text_w.saturating_add(160).max(280);
        let tile_h = text_h
            .saturating_add(watermark.row_spacing)
            .saturating_add(80)
            .max(watermark.size.saturating_add(80));
        let mut tile: RgbaImage = ImageBuffer::from_pixel(tile_w, tile_h, Rgba([0, 0, 0, 0]));
        let color = watermark_color(watermark);
        if watermark.shadow {
            self.draw_text(
                &mut tile,
                Rgba([0, 0, 0, color.0[3].saturating_div(2)]),
                42,
                42,
                scale,
                font,
                &text,
            );
        }
        self.draw_text(&mut tile, color, 40, 40, scale, font, &text);

        let rotated = if watermark.angle.abs() > f32::EPSILON {
            rotate_about_center(
                &tile,
                watermark.angle.to_radians(),
                Interpolation::Nearest,
                Rgba([0, 0, 0, 0]),
            )
        } else {
            tile
        };
        let step_x = rotated.width().max(1);
        let step_y = rotated
            .height()
            .saturating_add(watermark.row_spacing)
            .max(1);
        let start_y = watermark.start_y.min(img.height());
        let mut y = start_y;
        while y < img.height() {
            let mut x = 0;
            while x < img.width() {
                overlay_alpha(img, &rotated, x, y);
                x = x.saturating_add(step_x);
            }
            y = y.saturating_add(step_y);
        }
    }

    fn watermark_for_uid(&self, uid: Option<i64>) -> &WatermarkConfig {
        if let Some(uid) = uid {
            if !user_list_contains(&self.config.user, uid)
                && self.config.image.non_commercial_watermark.enable
            {
                return &self.config.image.non_commercial_watermark;
            }
        }
        &self.config.image.watermark
    }

    fn draw_text(
        &self,
        img: &mut RgbaImage,
        color: Rgba<u8>,
        x: i32,
        y: i32,
        scale: PxScale,
        font: &FontArc,
        text: &str,
    ) {
        let text = self.render_text(text);
        if text.is_empty() {
            return;
        }
        draw_text_mut(img, color, x, y, scale, font, &text);
    }

    fn render_text(&self, text: &str) -> String {
        render_text_for_emoji_config(
            text,
            self.config.image.emoji.enable,
            &self.config.image.emoji.source,
        )
    }

    fn fmt_speed(&self, value: Option<f64>) -> String {
        let value = value.unwrap_or_default();
        if value <= 0.0 {
            return "N/A".to_string();
        }
        let format = SpeedFormat::parse(&self.config.image.speed_format);
        let base = format.base();
        let mut amount = if format.bits { value * 8.0 } else { value };
        let units = if format.bits {
            ["bps", "Kbps", "Mbps", "Gbps", "Tbps"]
        } else {
            ["B/s", "KB/s", "MB/s", "GB/s", "TB/s"]
        };
        let mut idx = 0;
        while amount >= base && idx + 1 < units.len() {
            amount /= base;
            idx += 1;
        }
        format!("{amount:.1}{}", units[idx])
    }

    fn speed_display_value(&self, bytes_per_second: f64) -> f64 {
        let format = SpeedFormat::parse(&self.config.image.speed_format);
        let base = format.base();
        let value = if format.bits {
            bytes_per_second * 8.0
        } else {
            bytes_per_second
        };
        value / base / base
    }
}

fn save_png(img: &RgbaImage, path: &Path, compress: bool) -> Result<()> {
    let file = File::create(path)?;
    let writer = BufWriter::new(file);
    let compression = if compress {
        CompressionType::Best
    } else {
        CompressionType::Fast
    };
    let encoder = PngEncoder::new_with_quality(writer, compression, FilterType::Adaptive);
    encoder.write_image(
        img.as_raw(),
        img.width(),
        img.height(),
        ColorType::Rgba8.into(),
    )?;
    Ok(())
}

fn watermark_text(watermark: &WatermarkConfig, uid: Option<i64>) -> String {
    if watermark.trace {
        format!(
            "{} UID:{}",
            watermark.text,
            uid.map(|value| value.to_string())
                .unwrap_or_else(|| "unknown".to_string())
        )
    } else {
        watermark.text.clone()
    }
}

fn watermark_color(watermark: &WatermarkConfig) -> Rgba<u8> {
    let mut color = color_stop_rgba(&watermark.color);
    color.0[3] = watermark.alpha.min(color.0[3]);
    color
}

#[derive(Debug, Clone, Copy)]
struct ProtocolLogoStyle {
    text: &'static str,
    background: Rgba<u8>,
    border: Rgba<u8>,
    foreground: Rgba<u8>,
}

fn protocol_logo_style(value: &str) -> Option<ProtocolLogoStyle> {
    let normalized = value
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .collect::<String>()
        .to_ascii_lowercase();
    let (text, bg, border) = match normalized.as_str() {
        "vless" => ("VLESS", [64, 123, 255, 255], [25, 71, 179, 255]),
        "hysteria" | "hysteria2" | "hy2" => ("HY", [236, 81, 112, 255], [150, 35, 59, 255]),
        "shadowsocks" | "ss" => ("SS", [71, 170, 115, 255], [31, 104, 68, 255]),
        "shadowsocksr" | "ssr" => ("SSR", [122, 90, 212, 255], [74, 48, 150, 255]),
        "vmess" => ("VMESS", [26, 188, 156, 255], [19, 120, 101, 255]),
        "wireguard" | "wg" => ("WG", [245, 185, 66, 255], [156, 104, 15, 255]),
        "ssh" => ("SSH", [97, 105, 117, 255], [48, 55, 66, 255]),
        "sudoku" => ("SDK", [236, 120, 55, 255], [152, 69, 22, 255]),
        "snell" => ("SNELL", [42, 157, 143, 255], [27, 94, 86, 255]),
        "tuic" => ("TUIC", [10, 132, 255, 255], [6, 77, 157, 255]),
        "trojan" => ("TRJN", [177, 66, 89, 255], [109, 32, 49, 255]),
        _ => return None,
    };
    Some(ProtocolLogoStyle {
        text,
        background: Rgba(bg),
        border: Rgba(border),
        foreground: Rgba([255, 255, 255, 255]),
    })
}

fn type_label(value: &str, logo_enabled: bool) -> String {
    if logo_enabled && protocol_logo_style(value).is_some() {
        String::new()
    } else {
        truncate(value, 10)
    }
}

fn unsafe_tip_suffix(show: bool, slave: Option<&SlaveConfigEntry>) -> &'static str {
    if show && slave.is_some_and(is_unsafe_slave) {
        "  Warning: unsafe backend"
    } else {
        ""
    }
}

fn is_unsafe_slave(slave: &SlaveConfigEntry) -> bool {
    !slave.tls || slave.skip_cert_verify
}

fn user_list_contains(users: &[UserId], uid: i64) -> bool {
    users.iter().any(|value| match value {
        serde_yaml::Value::Number(number) => number.as_i64() == Some(uid),
        serde_yaml::Value::String(text) => text.trim().parse::<i64>().ok() == Some(uid),
        _ => false,
    })
}

fn overlay_alpha(base: &mut RgbaImage, layer: &RgbaImage, x: u32, y: u32) {
    for layer_y in 0..layer.height() {
        let base_y = y + layer_y;
        if base_y >= base.height() {
            break;
        }
        for layer_x in 0..layer.width() {
            let base_x = x + layer_x;
            if base_x >= base.width() {
                break;
            }
            let top = layer.get_pixel(layer_x, layer_y).0;
            if top[3] == 0 {
                continue;
            }
            let bottom = base.get_pixel(base_x, base_y).0;
            let alpha = top[3] as f32 / 255.0;
            let inv_alpha = 1.0 - alpha;
            let out = [
                (top[0] as f32 * alpha + bottom[0] as f32 * inv_alpha).round() as u8,
                (top[1] as f32 * alpha + bottom[1] as f32 * inv_alpha).round() as u8,
                (top[2] as f32 * alpha + bottom[2] as f32 * inv_alpha).round() as u8,
                bottom[3],
            ];
            base.put_pixel(base_x, base_y, Rgba(out));
        }
    }
}

fn truncate(value: &str, max_chars: usize) -> String {
    let mut out = value.chars().take(max_chars).collect::<String>();
    if value.chars().count() > max_chars {
        out.push_str("...");
    }
    out
}

fn fmt_ms(value: Option<f64>) -> String {
    value
        .filter(|v| *v > 0.0)
        .map(|v| format!("{v:.0}ms"))
        .unwrap_or_else(|| "N/A".to_string())
}

fn topology_summary(table: &TestResultTable) -> &'static str {
    match (table.inbound.is_some(), table.outbound.is_some()) {
        (true, true) => "in/out",
        (true, false) => "in",
        (false, true) => "out",
        (false, false) => "none",
    }
}

fn table_has_speed(table: &TestResultTable) -> bool {
    table.rows.iter().any(|row| {
        row.avg_speed_bytes.unwrap_or_default() > 0.0
            || row.max_speed_bytes.unwrap_or_default() > 0.0
            || row.per_second_mb.iter().any(|speed| *speed > 0.0)
    })
}

fn path_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SpeedFormat {
    bits: bool,
    binary: bool,
}

impl SpeedFormat {
    fn parse(value: &str) -> Self {
        match value.to_ascii_lowercase().as_str() {
            "byte/binary" => Self {
                bits: false,
                binary: true,
            },
            "bit/binary" => Self {
                bits: true,
                binary: true,
            },
            "bit/decimal" => Self {
                bits: true,
                binary: false,
            },
            _ => Self {
                bits: false,
                binary: false,
            },
        }
    }

    fn base(self) -> f64 {
        if self.binary { 1024.0 } else { 1000.0 }
    }
}

fn pick_stop(stops: &[ColorStop], value: f64) -> Option<&ColorStop> {
    stops
        .iter()
        .filter(|stop| value >= stop.label)
        .max_by(|a, b| a.label.total_cmp(&b.label))
}

fn color_stop_rgba(stop: &ColorStop) -> Rgba<u8> {
    hex_rgba(&stop.value, stop.alpha).unwrap_or(Rgba([255, 255, 255, stop.alpha]))
}

fn read_font(path: impl AsRef<Path>) -> Option<FontArc> {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| FontArc::try_from_vec(bytes).ok())
}

fn system_font_candidates() -> Vec<&'static str> {
    vec![
        "C:/Windows/Fonts/msyh.ttc",
        "C:/Windows/Fonts/simsun.ttc",
        "C:/Windows/Fonts/arial.ttf",
        "/System/Library/Fonts/PingFang.ttc",
        "/System/Library/Fonts/Supplemental/Arial Unicode.ttf",
        "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/truetype/noto/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
    ]
}

fn render_text_for_emoji_config(text: &str, emoji_enabled: bool, source: &str) -> String {
    let _source = normalized_emoji_source(source);
    if emoji_enabled {
        text.to_string()
    } else {
        strip_emoji(text)
    }
}

fn normalized_emoji_source(source: &str) -> &'static str {
    match source.trim() {
        "ApplePediaSource" => "ApplePediaSource",
        "GooglePediaSource" => "GooglePediaSource",
        "SamsungPediaSource" => "SamsungPediaSource",
        "MicrosoftPediaSource" => "MicrosoftPediaSource",
        "WhatsAppPediaSource" => "WhatsAppPediaSource",
        "TwitterPediaSource" => "TwitterPediaSource",
        "FacebookPediaSource" => "FacebookPediaSource",
        "MicrosoftTeamsPediaSource" => "MicrosoftTeamsPediaSource",
        "SkypePediaSource" => "SkypePediaSource",
        "JoyPixelsPediaSource" => "JoyPixelsPediaSource",
        "TossFacePediaSource" => "TossFacePediaSource",
        "TwemojiLocalSource" => "TwemojiLocalSource",
        "OpenmojiLocalSource" => "OpenmojiLocalSource",
        _ => "TwemojiLocalSource",
    }
}

fn strip_emoji(text: &str) -> String {
    text.chars().filter(|ch| !is_emoji_like(*ch)).collect()
}

fn is_emoji_like(ch: char) -> bool {
    matches!(
        ch as u32,
        0x1F000..=0x1FAFF
            | 0x2600..=0x27BF
            | 0xFE00..=0xFE0F
            | 0x200D
    )
}

fn mix_rgba(start: Rgba<u8>, end: Rgba<u8>, ratio: f32) -> Rgba<u8> {
    let ratio = ratio.clamp(0.0, 1.0);
    let mut out = [0_u8; 4];
    for idx in 0..4 {
        out[idx] = (start.0[idx] as f32 + (end.0[idx] as f32 - start.0[idx] as f32) * ratio)
            .round()
            .clamp(0.0, 255.0) as u8;
    }
    Rgba(out)
}

fn invert_image(img: &mut RgbaImage) {
    for pixel in img.pixels_mut() {
        pixel.0[0] = 255_u8.saturating_sub(pixel.0[0]);
        pixel.0[1] = 255_u8.saturating_sub(pixel.0[1]);
        pixel.0[2] = 255_u8.saturating_sub(pixel.0[2]);
    }
}

fn hex_rgba(value: &str, alpha: u8) -> Option<Rgba<u8>> {
    let raw = value.trim().trim_start_matches('#');
    if raw.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&raw[0..2], 16).ok()?;
    let g = u8::from_str_radix(&raw[2..4], 16).ok()?;
    let b = u8::from_str_radix(&raw[4..6], 16).ok()?;
    Some(Rgba([r, g, b, alpha]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::KoipyConfig;

    #[test]
    fn renders_basic_png() {
        let renderer = ResultRenderer::new(KoipyConfig::default());
        let table = TestResultTable::default();
        let dir = std::env::temp_dir().join("koipy-rs-image-test");
        let rendered = renderer.render_table(&table, &dir).expect("render png");
        assert!(rendered.path.exists());
        assert!(rendered.width > 0);
        assert!(rendered.height > 0);
    }

    #[test]
    fn parses_hex_color() {
        assert_eq!(hex_rgba("#bee47e", 255).unwrap().0, [190, 228, 126, 255]);
    }

    #[test]
    fn formats_speed_by_config() {
        let mut cfg = KoipyConfig::default();
        cfg.image.speed_format = "bit/decimal".to_string();
        let renderer = ResultRenderer::new(cfg);
        assert_eq!(renderer.fmt_speed(Some(1_000_000.0)), "8.0Mbps");
    }

    #[test]
    fn mixes_and_inverts_pixels() {
        assert_eq!(
            mix_rgba(Rgba([0, 0, 0, 255]), Rgba([100, 50, 0, 255]), 0.5).0,
            [50, 25, 0, 255]
        );
        let mut img: RgbaImage = ImageBuffer::from_pixel(1, 1, Rgba([10, 20, 30, 128]));
        invert_image(&mut img);
        assert_eq!(img.get_pixel(0, 0).0, [245, 235, 225, 128]);
    }

    #[test]
    fn watermark_trace_and_alpha_are_configurable() {
        let mut watermark = crate::config::WatermarkConfig {
            enable: true,
            text: "Koipy".to_string(),
            trace: true,
            alpha: 24,
            ..Default::default()
        };
        watermark.color.alpha = 64;
        assert_eq!(watermark_text(&watermark, Some(123)), "Koipy UID:123");
        assert_eq!(watermark_color(&watermark).0[3], 24);
    }

    #[test]
    fn logo_setting_controls_protocol_type_label() {
        assert_eq!(type_label("Shadowsocks", true), "");
        assert_eq!(type_label("Shadowsocks", false), "Shadowsock...");
        assert!(protocol_logo_style("vless").is_some());
        assert!(protocol_logo_style("unknown").is_none());
    }

    #[test]
    fn unsafe_tip_follows_slave_tls_settings() {
        let mut slave = crate::config::SlaveConfigEntry {
            id: "local".to_string(),
            comment: String::new(),
            hidden: false,
            token: String::new(),
            r#type: crate::config::SlaveType::MiaoSpeed,
            address: "127.0.0.1:8765".to_string(),
            path: "/".to_string(),
            proxy: None,
            skip_cert_verify: false,
            tls: true,
            invoker: None,
            buildtoken: None,
            option: Default::default(),
        };
        assert_eq!(unsafe_tip_suffix(true, Some(&slave)), "");
        slave.skip_cert_verify = true;
        assert_eq!(
            unsafe_tip_suffix(true, Some(&slave)),
            "  Warning: unsafe backend"
        );
        assert_eq!(unsafe_tip_suffix(false, Some(&slave)), "");
        slave.skip_cert_verify = false;
        slave.tls = false;
        assert!(is_unsafe_slave(&slave));
    }

    #[test]
    fn non_commercial_watermark_is_used_for_non_users() {
        let mut cfg = KoipyConfig::default();
        cfg.user.push(serde_yaml::Value::Number(7.into()));
        cfg.image.watermark.enable = true;
        cfg.image.watermark.text = "private".to_string();
        cfg.image.non_commercial_watermark.enable = true;
        cfg.image.non_commercial_watermark.text = "public".to_string();
        let renderer = ResultRenderer::new(cfg);
        assert_eq!(renderer.watermark_for_uid(Some(7)).text, "private");
        assert_eq!(renderer.watermark_for_uid(Some(8)).text, "public");
        assert_eq!(renderer.watermark_for_uid(None).text, "private");
    }

    #[test]
    fn emoji_config_controls_text_rendering_fallback() {
        assert_eq!(
            normalized_emoji_source("OpenmojiLocalSource"),
            "OpenmojiLocalSource"
        );
        assert_eq!(
            normalized_emoji_source("MissingSource"),
            "TwemojiLocalSource"
        );
        assert_eq!(
            render_text_for_emoji_config("Ping 🚀 ok", false, "TwemojiLocalSource"),
            "Ping  ok"
        );
        assert_eq!(
            render_text_for_emoji_config("Ping 🚀 ok", true, "MissingSource"),
            "Ping 🚀 ok"
        );
    }

    #[test]
    fn default_renderer_loads_a_configured_or_system_font() {
        let renderer = ResultRenderer::new(KoipyConfig::default());
        assert!(renderer.load_font().is_some());
    }

    #[test]
    fn saves_png_with_configurable_compression() {
        let dir = std::env::temp_dir().join("koipy-rs-png-compress-test");
        std::fs::create_dir_all(&dir).expect("dir");
        let img: RgbaImage = ImageBuffer::from_pixel(16, 16, Rgba([1, 2, 3, 255]));
        let fast = dir.join("fast.png");
        let best = dir.join("best.png");
        save_png(&img, &fast, false).expect("fast png");
        save_png(&img, &best, true).expect("compressed png");
        assert!(fast.exists());
        assert!(best.exists());
        assert!(std::fs::metadata(&fast).expect("fast meta").len() > 0);
        assert!(std::fs::metadata(&best).expect("best meta").len() > 0);
        let _ = std::fs::remove_file(fast);
        let _ = std::fs::remove_file(best);
    }

    #[test]
    fn video_output_falls_back_without_speed_data() {
        let renderer = ResultRenderer::new(KoipyConfig::default());
        let dir = std::env::temp_dir().join("koipy-rs-video-fallback-test");
        let rendered = renderer
            .render_video_or_fallback(&TestResultTable::default(), &dir, None)
            .expect("render fallback");
        assert!(!rendered.is_video());
        assert!(rendered.path().exists());
        assert_eq!(
            rendered.fallback_reason(),
            Some("video output requires speed test data")
        );
    }
}
