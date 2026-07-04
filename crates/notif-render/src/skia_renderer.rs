#![forbid(unsafe_code)]
//! `SkiaRenderer` — a full-featured notification renderer using `tiny-skia` and `cosmic-text`.
//!
//! Buffer format: BGRA bytes (wl_shm ARGB8888 little-endian), premultiplied alpha.
//! tiny-skia produces premultiplied RGBA; the one and only channel swizzle lives
//! in the final copy loop of [`Renderer::render`].
//!
//! Coordinate spaces:
//! * [`Layout::width`] / [`Layout::height`] are **logical** pixels.
//! * [`Layout::hit_regions`] are in **buffer** pixels (already multiplied by `scale`),
//!   matching what `notif-wl` expects for pointer hit-testing.
//!
//! All text is laid out at the final buffer-pixel size (`font_size * scale`);
//! nothing is rasterized small and upscaled.

use std::collections::HashMap;
use std::path::PathBuf;

use cosmic_text::{Attrs, Buffer, Family, FontSystem, Metrics, Shaping, SwashCache, SwashContent};
use tiny_skia::{Color, Paint, Pixmap, PixmapPaint, Stroke, Transform};

use notif_types::config::Rgba;
use notif_types::{DisplayNotification, ImageSource, RawImage, Urgency, config::Config};

use crate::{HitRegion, HitTarget, Layout, Rect, Renderer};

// ── Layout constants (logical pixels, multiplied by scale at use sites) ───────

/// Inner padding around notification content.
const PADDING: f32 = 12.0;
/// Close-button square size.
const CLOSE_SIZE: f32 = 20.0;
/// Close-button inset from the top-right corner.
const CLOSE_INSET: f32 = 8.0;
/// Horizontal gap between action buttons.
const BTN_GAP: f32 = 8.0;
/// Minimum notification height.
const MIN_NOTIF_H: f32 = 48.0;

/// Glyph-metadata bit marking underlined spans (`<u>` / `<a>`).
const META_UNDERLINE: usize = 1;

// ── Markup span model ─────────────────────────────────────────────────────────

/// A run of styled text produced by the body-markup parser.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    /// The (entity-decoded) text of this run.
    pub text: String,
    /// Render bold.
    pub bold: bool,
    /// Render italic.
    pub italic: bool,
    /// Render underlined (`<u>` and `<a href>` spans).
    pub underline: bool,
}

impl Span {
    /// An unstyled span.
    pub fn plain(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            bold: false,
            italic: false,
            underline: false,
        }
    }
}

/// Decode the XML entities `&amp; &lt; &gt; &quot; &apos;` in `s`, appending
/// to `out`.  Unknown entities are copied verbatim.
fn unescape_into(s: &str, out: &mut String) {
    let mut rest = s;
    while let Some(amp) = rest.find('&') {
        let (head, tail) = rest.split_at(amp);
        out.push_str(head);
        // `tail` starts with '&'. Look for ';' within a short window.
        let semi = tail
            .char_indices()
            .take(10)
            .find(|(_, c)| *c == ';')
            .map(|(i, _)| i);
        match semi {
            Some(end) => {
                let entity = tail.get(1..end).unwrap_or("");
                let replacement = match entity {
                    "amp" => Some("&"),
                    "lt" => Some("<"),
                    "gt" => Some(">"),
                    "quot" => Some("\""),
                    "apos" => Some("'"),
                    "nbsp" => Some("\u{00A0}"),
                    _ => None,
                };
                match replacement {
                    Some(r) => {
                        out.push_str(r);
                        rest = tail.get(end + 1..).unwrap_or("");
                    }
                    None => {
                        // Unknown entity: keep the '&' literally and continue.
                        out.push('&');
                        rest = tail.get(1..).unwrap_or("");
                    }
                }
            }
            None => {
                out.push('&');
                rest = tail.get(1..).unwrap_or("");
            }
        }
    }
    out.push_str(rest);
}

/// Parse the freedesktop body-markup subset (`<b> <i> <u> <a href>`) into
/// styled [`Span`]s.  All other tags are stripped, entities are unescaped, and
/// anchors render as underline.  Malformed markup degrades to tag-stripping —
/// this function never panics.
pub fn parse_markup(input: &str) -> Vec<Span> {
    #[derive(Clone, Copy, Default)]
    struct State {
        bold: u32,
        italic: u32,
        underline: u32,
    }
    let mut state = State::default();
    let mut spans: Vec<Span> = Vec::new();
    let mut text = String::new();
    let mut rest = input;

    fn flush(spans: &mut Vec<Span>, text: &mut String, st: State) {
        if !text.is_empty() {
            let mut decoded = String::with_capacity(text.len());
            unescape_into(text, &mut decoded);
            spans.push(Span {
                text: decoded,
                bold: st.bold > 0,
                italic: st.italic > 0,
                underline: st.underline > 0,
            });
            text.clear();
        }
    }

    while let Some(lt) = rest.find('<') {
        let (head, tail) = rest.split_at(lt);
        text.push_str(head);
        match tail.find('>') {
            Some(gt) => {
                let tag_body = tail.get(1..gt).unwrap_or("");
                let closing = tag_body.starts_with('/');
                let name: String = tag_body
                    .trim_start_matches('/')
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric())
                    .collect::<String>()
                    .to_ascii_lowercase();
                if matches!(name.as_str(), "b" | "i" | "u" | "a") {
                    flush(&mut spans, &mut text, state);
                    let bump = |v: &mut u32| {
                        if closing {
                            *v = v.saturating_sub(1);
                        } else {
                            *v = v.saturating_add(1);
                        }
                    };
                    match name.as_str() {
                        "b" => bump(&mut state.bold),
                        "i" => bump(&mut state.italic),
                        "u" | "a" => bump(&mut state.underline),
                        _ => {}
                    }
                }
                // Unknown tags (and the tag text itself) are stripped.
                rest = tail.get(gt + 1..).unwrap_or("");
            }
            None => {
                // '<' with no closing '>': treat the rest as inside a tag
                // (same degradation as strip_markup).
                rest = "";
            }
        }
    }
    text.push_str(rest);
    flush(&mut spans, &mut text, state);
    spans
}

// ── Icon cache key ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum CacheKey {
    /// Filesystem path + target size.
    Path(String, u32),
    /// Freedesktop icon name + target size.
    Name(String, u32),
}

