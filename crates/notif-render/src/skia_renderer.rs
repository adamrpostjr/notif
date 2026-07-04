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
use std::time::SystemTime;

use cosmic_text::{Attrs, Buffer, Family, FontSystem, Metrics, Shaping, SwashCache, SwashContent};
use tiny_skia::{Color, Paint, Pixmap, PixmapPaint, Stroke, Transform};

use notif_types::config::Rgba;
use notif_types::{DisplayNotification, ImageSource, RawImage, Urgency, config::Config};

use crate::{HitRegion, HitTarget, Layout, Rect, Renderer};

// ── Layout constants (logical pixels, multiplied by scale at use sites) ───────

/// Inner padding around notification content.
const PADDING: f32 = 12.0;
// ── Center-panel layout constants (logical pixels) ────────────────────────────

/// Height of the center panel header row.
const CENTER_HEADER_H: f32 = 44.0;
/// Horizontal padding inside the center panel.
const CENTER_PADDING: f32 = 10.0;
/// Width of the per-urgency accent bar on the left of each entry.
const CENTER_ACCENT_W: f32 = 4.0;
/// Horizontal gap between accent bar and entry text.
const CENTER_ACCENT_GAP: f32 = 8.0;
/// Size of the '×' close button in center entries.
const CENTER_CLOSE_SIZE: f32 = 20.0;
/// Inset of the '×' close button from the right edge.
const CENTER_CLOSE_INSET: f32 = 6.0;
/// Vertical padding inside each center entry (top and bottom).
const CENTER_ENTRY_PAD: f32 = 8.0;
/// Height of the hairline separator between entries (buffer pixels, not logical).
const CENTER_SEP_H: f32 = 1.0;
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

// ── Alpha premultiplication helper ────────────────────────────────────────────