// ── Per-notification geometry (buffer pixels) ─────────────────────────────────

/// Geometry of a single action button (buffer pixels).
struct ButtonGeometry {
    rect: Rect,
    key: String,
    label: String,
}

/// Full per-notification geometry in buffer-pixel space, including the
/// (possibly ellipsis-clamped) text spans so `measure` and `render` always
/// agree on content.
struct NotifGeometry {
    /// Whole-notification rect.
    body: Rect,
    /// Close-button rect.
    close: Rect,
    /// Action buttons (excludes the "default" action).
    actions: Vec<ButtonGeometry>,
    /// Height of the action row (0 if no action buttons), buffer px.
    action_row_h: f32,
    /// Summary spans (bold), clamped to two lines.
    summary_spans: Vec<Span>,
    /// Measured summary height, buffer px.
    summary_h: f32,
    /// Body spans, markup-parsed and ellipsis-clamped to the available height.
    body_spans: Vec<Span>,
}

// ── Text shaping helpers (free functions to allow split borrows) ──────────────

/// Map the config font family string to a cosmic-text [`Family`].
fn family_of(cfg: &Config) -> Family<'_> {
    match cfg.font_family.as_str() {
        "sans-serif" => Family::SansSerif,
        "serif" => Family::Serif,
        "monospace" => Family::Monospace,
        name => Family::Name(name),
    }
}

fn attrs_for<'a>(span: &Span, family: Family<'a>) -> Attrs<'a> {
    let mut attrs = Attrs::new().family(family);
    if span.bold {
        attrs = attrs.weight(cosmic_text::Weight::BOLD);
    }
    if span.italic {
        attrs = attrs.style(cosmic_text::Style::Italic);
    }
    if span.underline {
        attrs = attrs.metadata(META_UNDERLINE);
    }
    attrs
}

/// Shape `spans` at `font_size` constrained to `max_width`; returns the shaped
/// [`Buffer`] plus `(line_count, total_height)`.
fn shape_spans(
    font_system: &mut FontSystem,
    spans: &[Span],
    family: Family<'_>,
    font_size: f32,
    max_width: f32,
) -> (Buffer, usize, f32) {
    let line_height = (font_size * 1.3).ceil();
    let metrics = Metrics::new(font_size, line_height);
    let mut buffer = Buffer::new(font_system, metrics);
    buffer.set_size(Some(max_width.max(1.0)), None);
    let default_attrs = Attrs::new().family(family);
    let rich: Vec<(&str, Attrs)> = spans
        .iter()
        .map(|sp| (sp.text.as_str(), attrs_for(sp, family)))
        .collect();
    buffer.set_rich_text(rich, &default_attrs, Shaping::Advanced, None);
    buffer.shape_until_scroll(font_system, false);

    let mut lines = 0usize;
    let mut height = 0.0f32;
    for run in buffer.layout_runs() {
        lines += 1;
        let bottom = run.line_top + run.line_height;
        if bottom > height {
            height = bottom;
        }
    }
    (buffer, lines, height)
}

/// Truncate `spans` so the shaped text fits within `max_lines` at `max_width`,
/// appending an ellipsis when content was dropped.  Returns the (possibly
/// clamped) spans and their shaped height.
pub fn clamp_spans_to_lines(
    font_system: &mut FontSystem,
    spans: &[Span],
    family: Family<'_>,
    font_size: f32,
    max_width: f32,
    max_lines: usize,
) -> (Vec<Span>, f32) {
    let max_lines = max_lines.max(1);
    let (_, lines, height) = shape_spans(font_system, spans, family, font_size, max_width);
    if lines <= max_lines {
        return (spans.to_vec(), height);
    }

    let total_chars: usize = spans.iter().map(|sp| sp.text.chars().count()).sum();

    let truncate = |budget: usize| -> Vec<Span> {
        let mut remaining = budget;
        let mut out: Vec<Span> = Vec::new();
        for sp in spans {
            if remaining == 0 {
                break;
            }
            let count = sp.text.chars().count();
            if count <= remaining {
                remaining -= count;
                out.push(sp.clone());
            } else {
                let cut: String = sp.text.chars().take(remaining).collect();
                remaining = 0;
                out.push(Span {
                    text: cut,
                    ..sp.clone()
                });
            }
        }
        if let Some(last) = out.last_mut() {
            let trimmed = last.text.trim_end().to_owned();
            last.text = trimmed;
            last.text.push('…');
        } else {
            out.push(Span::plain("…"));
        }
        out
    };

    // Binary-search the largest character budget that fits.
    let mut lo = 0usize;
    let mut hi = total_chars;
    let mut best: Option<(Vec<Span>, f32)> = None;
    while lo < hi {
        let mid = lo + (hi - lo).div_ceil(2);
        let candidate = truncate(mid);
        let (_, lines, h) = shape_spans(font_system, &candidate, family, font_size, max_width);
        if lines <= max_lines {
            best = Some((candidate, h));
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    best.unwrap_or_else(|| {
        let line_height = (font_size * 1.3).ceil();
        (vec![Span::plain("…")], line_height)
    })
}

// ── SkiaRenderer ───────────────────────────────────────────────────────────────

/// Full renderer using tiny-skia for drawing and cosmic-text for font rasterization.
///
/// The [`FontSystem`] and [`SwashCache`] are built once at construction (the
/// system font scan takes ~100 ms) and reused for every frame.
pub struct SkiaRenderer {
    font_system: FontSystem,
    swash_cache: SwashCache,
    icon_cache: HashMap<CacheKey, Option<Pixmap>>,
}

impl SkiaRenderer {
    /// Create a new `SkiaRenderer` with system fonts loaded.
    pub fn new() -> Self {
        Self {
            font_system: FontSystem::new(),
            swash_cache: SwashCache::new(),
            icon_cache: HashMap::new(),
        }
    }

    /// Create a renderer that loads only the given font files.
    ///
    /// Intended for deterministic tests: no system fonts are loaded, and the
    /// sans-serif generic family is mapped to "DejaVu Sans".
    pub fn with_font_files(paths: &[PathBuf]) -> Self {
        let mut db = cosmic_text::fontdb::Database::new();
        for p in paths {
            if let Err(e) = db.load_font_file(p) {
                log::warn!("failed to load test font {p:?}: {e}");
            }
        }
        db.set_sans_serif_family("DejaVu Sans");
        let font_system = FontSystem::new_with_locale_and_db("en-US".to_owned(), db);
        Self {
            font_system,
            swash_cache: SwashCache::new(),
            icon_cache: HashMap::new(),
        }
    }

    /// Mutable access to the font system (used by shaping tests).
    pub fn font_system_mut(&mut self) -> &mut FontSystem {
        &mut self.font_system
    }

    /// Convert a raw `image-data` hint image into a premultiplied RGBA pixmap,
    /// copying row by row so padded rowstrides are honoured.
    ///
    /// Public for testing (rowstride handling).
    pub fn raw_image_to_pixmap(raw: &RawImage) -> Option<Pixmap> {
        if raw.width <= 0 || raw.height <= 0 || raw.rowstride <= 0 || raw.channels < 3 {
            return None;
        }
        let w = raw.width as u32;
        let h = raw.height as u32;
        let mut pixmap = Pixmap::new(w, h)?;

        let data = pixmap.data_mut();
        for row in 0..h {
            for col in 0..w {
                let src_idx =
                    (row as i32 * raw.rowstride) as usize + col as usize * raw.channels as usize;
                let r = raw.data.get(src_idx).copied().unwrap_or(0);
                let g = raw.data.get(src_idx + 1).copied().unwrap_or(0);
                let b = raw.data.get(src_idx + 2).copied().unwrap_or(0);
                let a = if raw.has_alpha {
                    raw.data.get(src_idx + 3).copied().unwrap_or(255)
                } else {
                    255
                };
                let dst_idx = (row * w + col) as usize * 4;
                let scale = a as u32;
                let pre_r = ((r as u32 * scale + 127) / 255) as u8;
                let pre_g = ((g as u32 * scale + 127) / 255) as u8;
                let pre_b = ((b as u32 * scale + 127) / 255) as u8;
                if let Some(px) = data.get_mut(dst_idx..dst_idx + 4) {
                    px.copy_from_slice(&[pre_r, pre_g, pre_b, a]);
                }
            }
        }
        Some(pixmap)
    }

    /// Scale `src` to fit within a `target`×`target` square, preserving aspect.
    fn scale_pixmap(src: &Pixmap, target: u32) -> Option<Pixmap> {
        if target == 0 {
            return None;
        }
        if src.width() == target && src.height() == target {
            return Some(src.clone());
        }
        let scale = (target as f32 / src.width() as f32).min(target as f32 / src.height() as f32);
        let out_w = ((src.width() as f32 * scale).round() as u32).max(1);
        let out_h = ((src.height() as f32 * scale).round() as u32).max(1);
        let mut out = Pixmap::new(out_w, out_h)?;
        let paint = PixmapPaint {
            quality: tiny_skia::FilterQuality::Bilinear,
            ..PixmapPaint::default()
        };
        out.draw_pixmap(
            0,
            0,
            src.as_ref(),
            &paint,
            Transform::from_scale(scale, scale),
            None,
        );
        Some(out)
    }

    fn load_raster_image(path: &PathBuf, target_size: u32) -> Option<Pixmap> {
        let img = image::open(path).ok()?;
        let img = img.resize(
            target_size,
            target_size,
            image::imageops::FilterType::Lanczos3,
        );
        let rgba = img.to_rgba8();
        let (w, h) = rgba.dimensions();
        let mut pixmap = Pixmap::new(w, h)?;
        let data = pixmap.data_mut();
        for (i, pixel) in rgba.pixels().enumerate() {
            let [r, g, b, a] = pixel.0;
            let dst = i * 4;
            let scale = a as u32;
            let pre_r = ((r as u32 * scale + 127) / 255) as u8;
            let pre_g = ((g as u32 * scale + 127) / 255) as u8;
            let pre_b = ((b as u32 * scale + 127) / 255) as u8;
            if let Some(px) = data.get_mut(dst..dst + 4) {
                px.copy_from_slice(&[pre_r, pre_g, pre_b, a]);
            }
        }
        Some(pixmap)
    }

    #[cfg(feature = "svg")]
    fn load_svg(path: &PathBuf, target_size: u32) -> Option<Pixmap> {
        let data = std::fs::read(path).ok()?;
        let opts = resvg::usvg::Options::default();
        let tree = resvg::usvg::Tree::from_data(&data, &opts).ok()?;
        let mut pixmap = Pixmap::new(target_size, target_size)?;
        let size = tree.size();
        let scale_x = target_size as f32 / size.width();
        let scale_y = target_size as f32 / size.height();
        let scale = scale_x.min(scale_y);
        let transform = tiny_skia::Transform::from_scale(scale, scale);
        resvg::render(&tree, transform, &mut pixmap.as_mut());
        Some(pixmap)
    }

    fn load_image_from_path(path: &PathBuf, target_size: u32) -> Option<Pixmap> {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_lowercase();

        #[cfg(feature = "svg")]
        if ext == "svg" {
            return Self::load_svg(path, target_size);
        }
        #[cfg(not(feature = "svg"))]
        if ext == "svg" {
            log::warn!("svg feature disabled; cannot load {path:?}");
            return None;
        }
        let _ = ext;

        Self::load_raster_image(path, target_size)
    }

    /// Resolve a freedesktop icon name to a path, trying the configured theme
    /// first and falling back to the default (hicolor) lookup.
    fn resolve_icon(name: &str, icon_size: u32, theme: Option<&str>) -> Option<PathBuf> {
        let size = icon_size.min(u16::MAX as u32) as u16;
        if let Some(theme_name) = theme
            && let Some(p) = freedesktop_icons::lookup(name)
                .with_size(size)
                .with_theme(theme_name)
                .find()
        {
            return Some(p);
        }
        freedesktop_icons::lookup(name).with_size(size).find()
    }

    /// Load an icon pixmap scaled to `icon_size`, checking the cache first.
    fn load_icon_cached(
        &mut self,
        source: &ImageSource,
        icon_size: u32,
        icon_theme: Option<&str>,
    ) -> Option<Pixmap> {
        match source {
            ImageSource::Data(raw) => {
                // Per-notification raw data — decode + scale, no caching.
                Self::raw_image_to_pixmap(raw).and_then(|p| Self::scale_pixmap(&p, icon_size))
            }
            ImageSource::Path(p) => {
                let key = CacheKey::Path(p.clone(), icon_size);
                if let Some(cached) = self.icon_cache.get(&key) {
                    return cached.clone();
                }
                let result = Self::load_image_from_path(&PathBuf::from(p), icon_size);
                self.icon_cache.insert(key, result.clone());
                result
            }
            ImageSource::Icon(name) => {
                let key = CacheKey::Name(name.clone(), icon_size);
                if let Some(cached) = self.icon_cache.get(&key) {
                    return cached.clone();
                }
                let result = Self::resolve_icon(name, icon_size, icon_theme)
                    .and_then(|path| Self::load_image_from_path(&path, icon_size));
                self.icon_cache.insert(key, result.clone());
                result
            }
        }
    }

    /// Build a rounded-rect path.
    fn rounded_rect_path(x: f32, y: f32, w: f32, h: f32, radius: f32) -> Option<tiny_skia::Path> {
        let r = radius.min(w / 2.0).min(h / 2.0);
        if r <= 0.0 {
            let rect = tiny_skia::Rect::from_xywh(x, y, w, h)?;
            return Some(tiny_skia::PathBuilder::from_rect(rect));
        }
        let k = r * 0.552;
        let mut pb = tiny_skia::PathBuilder::new();
        pb.move_to(x + r, y);
        pb.line_to(x + w - r, y);
        pb.cubic_to(x + w - r + k, y, x + w, y + r - k, x + w, y + r);
        pb.line_to(x + w, y + h - r);
        pb.cubic_to(x + w, y + h - r + k, x + w - r + k, y + h, x + w - r, y + h);
        pb.line_to(x + r, y + h);
        pb.cubic_to(x + r - k, y + h, x, y + h - r + k, x, y + h - r);
        pb.line_to(x, y + r);
        pb.cubic_to(x, y + r - k, x + r - k, y, x + r, y);
        pb.close();
        pb.finish()
    }

    /// Draw a rounded rectangle filled with `color`.
    fn fill_rounded_rect(
        pixmap: &mut Pixmap,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        radius: f32,
        color: Color,
    ) {
        if w <= 0.0 || h <= 0.0 {
            return;
        }
        let mut paint = Paint::default();
        paint.set_color(color);
        paint.anti_alias = true;

        if let Some(path) = Self::rounded_rect_path(x, y, w, h, radius) {
            pixmap.fill_path(
                &path,
                &paint,
                tiny_skia::FillRule::Winding,
                Transform::identity(),
                None,
            );
        }
    }

    /// Draw a rounded rectangle border, inset so the stroke stays inside.
    #[allow(clippy::too_many_arguments)]
    fn stroke_rounded_rect(
        pixmap: &mut Pixmap,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        radius: f32,
        border_width: f32,
        color: Color,
    ) {
        if w <= 0.0 || h <= 0.0 || border_width <= 0.0 {
            return;
        }
        let mut paint = Paint::default();
        paint.set_color(color);
        paint.anti_alias = true;

        let stroke = Stroke {
            width: border_width,
            ..Stroke::default()
        };

        let half = border_width / 2.0;
        if let Some(path) = Self::rounded_rect_path(
            x + half,
            y + half,
            (w - border_width).max(0.0),
            (h - border_width).max(0.0),
            (radius - half).max(0.0),
        ) {
            pixmap.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
        }
    }

    /// Render styled spans into `pixmap` at position `(text_x, text_y)`,
    /// clipping below `clip_bottom` (absolute buffer y).
    #[allow(clippy::too_many_arguments)]
    fn render_spans_into(
        pixmap: &mut Pixmap,
        font_system: &mut FontSystem,
        swash_cache: &mut SwashCache,
        spans: &[Span],
        family: Family<'_>,
        text_x: f32,
        text_y: f32,
        max_width: f32,
        clip_bottom: f32,
        font_size: f32,
        fg: Rgba,
    ) {
        if spans.iter().all(|sp| sp.text.is_empty()) || max_width <= 0.0 {
            return;
        }

        let (buffer, _, _) = shape_spans(font_system, spans, family, font_size, max_width);

        let pix_w = pixmap.width() as i32;
        let pix_h = pixmap.height() as i32;
        let clip_y = (clip_bottom.min(pix_h as f32)) as i32;

        for run in buffer.layout_runs() {
            if text_y + run.line_top >= clip_bottom {
                break;
            }
            for glyph in run.glyphs.iter() {
                let physical = glyph.physical((text_x, text_y + run.line_y), 1.0);
                let maybe_img = swash_cache.get_image(font_system, physical.cache_key);
                let image = match maybe_img {
                    Some(img) if img.placement.width > 0 && img.placement.height > 0 => img,
                    _ => continue,
                };

                let glyph_w = image.placement.width as i32;
                let glyph_h = image.placement.height as i32;
                let glyph_x = physical.x + image.placement.left;
                let glyph_y = physical.y - image.placement.top;

                let px_data = pixmap.data_mut();

                match image.content {
                    SwashContent::Mask | SwashContent::SubpixelMask => {
                        for gy in 0..glyph_h {
                            let py = glyph_y + gy;
                            if py < 0 || py >= pix_h || py >= clip_y {
                                continue;
                            }
                            for gx in 0..glyph_w {
                                let px_x = glyph_x + gx;
                                if px_x < 0 || px_x >= pix_w {
                                    continue;
                                }
                                let mask_idx = (gy * glyph_w + gx) as usize;
                                let alpha = image.data.get(mask_idx).copied().unwrap_or(0);
                                if alpha == 0 {
                                    continue;
                                }
                                let dst_idx = (py * pix_w + px_x) as usize * 4;
                                let src_a = fg.a as u32 * alpha as u32 / 255;
                                let inv_a = 255 - src_a;
                                let dr = px_data.get(dst_idx).copied().unwrap_or(0);
                                let dg = px_data.get(dst_idx + 1).copied().unwrap_or(0);
                                let db = px_data.get(dst_idx + 2).copied().unwrap_or(0);
                                let da = px_data.get(dst_idx + 3).copied().unwrap_or(0);

                                // Premultiplied src-over.
                                let out_r = ((fg.r as u32 * src_a + dr as u32 * inv_a) / 255) as u8;
                                let out_g = ((fg.g as u32 * src_a + dg as u32 * inv_a) / 255) as u8;
                                let out_b = ((fg.b as u32 * src_a + db as u32 * inv_a) / 255) as u8;
                                let out_a = ((src_a * 255 + da as u32 * inv_a) / 255) as u8;

                                if let Some(dst_px) = px_data.get_mut(dst_idx..dst_idx + 4) {
                                    dst_px.copy_from_slice(&[out_r, out_g, out_b, out_a]);
                                }
                            }
                        }
                    }
                    SwashContent::Color => {
                        for gy in 0..glyph_h {
                            let py = glyph_y + gy;
                            if py < 0 || py >= pix_h || py >= clip_y {
                                continue;
                            }
                            for gx in 0..glyph_w {
                                let px_x = glyph_x + gx;
                                if px_x < 0 || px_x >= pix_w {
                                    continue;
                                }
                                let src_idx = (gy * glyph_w + gx) as usize * 4;
                                let sr = image.data.get(src_idx).copied().unwrap_or(0);
                                let sg = image.data.get(src_idx + 1).copied().unwrap_or(0);
                                let sb = image.data.get(src_idx + 2).copied().unwrap_or(0);
                                let sa = image.data.get(src_idx + 3).copied().unwrap_or(0);
                                if sa == 0 {
                                    continue;
                                }
                                let dst_idx = (py * pix_w + px_x) as usize * 4;
                                let dr = px_data.get(dst_idx).copied().unwrap_or(0);
                                let dg = px_data.get(dst_idx + 1).copied().unwrap_or(0);
                                let db = px_data.get(dst_idx + 2).copied().unwrap_or(0);
                                let da = px_data.get(dst_idx + 3).copied().unwrap_or(0);

                                // Straight-alpha src over premultiplied dst.
                                let inv_sa = 255 - sa as u32;
                                let out_r = ((sr as u32 * sa as u32 / 255)
                                    + dr as u32 * inv_sa / 255)
                                    as u8;
                                let out_g = ((sg as u32 * sa as u32 / 255)
                                    + dg as u32 * inv_sa / 255)
                                    as u8;
                                let out_b = ((sb as u32 * sa as u32 / 255)
                                    + db as u32 * inv_sa / 255)
                                    as u8;
                                let out_a = (sa as u32 + da as u32 * inv_sa / 255).min(255) as u8;

                                if let Some(dst_px) = px_data.get_mut(dst_idx..dst_idx + 4) {
                                    dst_px.copy_from_slice(&[out_r, out_g, out_b, out_a]);
                                }
                            }
                        }
                    }
                }
            }

            // Manual underline for <u> / <a> spans (via glyph metadata).
            for glyph in run.glyphs.iter() {
                if glyph.metadata & META_UNDERLINE == 0 {
                    continue;
                }
                let ux = text_x + glyph.x;
                let uy = text_y + run.line_y + 2.0;
                if uy >= clip_bottom {
                    continue;
                }
                let mut paint = Paint::default();
                // Premultiply the underline colour.
                let a32 = fg.a as u32;
                paint.set_color(Color::from_rgba8(
                    ((fg.r as u32 * a32 + 127) / 255) as u8,
                    ((fg.g as u32 * a32 + 127) / 255) as u8,
                    ((fg.b as u32 * a32 + 127) / 255) as u8,
                    fg.a,
                ));
                let uh = (font_size / 13.0).max(1.0);
                if let Some(rect) = tiny_skia::Rect::from_xywh(ux, uy, glyph.w, uh) {
                    pixmap.fill_rect(rect, &paint, Transform::identity(), None);
                }
            }
        }
    }

    /// Measure the natural single-line width (buffer px) of `text`.
    fn measure_text_width_with(
        font_system: &mut FontSystem,
        text: &str,
        family: Family<'_>,
        font_size: f32,
    ) -> f32 {
        if text.is_empty() {
            return 0.0;
        }
        let spans = [Span::plain(text)];
        let (buffer, _, _) = shape_spans(font_system, &spans, family, font_size, 100_000.0);
        let mut max_w: f32 = 0.0;
        for run in buffer.layout_runs() {
            if run.line_w > max_w {
                max_w = run.line_w;
            }
        }
        max_w
    }

    /// Compute the full geometry of one notification in **buffer pixels**,
    /// anchored at buffer-pixel `y`.  Used identically by `measure` and `render`
    /// so hit regions, text content, and drawing always agree.
    fn compute_geometry(
        font_system: &mut FontSystem,
        item: &DisplayNotification,
        cfg: &Config,
        scale: f64,
        y: i32,
    ) -> NotifGeometry {
        let s = scale as f32;
        let bw = (cfg.max_width as f64 * scale).ceil() as u32;
        let padding = PADDING * s;
        let icon_size = cfg.icon_size as f32 * s;
        let font_size = cfg.font_size * s;
        let line_h = (font_size * 1.3).ceil();
        let family = family_of(cfg);

        let has_icon = item.notification.image.is_some() || !item.notification.app_icon.is_empty();
        let text_x_offset = if has_icon { icon_size + padding } else { 0.0 };
        let text_w = (bw as f32 - padding - text_x_offset - padding).max(50.0);
        // The summary shares its row with the close button.
        let summary_w = (text_w - CLOSE_SIZE * s - CLOSE_INSET * s).max(40.0);

        let max_h_buf = (cfg.max_height as f64 * scale) as f32;

        // Summary: bold, clamped to two lines.
        let summary_src = vec![Span {
            text: item.notification.summary.clone(),
            bold: true,
            italic: false,
            underline: false,
        }];
        let (summary_spans, summary_h) = clamp_spans_to_lines(
            font_system,
            &summary_src,
            family,
            font_size * 1.1,
            summary_w,
            2,
        );
        let summary_h = summary_h.max(line_h);

        // Body: markup subset when enabled, plain otherwise.
        let body_src: Vec<Span> = if item.notification.body.is_empty() {
            Vec::new()
        } else if cfg.body_markup {
            parse_markup(&item.notification.body)
        } else {
            vec![Span::plain(item.notification.body.clone())]
        };
        let body_is_empty = body_src.iter().all(|sp| sp.text.trim().is_empty());

        // Action buttons (excluding "default", which is click-on-body).
        let action_labels: Vec<(&str, &str)> = item
            .notification
            .actions
            .iter()
            .filter(|a| a.key != "default")
            .map(|a| (a.key.as_str(), a.label.as_str()))
            .collect();

        let btn_h = font_size * 1.8;
        let btn_gap = BTN_GAP * s;
        let action_row_h = if action_labels.is_empty() {
            0.0
        } else {
            btn_h + btn_gap
        };

        // Clamp the body to the height remaining below the summary.
        let body_gap = if body_is_empty { 0.0 } else { padding * 0.3 };
        let body_avail = max_h_buf - padding * 2.0 - summary_h - body_gap - action_row_h;
        let max_body_lines = ((body_avail / line_h).floor() as usize).max(1);
        let (body_spans, body_h) = if body_is_empty {
            (Vec::new(), 0.0)
        } else {
            clamp_spans_to_lines(
                font_system,
                &body_src,
                family,
                font_size,
                text_w,
                max_body_lines,
            )
        };

        let text_h = summary_h + body_gap + body_h;
        let inner_h = text_h.max(if has_icon { icon_size } else { 0.0 });
        let total = (inner_h + padding * 2.0 + action_row_h)
            .min(max_h_buf)
            .max(MIN_NOTIF_H * s);
        let total_h = total as u32;

        let body_rect = Rect {
            x: 0,
            y,
            width: bw,
            height: total_h,
        };

        let close_sz = (CLOSE_SIZE * s) as u32;
        let inset = (CLOSE_INSET * s) as i32;
        let close_rect = Rect {
            x: bw as i32 - inset - close_sz as i32,
            y: y + inset,
            width: close_sz,
            height: close_sz,
        };

        let mut actions = Vec::with_capacity(action_labels.len());
        if !action_labels.is_empty() {
            let btn_y = y + total_h as i32 - (padding + btn_h) as i32;
            let mut bx = padding as i32;
            for (key, label) in action_labels {
                let label_w = Self::measure_text_width_with(font_system, label, family, font_size);
                // Generous slack so shaping-width rounding never wraps the label.
                let btn_w = (label_w.ceil() + padding * 1.5).max(btn_h) as u32;
                if bx + btn_w as i32 > bw as i32 - padding as i32 {
                    break; // No horizontal room for further buttons.
                }
                actions.push(ButtonGeometry {
                    rect: Rect {
                        x: bx,
                        y: btn_y,
                        width: btn_w,
                        height: btn_h as u32,
                    },
                    key: key.to_owned(),
                    label: label.to_owned(),
                });
                bx += btn_w as i32 + btn_gap as i32;
            }
        }

        NotifGeometry {
            body: body_rect,
            close: close_rect,
            actions,
            action_row_h,
            summary_spans,
            summary_h,
            body_spans,
        }
    }

    /// Draw the close button ("×") into the pixmap.
    fn draw_close_button(pixmap: &mut Pixmap, rect: &Rect, scale: f64, fg: Rgba, hovered: bool) {
        let s = scale as f32;
        let x = rect.x as f32;
        let y = rect.y as f32;
        let w = rect.width as f32;
        let h = rect.height as f32;

        if hovered {
            // Subtle brighter square behind the ×.
            let hl = Color::from_rgba8(255, 255, 255, 0x30);
            Self::fill_rounded_rect(pixmap, x, y, w, h, 4.0 * s, hl);
        }

        let m = w * 0.3;
        let mut pb = tiny_skia::PathBuilder::new();
        pb.move_to(x + m, y + m);
        pb.line_to(x + w - m, y + h - m);
        pb.move_to(x + w - m, y + m);
        pb.line_to(x + m, y + h - m);
        let path = match pb.finish() {
            Some(p) => p,
            None => return,
        };

        let mut paint = Paint::default();
        let alpha = if hovered { fg.a } else { fg.a / 2 };
        paint.set_color(Color::from_rgba8(fg.r, fg.g, fg.b, alpha));
        paint.anti_alias = true;
        let stroke = Stroke {
            width: (1.5 * s).max(1.0),
            ..Stroke::default()
        };
        pixmap.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
    }

    fn render_notification(
        &mut self,
        pixmap: &mut Pixmap,
        item: &DisplayNotification,
        cfg: &Config,
        scale: f64,
        geo: &NotifGeometry,
        hover: Option<&HitTarget>,
    ) {
        let id = item.notification.id;
        let style = match item.notification.urgency {
            Urgency::Low => &cfg.low,
            Urgency::Normal => &cfg.normal,
            Urgency::Critical => &cfg.critical,
        };

        let s = scale as f32;
        let x = geo.body.x as f32;
        let y = geo.body.y as f32;
        let w = geo.body.width as f32;
        let h = geo.body.height as f32;
        let family = family_of(cfg);

        let body_hovered = hover.is_some_and(|t| *t == HitTarget::Body(id));

        let bg = style.background;
        let (br, bg_g, bb, ba) = if body_hovered {
            (
                bg.r.saturating_add(0x28),
                bg.g.saturating_add(0x28),
                bg.b.saturating_add(0x28),
                bg.a,
            )
        } else {
            (bg.r, bg.g, bg.b, bg.a)
        };

        let bg_color = Color::from_rgba8(br, bg_g, bb, ba);
        let radius = style.corner_radius as f32 * s;

        Self::fill_rounded_rect(pixmap, x, y, w, h, radius, bg_color);

        if style.border_width > 0 {
            let bc = style.border_color;
            let border_color = Color::from_rgba8(bc.r, bc.g, bc.b, bc.a);
            let bw = style.border_width as f32 * s;
            Self::stroke_rounded_rect(pixmap, x, y, w, h, radius, bw, border_color);
        }

        let padding = PADDING * s;
        let icon_size_px = cfg.icon_size as f32 * s;
        let font_size = cfg.font_size * s;
        let fg = style.foreground;

        // Icon: image hint takes precedence, then app_icon (path or name).
        let icon_src = item.notification.image.clone().or_else(|| {
            let app_icon = &item.notification.app_icon;
            if app_icon.is_empty() {
                None
            } else if let Some(path) = app_icon.strip_prefix("file://") {
                Some(ImageSource::Path(path.to_owned()))
            } else if app_icon.starts_with('/') {
                Some(ImageSource::Path(app_icon.clone()))
            } else {
                Some(ImageSource::Icon(app_icon.clone()))
            }
        });

        let mut icon_x_offset = 0.0_f32;
        if let Some(src) = icon_src {
            let icon_target = (cfg.icon_size as f64 * scale) as u32;
            let theme = cfg.icon_theme.as_deref();
            let icon_px = self.load_icon_cached(&src, icon_target.max(1), theme);
            if let Some(ipx) = icon_px {
                let ix = (x + padding) as i32;
                // Vertically center the icon in the card.
                let iy = (y + (h - ipx.height() as f32) / 2.0).max(y + padding) as i32;
                pixmap.draw_pixmap(
                    ix,
                    iy,
                    ipx.as_ref(),
                    &PixmapPaint::default(),
                    Transform::identity(),
                    None,
                );
                icon_x_offset = icon_size_px + padding;
            }
        }

        let text_x = x + padding + icon_x_offset;
        let text_w = (w - padding - icon_x_offset - padding).max(10.0);
        let summary_w = (text_w - CLOSE_SIZE * s - CLOSE_INSET * s).max(40.0);
        let clip_bottom = y + h - padding - geo.action_row_h;

        // Summary (bold, slightly larger).
        Self::render_spans_into(
            pixmap,
            &mut self.font_system,
            &mut self.swash_cache,
            &geo.summary_spans,
            family,
            text_x,
            y + padding,
            summary_w,
            clip_bottom,
            font_size * 1.1,
            fg,
        );

        // Body below the summary (markup-styled, ellipsis-clamped).
        if !geo.body_spans.is_empty() {
            let body_y = y + padding + geo.summary_h + padding * 0.3;
            Self::render_spans_into(
                pixmap,
                &mut self.font_system,
                &mut self.swash_cache,
                &geo.body_spans,
                family,
                text_x,
                body_y,
                text_w,
                clip_bottom,
                font_size,
                fg,
            );
        }

        // Close button.
        let close_hovered = hover.is_some_and(|t| *t == HitTarget::CloseButton(id));
        Self::draw_close_button(pixmap, &geo.close, scale, fg, close_hovered);

        // Action buttons.
        for btn in &geo.actions {
            let btn_hovered = hover.is_some_and(|t| {
                matches!(t, HitTarget::ActionButton { id: hid, key } if *hid == id && *key == btn.key)
            });

            let boost = if btn_hovered { 0x38 } else { 0x18 };
            let btn_bg = Color::from_rgba8(
                bg.r.saturating_add(boost),
                bg.g.saturating_add(boost),
                bg.b.saturating_add(boost),
                bg.a,
            );
            let bx = btn.rect.x as f32;
            let by = btn.rect.y as f32;
            let bw2 = btn.rect.width as f32;
            let bh2 = btn.rect.height as f32;
            Self::fill_rounded_rect(pixmap, bx, by, bw2, bh2, 4.0 * s, btn_bg);

            // Center-ish the label.
            let line_h = (font_size * 1.3).ceil();
            let label_y = by + ((bh2 - line_h) * 0.5).max(0.0);
            let label_spans = [Span::plain(btn.label.clone())];
            Self::render_spans_into(
                pixmap,
                &mut self.font_system,
                &mut self.swash_cache,
                &label_spans,
                family,
                bx + padding * 0.5,
                label_y,
                (bw2 - padding * 0.5).max(4.0),
                by + bh2,
                font_size,
                fg,
            );
        }
    }
}

impl Default for SkiaRenderer {
    fn default() -> Self {
        Self::new()
    }
}

impl Renderer for SkiaRenderer {
    fn measure(&mut self, items: &[DisplayNotification], cfg: &Config, scale: f64) -> Layout {
        if items.is_empty() {
            return Layout {
                width: 0,
                height: 0,
                hit_regions: Vec::new(),
            };
        }

        let gap_buf = (cfg.gap as f64 * scale).round() as i32;
        let mut hit_regions = Vec::new();
        let mut y_cursor = 0i32;

        for item in items {
            let id = item.notification.id;
            let geo = Self::compute_geometry(&mut self.font_system, item, cfg, scale, y_cursor);

            // notif-wl hit-tests with `.find()`, so push the more specific
            // targets (close, action buttons) before the whole-body region.
            hit_regions.push(HitRegion {
                rect: geo.close,
                target: HitTarget::CloseButton(id),
            });
            for btn in &geo.actions {
                hit_regions.push(HitRegion {
                    rect: btn.rect,
                    target: HitTarget::ActionButton {
                        id,
                        key: btn.key.clone(),
                    },
                });
            }
            hit_regions.push(HitRegion {
                rect: geo.body,
                target: HitTarget::Body(id),
            });

            y_cursor += geo.body.height as i32 + gap_buf;
        }

        let total_buf_h = (y_cursor - gap_buf).max(0) as u32;
        let logical_h = (total_buf_h as f64 / scale).ceil() as u32;

        Layout {
            width: cfg.max_width,
            height: logical_h,
            hit_regions,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn render(
        &mut self,
        buf: &mut [u8],
        stride: u32,
        layout: &Layout,
        items: &[DisplayNotification],
        cfg: &Config,
        scale: f64,
        hover: Option<&HitTarget>,
    ) {
        buf.fill(0);

        if layout.width == 0 || layout.height == 0 {
            return;
        }

        let buf_w = ((layout.width as f64 * scale).ceil()) as u32;
        let buf_h = ((layout.height as f64 * scale).ceil()) as u32;

        if buf_w == 0 || buf_h == 0 {
            return;
        }

        let mut pixmap = match Pixmap::new(buf_w, buf_h) {
            Some(p) => p,
            None => return,
        };

        // Recompute per-notification geometry exactly as measure() does so the
        // drawing agrees with the published hit regions.
        let gap_buf = (cfg.gap as f64 * scale).round() as i32;
        let mut y_cursor = 0i32;

        for item in items {
            let geo = Self::compute_geometry(&mut self.font_system, item, cfg, scale, y_cursor);
            self.render_notification(&mut pixmap, item, cfg, scale, &geo, hover);
            y_cursor += geo.body.height as i32 + gap_buf;
        }

        // Copy RGBA pixmap → BGRA output buffer (wl_shm ARGB8888 little-endian).
        // This is the single swizzle point for the whole crate.
        let pix_data = pixmap.data();
        let pix_w = pixmap.width();

        for row in 0..buf_h.min(pixmap.height()) {
            let buf_row_start = row as usize * stride as usize;
            let pix_row_start = row as usize * pix_w as usize * 4;

            for col in 0..buf_w.min(pix_w) {
                let pix_idx = pix_row_start + col as usize * 4;
                let buf_idx = buf_row_start + col as usize * 4;

                let r = pix_data.get(pix_idx).copied().unwrap_or(0);
                let g = pix_data.get(pix_idx + 1).copied().unwrap_or(0);
                let b = pix_data.get(pix_idx + 2).copied().unwrap_or(0);
                let a = pix_data.get(pix_idx + 3).copied().unwrap_or(0);

                // RGBA → BGRA
                if let Some(px) = buf.get_mut(buf_idx..buf_idx + 4) {
                    px.copy_from_slice(&[b, g, r, a]);
                }
            }
        }
    }
}

// ── strip_markup (plain-text fallback, kept for tests and non-styled paths) ──

impl SkiaRenderer {
    /// Strip simple HTML/Pango markup and decode common XML entities,
    /// returning plain text (no styling).
    ///
    /// Public for testing.
    pub fn strip_markup(s: &str) -> String {
        parse_markup(s)
            .into_iter()
            .map(|sp| sp.text)
            .collect::<String>()
    }
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use std::path::Path;

    fn test_font_system() -> FontSystem {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fonts");
        let mut db = cosmic_text::fontdb::Database::new();
        db.load_font_file(dir.join("DejaVuSans.ttf")).unwrap();
        db.load_font_file(dir.join("DejaVuSans-Bold.ttf")).unwrap();
        db.set_sans_serif_family("DejaVu Sans");
        FontSystem::new_with_locale_and_db("en-US".to_owned(), db)
    }

    // ── Markup parser ──────────────────────────────────────────────────────

    #[test]
    fn markup_plain_text() {
        assert_eq!(
            parse_markup("hello world"),
            vec![Span::plain("hello world")]
        );
    }

    #[test]
    fn markup_styles_and_anchor() {
        let spans = parse_markup("a<b>b</b><i>i</i><u>u</u><a href=\"x\">l</a>");
        assert_eq!(spans.len(), 5);
        assert_eq!(spans[0], Span::plain("a"));
        assert!(spans[1].bold && spans[1].text == "b");
        assert!(spans[2].italic && spans[2].text == "i");
        assert!(spans[3].underline && spans[3].text == "u");
        assert!(spans[4].underline && spans[4].text == "l");
    }

    #[test]
    fn markup_nesting() {
        let spans = parse_markup("<b>bold <i>bolditalic</i> bold</b>");
        assert_eq!(spans.len(), 3);
        assert!(spans[0].bold && !spans[0].italic);
        assert!(spans[1].bold && spans[1].italic);
        assert!(spans[2].bold && !spans[2].italic);
        assert_eq!(spans[1].text, "bolditalic");
    }

    #[test]
    fn markup_strips_unknown_tags() {
        let spans = parse_markup("<img src=\"x\"/>text<span>more</span>");
        let joined: String = spans.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(joined, "textmore");
    }

    #[test]
    fn markup_entities() {
        let spans = parse_markup("&amp;&lt;&gt;&quot;&apos; &unknown; &broken");
        let joined: String = spans.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(joined, "&<>\"' &unknown; &broken");
    }

    #[test]
    fn markup_malformed_never_panics() {
        for s in [
            "<b>unclosed",
            "</b>stray close",
            "<b><i>cross</b></i>",
            "half a tag <",
            "<>",
            "<a href='y",
            "&",
            "&;",
            "<b>&amp",
        ] {
            let _ = parse_markup(s);
        }
        // An unclosed style tag still styles the remainder.
        let spans = parse_markup("<b>unclosed");
        assert_eq!(spans.len(), 1);
        assert!(spans[0].bold);
        assert_eq!(spans[0].text, "unclosed");
    }

    // ── Ellipsis clamping ──────────────────────────────────────────────────

    #[test]
    fn ellipsis_clamp_truncates_long_text() {
        let mut fs = test_font_system();
        let family = Family::Name("DejaVu Sans");
        let long = "word ".repeat(200);
        let spans = vec![Span::plain(long)];
        let (clamped, height) = clamp_spans_to_lines(&mut fs, &spans, family, 13.0, 300.0, 3);
        let joined: String = clamped.iter().map(|s| s.text.as_str()).collect();
        assert!(joined.ends_with('…'), "clamped text must end with ellipsis");
        assert!(joined.len() < 500, "text must actually be truncated");
        // Re-shape to confirm it fits in three lines.
        let (_, lines, _) = shape_spans(&mut fs, &clamped, family, 13.0, 300.0);
        assert!(lines <= 3, "clamped text still occupies {lines} lines");
        assert!(height > 0.0);
    }

    #[test]
    fn ellipsis_clamp_keeps_short_text() {
        let mut fs = test_font_system();
        let family = Family::Name("DejaVu Sans");
        let spans = vec![Span::plain("short")];
        let (clamped, _) = clamp_spans_to_lines(&mut fs, &spans, family, 13.0, 300.0, 3);
        assert_eq!(clamped, spans, "short text must pass through unchanged");
    }

    #[test]
    fn ellipsis_clamp_preserves_styles() {
        let mut fs = test_font_system();
        let family = Family::Name("DejaVu Sans");
        let spans = vec![
            Span {
                text: "bold ".repeat(100),
                bold: true,
                italic: false,
                underline: false,
            },
            Span::plain("plain ".repeat(100)),
        ];
        let (clamped, _) = clamp_spans_to_lines(&mut fs, &spans, family, 13.0, 300.0, 2);
        assert!(!clamped.is_empty());
        assert!(clamped[0].bold, "first span keeps its bold style");
    }

    // ── CJK + emoji shaping (system fonts; run locally with --ignored) ──────

    #[test]
    #[ignore = "requires system fonts with CJK and emoji coverage"]
    fn cjk_emoji_shapes_without_notdef() {
        let mut fs = FontSystem::new();
        let spans = vec![Span::plain("标题 🎉 émoji 中文 body 🚀 かな")];
        let (buffer, _, _) = shape_spans(&mut fs, &spans, Family::SansSerif, 16.0, 10_000.0);
        for run in buffer.layout_runs() {
            for glyph in run.glyphs.iter() {
                assert_ne!(
                    glyph.glyph_id,
                    0,
                    "got .notdef (tofu) for cluster {:?}",
                    run.text.get(glyph.start..glyph.end)
                );
            }
        }
    }
}