/// Premultiply straight-alpha RGBA channels into premultiplied RGBA, matching
/// the `+127` rounding convention used throughout this crate.
///
/// Returns `[pre_r, pre_g, pre_b, a]` — identical to the inline math that
/// existed at `raw_image_to_pixmap` and `load_raster_image` before they were
/// unified here.
#[inline]
fn premultiply_rgba(r: u8, g: u8, b: u8, a: u8) -> [u8; 4] {
    let scale = a as u32;
    let pre_r = ((r as u32 * scale + 127) / 255) as u8;
    let pre_g = ((g as u32 * scale + 127) / 255) as u8;
    let pre_b = ((b as u32 * scale + 127) / 255) as u8;
    [pre_r, pre_g, pre_b, a]
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

/// Geometry of a single action button (buffer pixels) with its shaped label buffer.
struct ButtonGeometry {
    rect: Rect,
    key: String,
    /// Pre-shaped cosmic-text buffer for the label (shaped once, reused across renders).
    label_buffer: Buffer,
}

/// Full per-notification geometry in buffer-pixel space, including the
/// (possibly ellipsis-clamped) text spans so `measure` and `render` always
/// agree on content.  Shaped [`Buffer`]s are stored here so that
/// `render_spans_into` never needs to re-shape text.
struct NotifGeometry {
    /// Whole-notification rect.
    body: Rect,
    /// Close-button rect.
    close: Rect,
    /// Action buttons (excludes the "default" action).
    actions: Vec<ButtonGeometry>,
    /// Height of the action row (0 if no action buttons), buffer px.
    action_row_h: f32,
    /// Pre-shaped buffer for the summary text.
    summary_buffer: Buffer,
    /// Measured summary height, buffer px.
    summary_h: f32,
    /// Body spans, markup-parsed and ellipsis-clamped to the available height.
    body_spans: Vec<Span>,
    /// Pre-shaped buffer for the body text (empty spans → dummy buffer that produces no runs).
    body_buffer: Buffer,
}

// ── Frame cache ───────────────────────────────────────────────────────────────

/// Identifies the content that affects layout/rendering.
/// Hover state is intentionally excluded so that hover-only redraws are cache hits.
#[derive(PartialEq)]
struct FrameCacheKey {
    /// Per-item fingerprints (id, text, urgency, actions, image identity).
    items: Vec<ItemKey>,
    /// Scale factor bits (f64::to_bits for exact comparison).
    scale_bits: u64,
    /// Config fingerprint (all layout-affecting fields).
    config_hash: u64,
}

/// Per-notification fingerprint.
#[derive(PartialEq, Eq)]
struct ItemKey {
    id: u32,
    summary: String,
    body: String,
    urgency: notif_types::Urgency,
    /// Action key+label pairs (excluding "default").
    actions: Vec<(String, String)>,
    /// Image identity: None, Some(Path(…)), Some(Icon(…)), or Some(Data) (all raw images are equal).
    image_id: ImageId,
    app_icon: String,
}

#[derive(PartialEq, Eq)]
enum ImageId {
    None,
    Data,
    Path(String),
    Icon(String),
}

/// A cached rendered frame — geometries (including shaped text buffers) for all items.
struct CachedFrame {
    key: FrameCacheKey,
    geometries: Vec<NotifGeometry>,
}

// ── Center-panel geometry ─────────────────────────────────────────────────────

/// Shaped geometry for one entry row in the notification center panel.
struct CenterEntryGeometry {
    /// Full entry bounding rect (buffer px, relative to panel top-left).
    entry_rect: Rect,
    /// Left accent bar (buffer px).
    accent_rect: Rect,
    /// '×' close-button rect (buffer px); target: `HitTarget::HistoryClose(id)`.
    close_rect: Rect,
    /// Pre-shaped summary line (bold).
    summary_buffer: Buffer,
    summary_h: f32,
    /// Pre-shaped meta line ("{app_name} · {relative age}") — dimmed.
    meta_buffer: Buffer,
    meta_h: f32,
    /// Pre-shaped body line (one line, ellipsized); empty buffer → no runs.
    body_buffer: Buffer,
    /// Shaped height of the body line; stored for future layout use.
    #[allow(dead_code)]
    body_h: f32,
    has_body: bool,
}

/// Geometry for the center-panel header row.
struct CenterHeaderGeometry {
    /// Full header rect (buffer px).
    header_rect: Rect,
    /// Pre-shaped title "Notifications (N)".
    title_buffer: Buffer,
    /// "Clear all" text rect (hit target).
    clear_all_rect: Rect,
    /// Pre-shaped "Clear all" text.
    clear_all_buffer: Buffer,
}

/// Cached result of `measure_center`.
struct CenterCachedFrame {
    key: CenterCacheKey,
    header: CenterHeaderGeometry,
    entries: Vec<CenterEntryGeometry>,
    /// "No notifications" placeholder buffer (None when entries is non-empty).
    placeholder_buffer: Option<Buffer>,
    /// Total buffer height (header + all entries + separator pixels).
    total_buf_h: u32,
}

/// Cache key for the center panel — hover is deliberately excluded.
///
/// The `fingerprint` is a single `u64` hash of (entry count, scale bits,
/// config hash, and per-entry: id, summary, body, app_name, urgency,
/// created_at).  No per-entry string allocations; any content change produces
/// a different fingerprint with overwhelming probability.
#[derive(PartialEq)]
struct CenterCacheKey {
    fingerprint: u64,
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

// ── Test-only shape-invocation counter ───────────────────────────────────────

// Thread-local count of `shape_spans` calls; only active under `#[cfg(test)]`.
#[cfg(test)]
thread_local! {
    static SHAPE_COUNT: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// Reset the thread-local shape counter to zero.
#[cfg(test)]
pub fn reset_shape_count() {
    SHAPE_COUNT.with(|c| c.set(0));
}

/// Return the current thread-local shape-invocation count.
#[cfg(test)]
pub fn get_shape_count() -> u32 {
    SHAPE_COUNT.with(|c| c.get())
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
    #[cfg(test)]
    SHAPE_COUNT.with(|c| c.set(c.get() + 1));
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
/// clamped) spans, their shaped height, and the final shaped [`Buffer`].
///
/// The returned [`Buffer`] is the definitive shaped form of the returned spans and
/// should be used directly for rendering to avoid redundant shaping.
pub fn clamp_spans_to_lines(
    font_system: &mut FontSystem,
    spans: &[Span],
    family: Family<'_>,
    font_size: f32,
    max_width: f32,
    max_lines: usize,
) -> (Vec<Span>, f32, Buffer) {
    let max_lines = max_lines.max(1);
    let (buf, lines, height) = shape_spans(font_system, spans, family, font_size, max_width);
    if lines <= max_lines {
        return (spans.to_vec(), height, buf);
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
    let mut best: Option<(Vec<Span>, f32, Buffer)> = None;
    while lo < hi {
        let mid = lo + (hi - lo).div_ceil(2);
        let candidate = truncate(mid);
        let (buf, lines, h) = shape_spans(font_system, &candidate, family, font_size, max_width);
        if lines <= max_lines {
            best = Some((candidate, h, buf));
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    best.unwrap_or_else(|| {
        let line_height = (font_size * 1.3).ceil();
        let fallback = vec![Span::plain("…")];
        let (fb, _, fh) = shape_spans(font_system, &fallback, family, font_size, max_width);
        (fallback, fh.max(line_height), fb)
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
    /// One-slot frame cache: geometry + shaped text buffers for the last measured/rendered set.
    frame_cache: Option<CachedFrame>,
    /// One-slot cache for the center panel geometry.
    center_cache: Option<CenterCachedFrame>,
    /// When `Some`, overrides `SystemTime::now()` in center age computation.
    /// Set in tests for deterministic golden output.
    pub now_override: Option<SystemTime>,
}

impl SkiaRenderer {
    /// Create a new `SkiaRenderer` with system fonts loaded.
    pub fn new() -> Self {
        Self {
            font_system: FontSystem::new(),
            swash_cache: SwashCache::new(),
            icon_cache: HashMap::new(),
            frame_cache: None,
            center_cache: None,
            now_override: None,
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
            frame_cache: None,
            center_cache: None,
            now_override: None,
        }
    }

    /// Mutable access to the font system (used by shaping tests).
    pub fn font_system_mut(&mut self) -> &mut FontSystem {
        &mut self.font_system
    }

    /// Build a [`FrameCacheKey`] from the current items, scale, and config.
    fn make_cache_key(items: &[DisplayNotification], scale: f64, cfg: &Config) -> FrameCacheKey {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let mut hasher = DefaultHasher::new();
        cfg.hash(&mut hasher);
        let config_hash = hasher.finish();

        let item_keys = items
            .iter()
            .map(|item| {
                let n = &item.notification;
                ItemKey {
                    id: n.id,
                    summary: n.summary.clone(),
                    body: n.body.clone(),
                    urgency: n.urgency,
                    actions: n
                        .actions
                        .iter()
                        .filter(|a| a.key != "default")
                        .map(|a| (a.key.clone(), a.label.clone()))
                        .collect(),
                    image_id: match &n.image {
                        None => ImageId::None,
                        Some(ImageSource::Data(_)) => ImageId::Data,
                        Some(ImageSource::Path(p)) => ImageId::Path(p.clone()),
                        Some(ImageSource::Icon(name)) => ImageId::Icon(name.clone()),
                    },
                    app_icon: n.app_icon.clone(),
                }
            })
            .collect();

        FrameCacheKey {
            items: item_keys,
            scale_bits: scale.to_bits(),
            config_hash,
        }
    }

    // ── Center-panel helpers ───────────────────────────────────────────────────

    /// Build a [`CenterCacheKey`] from the current entries, scale, and config.
    ///
    /// All fields that affect center layout are folded into a single `u64`
    /// fingerprint via `DefaultHasher` — no per-entry string allocations.
    /// Hover state is deliberately excluded so hover-only redraws are cache hits.
    fn make_center_cache_key(
        entries: &[DisplayNotification],
        scale: f64,
        cfg: &Config,
    ) -> CenterCacheKey {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let mut hasher = DefaultHasher::new();
        // Config affects layout and colours.
        cfg.hash(&mut hasher);
        // Scale factor.
        scale.to_bits().hash(&mut hasher);
        // Entry count (so an empty → non-empty transition is always a miss).
        entries.len().hash(&mut hasher);
        // Per-entry content fields (hash &str directly — no allocation).
        for dn in entries {
            let n = &dn.notification;
            n.id.hash(&mut hasher);
            n.summary.as_str().hash(&mut hasher);
            n.body.as_str().hash(&mut hasher);
            n.app_name.as_str().hash(&mut hasher);
            (n.urgency as u8).hash(&mut hasher);
            // created_at affects the relative-age string displayed in the meta line.
            n.created_at
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs()
                .hash(&mut hasher);
        }

        CenterCacheKey {
            fingerprint: hasher.finish(),
        }
    }

    /// Format a relative age string given the creation time and current time.
    ///
    /// Results: "just now", "Xm ago", "Xh ago", "Xd ago".
    fn format_relative_age(created_at: SystemTime, now: SystemTime) -> String {
        let secs = now.duration_since(created_at).unwrap_or_default().as_secs();
        if secs < 60 {
            "just now".to_owned()
        } else if secs < 3600 {
            format!("{}m ago", secs / 60)
        } else if secs < 86400 {
            format!("{}h ago", secs / 3600)
        } else {
            format!("{}d ago", secs / 86400)
        }
    }

    /// Build the center panel cache (header + per-entry geometries).
    ///
    /// `buf_w` is the buffer-pixel panel width.  `now` is the reference time
    /// for relative-age strings (set to `now_override` in tests).
    fn compute_center_cache(
        font_system: &mut FontSystem,
        entries: &[DisplayNotification],
        cfg: &Config,
        scale: f64,
        now: SystemTime,
    ) -> CenterCachedFrame {
        let s = scale as f32;
        let buf_w = (cfg.center_width as f64 * scale).ceil() as u32;
        let bw = buf_w as f32;
        let font_size = cfg.font_size * s;
        let family = family_of(cfg);

        // ── Header ────────────────────────────────────────────────────────────
        let header_h = (CENTER_HEADER_H * s).ceil() as u32;
        let header_rect = Rect {
            x: 0,
            y: 0,
            width: buf_w,
            height: header_h,
        };

        let title_text = format!("Notifications ({})", entries.len());
        let title_spans = [Span {
            text: title_text,
            bold: true,
            italic: false,
            underline: false,
        }];
        // Title width is bounded to leave room for the "Clear all" button.
        let clear_all_text = "Clear all";
        let clear_all_spans = [Span::plain(clear_all_text)];
        let (clear_all_buf, _, _) =
            shape_spans(font_system, &clear_all_spans, family, font_size, 200.0);
        let mut ca_w = 0.0f32;
        for run in clear_all_buf.layout_runs() {
            if run.line_w > ca_w {
                ca_w = run.line_w;
            }
        }
        let ca_btn_w = (ca_w.ceil() + CENTER_PADDING * s * 2.0).max(60.0 * s);
        let title_w = (bw - CENTER_PADDING * s - ca_btn_w - CENTER_PADDING * s).max(40.0);
        let (title_buf, _, _) = shape_spans(font_system, &title_spans, family, font_size, title_w);

        let clear_all_rect = Rect {
            x: (bw - CENTER_PADDING * s - ca_btn_w) as i32,
            y: 0,
            width: (ca_btn_w + CENTER_PADDING * s) as u32,
            height: header_h,
        };

        let header_geom = CenterHeaderGeometry {
            header_rect,
            title_buffer: title_buf,
            clear_all_rect,
            clear_all_buffer: clear_all_buf,
        };

        // ── Entries ───────────────────────────────────────────────────────────
        let summary_font_size = font_size * 1.05_f32;
        let meta_font_size = font_size * 0.9_f32;
        let line_h = (font_size * 1.3).ceil();
        let close_sz = (CENTER_CLOSE_SIZE * s) as u32;
        let accent_w = (CENTER_ACCENT_W * s).ceil() as u32;
        let text_x_offset = accent_w as f32 + CENTER_ACCENT_GAP * s;
        let text_w = (bw
            - text_x_offset
            - CENTER_CLOSE_SIZE * s
            - CENTER_CLOSE_INSET * s
            - CENTER_PADDING * s)
            .max(40.0);

        let mut y_cursor = header_h as i32;
        let mut entry_geoms: Vec<CenterEntryGeometry> = Vec::with_capacity(entries.len());

        for dn in entries {
            let n = &dn.notification;

            // Summary: bold, single line.
            let summary_spans = [Span {
                text: n.summary.clone(),
                bold: true,
                italic: false,
                underline: false,
            }];
            let (_, summary_h_raw, summary_buf) = clamp_spans_to_lines(
                font_system,
                &summary_spans,
                family,
                summary_font_size,
                text_w,
                1,
            );
            let summary_h = summary_h_raw.max(line_h);

            // Meta line: "{app_name} · {age}".
            let age = Self::format_relative_age(n.created_at, now);
            let meta_text = if n.app_name.is_empty() {
                age
            } else {
                format!("{} · {}", n.app_name, age)
            };
            let meta_spans = [Span::plain(meta_text)];
            let (_, meta_h_raw, meta_buf) =
                clamp_spans_to_lines(font_system, &meta_spans, family, meta_font_size, text_w, 1);
            let meta_h = meta_h_raw.max((meta_font_size * 1.3).ceil());

            // Body line: one line, ellipsized (skip if empty).
            let body_empty = n.body.trim().is_empty();
            let (body_buf, body_h, has_body) = if body_empty {
                let dummy = {
                    let metrics = cosmic_text::Metrics::new(font_size, line_h);
                    let mut b = cosmic_text::Buffer::new(font_system, metrics);
                    b.set_size(Some(text_w.max(1.0)), None);
                    b
                };
                (dummy, 0.0_f32, false)
            } else {
                let body_src: Vec<Span> = if cfg.body_markup {
                    parse_markup(&n.body)
                } else {
                    vec![Span::plain(n.body.clone())]
                };
                let (b_spans, b_h, b_buf) =
                    clamp_spans_to_lines(font_system, &body_src, family, font_size, text_w, 1);
                let _ = b_spans;
                (b_buf, b_h.max(line_h), true)
            };

            let pad = CENTER_ENTRY_PAD * s;
            let text_total_h = summary_h + meta_h + if has_body { body_h } else { 0.0 };
            let entry_h = (pad + text_total_h + pad + CENTER_SEP_H * s).ceil() as u32;

            let entry_rect = Rect {
                x: 0,
                y: y_cursor,
                width: buf_w,
                height: entry_h,
            };
            let accent_rect = Rect {
                x: 0,
                y: y_cursor,
                width: accent_w,
                height: entry_h,
            };
            let close_x = (bw - CENTER_CLOSE_INSET * s - close_sz as f32) as i32;
            let close_y = y_cursor + ((entry_h as f32 - close_sz as f32) / 2.0) as i32;
            let close_rect = Rect {
                x: close_x,
                y: close_y,
                width: close_sz,
                height: close_sz,
            };

            entry_geoms.push(CenterEntryGeometry {
                entry_rect,
                accent_rect,
                close_rect,
                summary_buffer: summary_buf,
                summary_h,
                meta_buffer: meta_buf,
                meta_h,
                body_buffer: body_buf,
                body_h,
                has_body,
            });

            y_cursor += entry_h as i32;
        }

        // ── Placeholder ───────────────────────────────────────────────────────
        let placeholder_buffer = if entries.is_empty() {
            let placeholder_spans = [Span::plain("No notifications")];
            let (pb, _, _) = shape_spans(
                font_system,
                &placeholder_spans,
                family,
                font_size,
                bw - CENTER_PADDING * s * 2.0,
            );
            Some(pb)
        } else {
            None
        };

        // Total panel height: entries down to y_cursor; if empty, show a fixed placeholder height.
        let total_buf_h = if entries.is_empty() {
            let placeholder_h = (CENTER_ENTRY_PAD * s * 4.0 + font_size * 1.3).ceil() as u32;
            header_h + placeholder_h
        } else {
            y_cursor.max(0) as u32
        };

        CenterCachedFrame {
            key: CenterCacheKey { fingerprint: 0 }, // filled by caller via `frame.key = new_key`
            header: header_geom,
            entries: entry_geoms,
            placeholder_buffer,
            total_buf_h,
        }
    }

    /// Draw a filled rectangle using pre-multiplied RGBA color bytes.
    fn fill_rect_premul(pixmap: &mut Pixmap, rect: &Rect, [r, g, b, a]: [u8; 4]) {
        let px_w = pixmap.width() as i32;
        let px_h = pixmap.height() as i32;
        let x0 = rect.x.max(0);
        let y0 = rect.y.max(0);
        let x1 = (rect.x + rect.width as i32).min(px_w);
        let y1 = (rect.y + rect.height as i32).min(px_h);
        if x0 >= x1 || y0 >= y1 {
            return;
        }
        let data = pixmap.data_mut();
        for row in y0..y1 {
            for col in x0..x1 {
                let idx = (row * px_w + col) as usize * 4;
                if let Some(px) = data.get_mut(idx..idx + 4) {
                    px.copy_from_slice(&[r, g, b, a]);
                }
            }
        }
    }

    /// Render the center panel onto `pixmap` using the cached geometry.
    fn render_center_pixmap(
        &mut self,
        pixmap: &mut Pixmap,
        cfg: &Config,
        scale: f64,
        entries: &[DisplayNotification],
        hover: Option<&HitTarget>,
    ) {
        let s = scale as f32;
        let bw = pixmap.width() as f32;
        let bh = pixmap.height() as f32;

        // ── Background ────────────────────────────────────────────────────────
        let bg = cfg.normal.background;
        // Premultiply panel background (fully opaque).
        let bg_pre = [bg.b, bg.g, bg.r, 0xff];
        // Slightly lighter header background.
        let hdr_bg = [
            bg.b.saturating_add(0x10),
            bg.g.saturating_add(0x10),
            bg.r.saturating_add(0x10),
            0xff,
        ];
        // Separator color: slightly lighter than bg.
        let sep_rgba = [
            bg.b.saturating_add(0x28),
            bg.g.saturating_add(0x28),
            bg.r.saturating_add(0x28),
            0xff,
        ];

        // Borrow the cached frame temporarily.
        let Some(cached) = self.center_cache.take() else {
            return;
        };

        // Fill full panel background.
        let panel_rect = Rect {
            x: 0,
            y: 0,
            width: pixmap.width(),
            height: pixmap.height(),
        };
        Self::fill_rect_premul(pixmap, &panel_rect, bg_pre);

        // ── Header ────────────────────────────────────────────────────────────
        let header_h = cached.header.header_rect.height as f32;
        let hdr_rect = Rect {
            x: 0,
            y: 0,
            width: pixmap.width(),
            height: cached.header.header_rect.height,
        };
        Self::fill_rect_premul(pixmap, &hdr_rect, hdr_bg);

        // Draw title text left-aligned.
        let fg = cfg.normal.foreground;
        Self::render_spans_into(
            pixmap,
            &mut self.font_system,
            &mut self.swash_cache,
            &cached.header.title_buffer,
            CENTER_PADDING * s,
            (header_h - cfg.font_size * s * 1.3) * 0.5,
            header_h,
            cfg.font_size * s,
            fg,
        );

        // Draw "Clear all" button — highlight on hover.
        let clear_all_hovered = hover.is_some_and(|t| *t == HitTarget::ClearAll);
        if clear_all_hovered {
            let hl = [
                bg.b.saturating_add(0x30),
                bg.g.saturating_add(0x30),
                bg.r.saturating_add(0x30),
                0xff,
            ];
            Self::fill_rect_premul(pixmap, &cached.header.clear_all_rect, hl);
        }
        let ca_fg = notif_types::config::Rgba {
            r: fg.r,
            g: fg.g,
            b: fg.b,
            a: if clear_all_hovered {
                fg.a
            } else {
                (fg.a as u32 * 3 / 4) as u8
            },
        };
        let ca_rect = &cached.header.clear_all_rect;
        Self::render_spans_into(
            pixmap,
            &mut self.font_system,
            &mut self.swash_cache,
            &cached.header.clear_all_buffer,
            ca_rect.x as f32 + CENTER_PADDING * s,
            (header_h - cfg.font_size * s * 1.3) * 0.5,
            header_h,
            cfg.font_size * s,
            ca_fg,
        );

        // Separator under header.
        let hdr_sep_rect = Rect {
            x: 0,
            y: hdr_rect.height as i32,
            width: pixmap.width(),
            height: 1,
        };
        Self::fill_rect_premul(pixmap, &hdr_sep_rect, sep_rgba);

        // ── Entries ───────────────────────────────────────────────────────────
        let font_size_buf = cfg.font_size * s;
        let summary_font_size = font_size_buf * 1.05_f32;
        let meta_font_size = font_size_buf * 0.9_f32;

        if cached.entries.is_empty() {
            // "No notifications" placeholder.
            if let Some(ref ph_buf) = cached.placeholder_buffer {
                let ph_y = header_h + CENTER_ENTRY_PAD * s * 2.0;
                // Center the text horizontally.
                let mut ph_w = 0.0f32;
                for run in ph_buf.layout_runs() {
                    if run.line_w > ph_w {
                        ph_w = run.line_w;
                    }
                }
                let ph_x = ((bw - ph_w) / 2.0).max(CENTER_PADDING * s);
                let ph_fg = notif_types::config::Rgba {
                    r: fg.r,
                    g: fg.g,
                    b: fg.b,
                    a: (fg.a as u32 * 3 / 5) as u8,
                };
                Self::render_spans_into(
                    pixmap,
                    &mut self.font_system,
                    &mut self.swash_cache,
                    ph_buf,
                    ph_x,
                    ph_y,
                    bh,
                    font_size_buf,
                    ph_fg,
                );
            }
        } else {
            for (idx, (dn, geo)) in entries.iter().zip(cached.entries.iter()).enumerate() {
                let n = &dn.notification;
                let style = match n.urgency {
                    notif_types::Urgency::Low => &cfg.low,
                    notif_types::Urgency::Normal => &cfg.normal,
                    notif_types::Urgency::Critical => &cfg.critical,
                };
                let entry_y = geo.entry_rect.y as f32;
                let entry_h = geo.entry_rect.height as f32;

                // Accent bar (left edge, urgency border color).
                let bc = style.border_color;
                let accent_pre = premultiply_rgba(bc.r, bc.g, bc.b, bc.a);
                // RGBA → premul RGBA → we store RGBA in pixmap (tiny-skia is RGBA).
                Self::fill_rect_premul(
                    pixmap,
                    &geo.accent_rect,
                    [accent_pre[0], accent_pre[1], accent_pre[2], accent_pre[3]],
                );

                // Row hover highlight (lighten bg slightly).
                let close_hovered = hover.is_some_and(|t| *t == HitTarget::HistoryClose(n.id));

                // Entry text X start (after accent bar + gap).
                let accent_w = geo.accent_rect.width as f32;
                let text_x = accent_w + CENTER_ACCENT_GAP * s;
                let pad = CENTER_ENTRY_PAD * s;
                let mut text_y = entry_y + pad;
                let clip_bottom = entry_y + entry_h - CENTER_SEP_H * s;

                // Summary (bold).
                Self::render_spans_into(
                    pixmap,
                    &mut self.font_system,
                    &mut self.swash_cache,
                    &geo.summary_buffer,
                    text_x,
                    text_y,
                    clip_bottom,
                    summary_font_size,
                    fg,
                );
                text_y += geo.summary_h;

                // Meta line (dimmed).
                let meta_fg = notif_types::config::Rgba {
                    r: fg.r,
                    g: fg.g,
                    b: fg.b,
                    a: (fg.a as u32 * 7 / 10) as u8,
                };
                Self::render_spans_into(
                    pixmap,
                    &mut self.font_system,
                    &mut self.swash_cache,
                    &geo.meta_buffer,
                    text_x,
                    text_y,
                    clip_bottom,
                    meta_font_size,
                    meta_fg,
                );
                text_y += geo.meta_h;

                // Body (if present).
                if geo.has_body {
                    Self::render_spans_into(
                        pixmap,
                        &mut self.font_system,
                        &mut self.swash_cache,
                        &geo.body_buffer,
                        text_x,
                        text_y,
                        clip_bottom,
                        font_size_buf,
                        meta_fg,
                    );
                }

                // '×' close button.
                Self::draw_close_button(pixmap, &geo.close_rect, scale, fg, close_hovered);

                // Hairline separator at bottom of entry (not on last entry).
                if idx + 1 < entries.len() {
                    let sep_y = (entry_y + entry_h - CENTER_SEP_H * s).round() as i32;
                    let sep_rect = Rect {
                        x: 0,
                        y: sep_y,
                        width: pixmap.width(),
                        height: 1,
                    };
                    Self::fill_rect_premul(pixmap, &sep_rect, sep_rgba);
                }
            }
        }

        // Put the cache back.
        self.center_cache = Some(cached);
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
                if let Some(px) = data.get_mut(dst_idx..dst_idx + 4) {
                    px.copy_from_slice(&premultiply_rgba(r, g, b, a));
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
            if let Some(px) = data.get_mut(dst..dst + 4) {
                px.copy_from_slice(&premultiply_rgba(r, g, b, a));
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

    /// Render a pre-shaped [`Buffer`] into `pixmap` at position `(text_x, text_y)`,
    /// clipping below `clip_bottom` (absolute buffer y).
    ///
    /// The buffer must already be shaped (e.g. produced by [`compute_geometry`]);
    /// this function never calls `shape_spans`.
    #[allow(clippy::too_many_arguments)]
    fn render_spans_into(
        pixmap: &mut Pixmap,
        font_system: &mut FontSystem,
        swash_cache: &mut SwashCache,
        buffer: &Buffer,
        text_x: f32,
        text_y: f32,
        clip_bottom: f32,
        font_size: f32,
        fg: Rgba,
    ) {
        // Guard: if the buffer has no width set, there is nothing to render.
        let max_width = buffer.size().0.unwrap_or(0.0);
        if max_width <= 0.0 {
            return;
        }

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

    /// Compute the full geometry of one notification in **buffer pixels**,
    /// anchored at buffer-pixel `y`.  Shapes each text region exactly once and
    /// stores the resulting [`Buffer`]s in [`NotifGeometry`] for direct reuse
    /// by `render_spans_into`.
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

        // Summary: bold, clamped to two lines.  Shaping #1 (per item).
        let summary_src = vec![Span {
            text: item.notification.summary.clone(),
            bold: true,
            italic: false,
            underline: false,
        }];
        let (_summary_spans, summary_h, summary_buffer) = clamp_spans_to_lines(
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
        // Shaping #2 (per item, only when body is non-empty).
        let body_gap = if body_is_empty { 0.0 } else { padding * 0.3 };
        let body_avail = max_h_buf - padding * 2.0 - summary_h - body_gap - action_row_h;
        let max_body_lines = ((body_avail / line_h).floor() as usize).max(1);
        let (body_spans, body_h, body_buffer) = if body_is_empty {
            // No body text — produce a dummy shaped buffer that emits no runs.
            let dummy = {
                let metrics = Metrics::new(font_size, line_h);
                let mut b = Buffer::new(font_system, metrics);
                b.set_size(Some(text_w.max(1.0)), None);
                b
            };
            (Vec::new(), 0.0, dummy)
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
                // Shape once to get both the width and the buffer.
                // Shaping #3+ (per action button, per item).
                let label_spans = [Span::plain(label)];
                let (label_buf, _, _) =
                    shape_spans(font_system, &label_spans, family, font_size, 100_000.0);
                let mut label_w = 0.0f32;
                for run in label_buf.layout_runs() {
                    if run.line_w > label_w {
                        label_w = run.line_w;
                    }
                }
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
                    label_buffer: label_buf,
                });
                bx += btn_w as i32 + btn_gap as i32;
            }
        }

        NotifGeometry {
            body: body_rect,
            close: close_rect,
            actions,
            action_row_h,
            summary_buffer,
            summary_h,
            body_spans,
            body_buffer,
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
        let clip_bottom = y + h - padding - geo.action_row_h;

        // Summary (bold, slightly larger) — use the pre-shaped buffer from the cache.
        Self::render_spans_into(
            pixmap,
            &mut self.font_system,
            &mut self.swash_cache,
            &geo.summary_buffer,
            text_x,
            y + padding,
            clip_bottom,
            font_size * 1.1,
            fg,
        );

        // Body below the summary (markup-styled, ellipsis-clamped) — cached buffer.
        if !geo.body_spans.is_empty() {
            let body_y = y + padding + geo.summary_h + padding * 0.3;
            Self::render_spans_into(
                pixmap,
                &mut self.font_system,
                &mut self.swash_cache,
                &geo.body_buffer,
                text_x,
                body_y,
                clip_bottom,
                font_size,
                fg,
            );
        }

        // Close button.
        let close_hovered = hover.is_some_and(|t| *t == HitTarget::CloseButton(id));
        Self::draw_close_button(pixmap, &geo.close, scale, fg, close_hovered);

        // Action buttons — use pre-shaped label buffers from the cache.
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

            // Center-ish the label using the pre-shaped buffer.
            let line_h = (font_size * 1.3).ceil();
            let label_y = by + ((bh2 - line_h) * 0.5).max(0.0);
            Self::render_spans_into(
                pixmap,
                &mut self.font_system,
                &mut self.swash_cache,
                &btn.label_buffer,
                bx + padding * 0.5,
                label_y,
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
            // Clear cache on empty so a stale entry never blocks a fresh non-empty call.
            self.frame_cache = None;
            return Layout {
                width: 0,
                height: 0,
                hit_regions: Vec::new(),
            };
        }

        let new_key = Self::make_cache_key(items, scale, cfg);

        // Populate the frame cache on a miss.
        if self.frame_cache.as_ref().is_none_or(|c| c.key != new_key) {
            let gap_buf = (cfg.gap as f64 * scale).round() as i32;
            let mut y_cursor = 0i32;
            let mut geometries = Vec::with_capacity(items.len());
            for item in items {
                let geo = Self::compute_geometry(&mut self.font_system, item, cfg, scale, y_cursor);
                y_cursor += geo.body.height as i32 + gap_buf;
                geometries.push(geo);
            }
            self.frame_cache = Some(CachedFrame {
                key: new_key,
                geometries,
            });
        }

        // Build hit_regions from the cached geometries.
        let gap_buf = (cfg.gap as f64 * scale).round() as i32;
        // frame_cache is guaranteed Some by the block above; zip stops early if sizes mismatch.
        let geo_iter: &[NotifGeometry] = self
            .frame_cache
            .as_ref()
            .map_or(&[], |c| c.geometries.as_slice());
        let mut hit_regions = Vec::new();
        let mut y_cursor = 0i32;

        for (item, geo) in items.iter().zip(geo_iter.iter()) {
            let id = item.notification.id;

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

        // Use the frame cache (filled by measure); recompute only on a miss
        // (e.g. render called without a preceding measure for these items).
        let new_key = Self::make_cache_key(items, scale, cfg);
        if self.frame_cache.as_ref().is_none_or(|c| c.key != new_key) {
            let gap_buf = (cfg.gap as f64 * scale).round() as i32;
            let mut y_cursor = 0i32;
            let mut geometries = Vec::with_capacity(items.len());
            for item in items {
                let geo = Self::compute_geometry(&mut self.font_system, item, cfg, scale, y_cursor);
                y_cursor += geo.body.height as i32 + gap_buf;
                geometries.push(geo);
            }
            self.frame_cache = Some(CachedFrame {
                key: new_key,
                geometries,
            });
        }

        // Render each notification using the cached geometries.
        // Borrow the geometries slice before calling render_notification (which
        // takes &mut self) by temporarily taking the frame_cache out.
        // We put it back immediately after.
        // frame_cache guaranteed Some by the block above.
        let Some(cached) = self.frame_cache.take() else {
            return;
        };
        let gap_buf = (cfg.gap as f64 * scale).round() as i32;
        let mut y_cursor = 0i32;
        for (item, geo) in items.iter().zip(cached.geometries.iter()) {
            // Reattach the y-offset: the cache stores geometry with the y
            // from the original compute pass; re-use directly.
            self.render_notification(&mut pixmap, item, cfg, scale, geo, hover);
            y_cursor += geo.body.height as i32 + gap_buf;
        }
        let _ = y_cursor;
        self.frame_cache = Some(cached);

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

    fn measure_center(
        &mut self,
        entries: &[DisplayNotification],
        cfg: &Config,
        scale: f64,
    ) -> Layout {
        let new_key = Self::make_center_cache_key(entries, scale, cfg);

        if self.center_cache.as_ref().is_none_or(|c| c.key != new_key) {
            let now = self.now_override.unwrap_or_else(SystemTime::now);
            let mut frame =
                Self::compute_center_cache(&mut self.font_system, entries, cfg, scale, now);
            frame.key = new_key;
            self.center_cache = Some(frame);
        }

        let Some(cached) = self.center_cache.as_ref() else {
            return Layout::default();
        };
        let total_buf_h = cached.total_buf_h;
        let logical_h = (total_buf_h as f64 / scale).ceil() as u32;

        // Build hit regions for the center panel.
        // Hit regions are stored in buffer-pixel space (already scaled), matching
        // what notif-wl expects for pointer hit-testing.
        let mut hit_regions = Vec::new();

        // "Clear all" button in header (buffer-pixel coordinates).
        let clear_all_buf_rect = cached.header.clear_all_rect;
        hit_regions.push(HitRegion {
            rect: clear_all_buf_rect,
            target: HitTarget::ClearAll,
        });

        // Close button for each entry (buffer-pixel coordinates).
        for (dn, geo) in entries.iter().zip(cached.entries.iter()) {
            hit_regions.push(HitRegion {
                rect: geo.close_rect,
                target: HitTarget::HistoryClose(dn.notification.id),
            });
        }

        Layout {
            width: cfg.center_width,
            height: logical_h,
            hit_regions,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn render_center(
        &mut self,
        buf: &mut [u8],
        stride: u32,
        layout: &Layout,
        entries: &[DisplayNotification],
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

        // Ensure the center cache is populated (measure_center may not have been called).
        let new_key = Self::make_center_cache_key(entries, scale, cfg);
        if self.center_cache.as_ref().is_none_or(|c| c.key != new_key) {
            let now = self.now_override.unwrap_or_else(SystemTime::now);
            let mut frame =
                Self::compute_center_cache(&mut self.font_system, entries, cfg, scale, now);
            frame.key = new_key;
            self.center_cache = Some(frame);
        }

        let mut pixmap = match Pixmap::new(buf_w, buf_h) {
            Some(p) => p,
            None => return,
        };

        self.render_center_pixmap(&mut pixmap, cfg, scale, entries, hover);

        // Copy RGBA pixmap → BGRA output buffer (wl_shm ARGB8888 little-endian).
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
        let (clamped, height, _buf) = clamp_spans_to_lines(&mut fs, &spans, family, 13.0, 300.0, 3);
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
        let (clamped, _, _buf) = clamp_spans_to_lines(&mut fs, &spans, family, 13.0, 300.0, 3);
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
        let (clamped, _, _buf) = clamp_spans_to_lines(&mut fs, &spans, family, 13.0, 300.0, 2);
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

    // ── Frame cache / shape-deduplication regression test ─────────────────────

    /// Helper: build a `DisplayNotification` suitable for shape-count testing.
    fn make_test_dn(id: u32) -> notif_types::DisplayNotification {
        use notif_types::{Action, Notification, Timeout, Urgency};
        use std::{collections::HashMap, time::SystemTime};

        notif_types::DisplayNotification::new(Notification {
            id,
            app_name: "test".into(),
            app_icon: String::new(),
            summary: "Shape-count summary".into(),
            body: "Shape-count body text".into(),
            actions: vec![Action {
                key: "ok".into(),
                label: "OK".into(),
            }],
            urgency: Urgency::Normal,
            expire_timeout: Timeout::Default,
            image: None,
            transient: false,
            resident: false,
            category: None,
            desktop_entry: None,
            created_at: SystemTime::UNIX_EPOCH,
            raw_hints: HashMap::new(),
        })
    }

    /// `measure()` + `render()` + `render(different hover)` on the same items
    /// must shape each text region exactly once (cache fills on measure, both
    /// render calls are cache hits and perform zero additional shaping).
    #[test]
    fn shape_count_regression() {
        use crate::HitTarget;
        use notif_types::config::Config;

        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fonts");
        let paths = vec![dir.join("DejaVuSans.ttf"), dir.join("DejaVuSans-Bold.ttf")];
        let mut renderer = SkiaRenderer::with_font_files(&paths);
        let cfg = Config::default();
        let items = vec![make_test_dn(1), make_test_dn(2)];

        // Reset the shape counter to zero before we start.
        reset_shape_count();
        let before = get_shape_count();

        // measure() — must fill the cache.
        let layout = renderer.measure(&items, &cfg, 1.0);

        let after_measure = get_shape_count();
        let shapes_in_measure = after_measure - before;
        // We expect > 0 shapes (summary + body + button per item).
        assert!(shapes_in_measure > 0, "measure must perform some shaping");

        // render() — must be a cache hit; no new shaping.
        let buf_w = (layout.width as f64 * 1.0).ceil() as u32;
        let buf_h = (layout.height as f64 * 1.0).ceil() as u32;
        let stride = buf_w * 4;
        let mut buf = vec![0u8; (stride * buf_h) as usize];
        renderer.render(&mut buf, stride, &layout, &items, &cfg, 1.0, None);

        let after_render1 = get_shape_count();
        assert_eq!(
            after_render1, after_measure,
            "render() on same items must not reshape (cache hit)"
        );

        // render() again with a different hover — still a cache hit.
        let hover = HitTarget::CloseButton(1);
        renderer.render(&mut buf, stride, &layout, &items, &cfg, 1.0, Some(&hover));

        let after_render2 = get_shape_count();
        assert_eq!(
            after_render2, after_render1,
            "render() with different hover must not reshape (hover not part of cache key)"
        );
    }

    /// `measure_center()` + `render_center()` + `render_center(different hover)` on
    /// the same entries must shape each text region exactly once.  Hover-only center
    /// redraws must be cache hits with zero additional shaping.
    #[test]
    fn shape_count_regression_center() {
        use crate::{HitTarget, Renderer};
        use notif_types::config::Config;
        use std::time::SystemTime;

        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fonts");
        let paths = vec![dir.join("DejaVuSans.ttf"), dir.join("DejaVuSans-Bold.ttf")];
        let mut renderer = SkiaRenderer::with_font_files(&paths);
        // Pin "now" for deterministic relative-age strings.
        renderer.now_override = Some(SystemTime::UNIX_EPOCH);
        let cfg = Config::default();
        let entries = vec![make_test_dn(10), make_test_dn(11)];

        // Reset counter.
        reset_shape_count();
        let before = get_shape_count();

        // measure_center() — must fill the center cache.
        let layout = renderer.measure_center(&entries, &cfg, 1.0);
        let after_measure = get_shape_count();
        let shapes_in_measure = after_measure - before;
        assert!(
            shapes_in_measure > 0,
            "measure_center must perform some shaping"
        );

        // render_center() — must be a cache hit; no new shaping.
        let buf_w = (layout.width as f64 * 1.0).ceil() as u32;
        let buf_h = (layout.height as f64 * 1.0).ceil() as u32;
        let stride = buf_w * 4;
        let mut buf = vec![0u8; (stride * buf_h) as usize];
        renderer.render_center(&mut buf, stride, &layout, &entries, &cfg, 1.0, None);

        let after_render1 = get_shape_count();
        assert_eq!(
            after_render1, after_measure,
            "render_center() on same entries must not reshape (cache hit)"
        );

        // render_center() again with a different hover — still a cache hit.
        let hover = HitTarget::HistoryClose(10);
        renderer.render_center(&mut buf, stride, &layout, &entries, &cfg, 1.0, Some(&hover));

        let after_render2 = get_shape_count();
        assert_eq!(
            after_render2, after_render1,
            "render_center() with different hover must not reshape (hover not part of cache key)"
        );
    }
}
