#![forbid(unsafe_code)]

//! `notif-render` — `Renderer` trait and `StubRenderer` for Wayland buffer rendering.
//!
//! Buffer format: ARGB8888 premultiplied, little-endian memory order (bytes: B, G, R, A).
//! This matches `wl_shm::Format::Argb8888`.

use notif_types::{DisplayNotification, Urgency, config::Config};

// ── Public geometry types ─────────────────────────────────────────────────────

/// Axis-aligned rectangle in buffer-pixel space.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    /// Left edge (pixels from left of buffer).
    pub x: i32,
    /// Top edge (pixels from top of buffer).
    pub y: i32,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
}

impl Rect {
    /// Returns `true` if the point `(px, py)` is inside this rect.
    pub fn contains(&self, px: i32, py: i32) -> bool {
        px >= self.x
            && py >= self.y
            && px < self.x + self.width as i32
            && py < self.y + self.height as i32
    }
}

/// What a pointer hit when clicking / hovering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HitTarget {
    /// The notification body; carries the notification ID.
    Body(u32),
    /// The close button for a notification.
    CloseButton(u32),
    /// An action button on a notification.
    ActionButton {
        /// Notification ID.
        id: u32,
        /// Action key string.
        key: String,
    },
}

/// A hit-testable region on the rendered surface.
#[derive(Debug, Clone)]
pub struct HitRegion {
    /// Rectangle in buffer-pixel coordinates.
    pub rect: Rect,
    /// What this region maps to.
    pub target: HitTarget,
}

/// Layout returned by [`Renderer::measure`].
#[derive(Debug, Clone)]
pub struct Layout {
    /// Logical surface width (before scale).
    pub width: u32,
    /// Logical surface height (before scale).
    pub height: u32,
    /// Hit regions in **buffer-pixel** space (already scaled).
    pub hit_regions: Vec<HitRegion>,
}

// ── Renderer trait ────────────────────────────────────────────────────────────

/// A renderer writes notification content into an ARGB8888 shared-memory buffer.
///
/// All sizes in [`Layout`] are logical pixels; the renderer is responsible for multiplying
/// by `scale` to produce the actual buffer dimensions.  Hit regions are stored in
/// buffer-pixel space so that the pointer hit-test code can work directly in that space
/// without an extra coordinate conversion.
pub trait Renderer: Send {
    /// Compute the surface geometry and hit regions for the given notifications.
    ///
    /// `scale` is the fractional scale factor (e.g. 1.5 for a 150 % HiDPI output).
    /// The returned `Layout::width` / `Layout::height` are **logical** pixels; the actual
    /// buffer dimensions are `ceil(width * scale)` × `ceil(height * scale)`.
    fn measure(&mut self, items: &[DisplayNotification], cfg: &Config, scale: f64) -> Layout;

    /// Render the notifications into `buf`.
    ///
    /// * `buf`    — byte slice sized exactly `stride * ceil(layout.height * scale)`.
    /// * `stride` — bytes per row (≥ `ceil(layout.width * scale) * 4`).
    /// * `layout` — as returned by the preceding [`measure`][Self::measure] call.
    /// * `hover`  — the target currently under the pointer, if any.
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
    );
}

// ── StubRenderer ──────────────────────────────────────────────────────────────

/// A minimal renderer that draws solid-colour rectangles for each notification.
///
/// Layout:
///   - Notifications are stacked top-to-bottom with `cfg.gap` pixels between them.
///   - Each notification occupies `cfg.max_width` × `STUB_NOTIF_HEIGHT` logical pixels.
///   - Outer margins: `cfg.margin_x` left/right, `cfg.margin_y` top/bottom.
///   - The whole rect is a `HitTarget::Body(id)`.
///   - Hovered body gets a brightened shade.
///
/// Colours come from the per-urgency background configured in [`Config`].
/// Buffer format: B, G, R, A in memory order (ARGB8888 little-endian, premultiplied).
pub struct StubRenderer;

/// Stub notification height (logical pixels); Task 5 replaces this with content sizing.
const STUB_NOTIF_HEIGHT: u32 = 72;

impl StubRenderer {
    fn urgency_color(cfg: &Config, urgency: Urgency, hovered: bool) -> [u8; 4] {
        let style = match urgency {
            Urgency::Low => &cfg.low,
            Urgency::Normal => &cfg.normal,
            Urgency::Critical => &cfg.critical,
        };
        let bg = style.background;
        // Apply a simple brightness boost for hover: saturating-add to each channel.
        let (r, g, b, a) = if hovered {
            (
                bg.r.saturating_add(0x28),
                bg.g.saturating_add(0x28),
                bg.b.saturating_add(0x28),
                bg.a,
            )
        } else {
            (bg.r, bg.g, bg.b, bg.a)
        };
        // Premultiply by alpha.
        let scale_a = a as u32;
        let pre = |c: u8| ((c as u32 * scale_a + 127) / 255) as u8;
        // Memory order: B, G, R, A  (little-endian ARGB8888).
        [pre(b), pre(g), pre(r), a]
    }
}

impl Renderer for StubRenderer {
    fn measure(&mut self, items: &[DisplayNotification], cfg: &Config, _scale: f64) -> Layout {
        if items.is_empty() {
            return Layout {
                width: 0,
                height: 0,
                hit_regions: Vec::new(),
            };
        }

        let n = items.len() as u32;
        let notif_w = cfg.max_width;
        let notif_h = STUB_NOTIF_HEIGHT;
        let total_h = cfg.margin_y * 2 + n * notif_h + n.saturating_sub(1) * cfg.gap;
        let total_w = cfg.margin_x * 2 + notif_w;

        // Hit regions are in logical coordinates (scale = 1.0 path).
        // The Wayland layer divides pointer logical coords by 1.0 (identity) at scale 1.0,
        // so these regions work directly.  Task 5's full renderer will scale them properly.
        let mut hit_regions = Vec::with_capacity(items.len());
        let mut y_cursor = cfg.margin_y as i32;
        for item in items {
            let id = item.notification.id;
            hit_regions.push(HitRegion {
                rect: Rect {
                    x: cfg.margin_x as i32,
                    y: y_cursor,
                    width: notif_w,
                    height: notif_h,
                },
                target: HitTarget::Body(id),
            });
            y_cursor += (notif_h + cfg.gap) as i32;
        }

        Layout {
            width: total_w,
            height: total_h,
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
        _scale: f64,
        hover: Option<&HitTarget>,
    ) {
        // Clear to fully transparent black.
        buf.fill(0);

        for (idx, item) in items.iter().enumerate() {
            let region = match layout.hit_regions.get(idx) {
                Some(r) => r,
                None => break,
            };
            let Rect {
                x,
                y,
                width,
                height,
            } = region.rect;
            let id = item.notification.id;
            let hovered = hover.is_some_and(|h| *h == HitTarget::Body(id));
            let pixel = Self::urgency_color(cfg, item.notification.urgency, hovered);

            let row_start_byte = y as usize * stride as usize;
            let col_start_byte = x as usize * 4;
            let rect_rows = height as usize;
            let rect_row_bytes = width as usize * 4;

            // Use get_mut to avoid the indexing_slicing lint.
            let buf_from_y = match buf.get_mut(row_start_byte..) {
                Some(s) => s,
                None => break,
            };

            for (row_idx, row) in buf_from_y.chunks_exact_mut(stride as usize).enumerate() {
                if row_idx >= rect_rows {
                    break;
                }
                let row_end = col_start_byte + rect_row_bytes;
                // get_mut to avoid indexing_slicing.
                let cell = match row.get_mut(col_start_byte..row_end) {
                    Some(c) => c,
                    None => break,
                };
                for px in cell.chunks_exact_mut(4) {
                    px.copy_from_slice(&pixel);
                }
            }
        }
    }
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod tests {
    use super::*;
    use notif_types::{DisplayNotification, Notification, Timeout, Urgency, config::Config};
    use std::{collections::HashMap, time::SystemTime};

    fn make_notif(id: u32, urgency: Urgency) -> DisplayNotification {
        DisplayNotification::new(Notification {
            id,
            app_name: "test".into(),
            app_icon: String::new(),
            summary: "Summary".into(),
            body: String::new(),
            actions: Vec::new(),
            urgency,
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

    #[test]
    fn measure_empty() {
        let mut r = StubRenderer;
        let cfg = Config::default();
        let layout = r.measure(&[], &cfg, 1.0);
        assert_eq!(layout.width, 0);
        assert_eq!(layout.height, 0);
        assert!(layout.hit_regions.is_empty());
    }

    #[test]
    fn measure_two_notifications() {
        let mut r = StubRenderer;
        let cfg = Config::default();
        let items = vec![
            make_notif(1, Urgency::Normal),
            make_notif(2, Urgency::Critical),
        ];
        let layout = r.measure(&items, &cfg, 1.0);
        assert_eq!(layout.width, cfg.margin_x * 2 + cfg.max_width);
        assert_eq!(
            layout.height,
            cfg.margin_y * 2 + 2 * STUB_NOTIF_HEIGHT + cfg.gap
        );
        assert_eq!(layout.hit_regions.len(), 2);
        assert_eq!(layout.hit_regions[0].target, HitTarget::Body(1));
        assert_eq!(layout.hit_regions[1].target, HitTarget::Body(2));
    }

    #[test]
    fn rect_contains() {
        let r = Rect {
            x: 10,
            y: 20,
            width: 50,
            height: 30,
        };
        assert!(r.contains(10, 20));
        assert!(r.contains(59, 49));
        assert!(!r.contains(60, 20));
        assert!(!r.contains(10, 50));
        assert!(!r.contains(9, 20));
    }

    #[test]
    fn render_fills_pixels() {
        let mut renderer = StubRenderer;
        let cfg = Config::default();
        let items = vec![make_notif(1, Urgency::Normal)];
        let layout = renderer.measure(&items, &cfg, 1.0);
        let stride = layout.width * 4;
        let mut buf = vec![0u8; (stride * layout.height) as usize];
        renderer.render(&mut buf, stride, &layout, &items, &cfg, 1.0, None);
        // The rect area should be non-zero (the normal bg is not black).
        let region = &layout.hit_regions[0];
        let row_off = region.rect.y as usize * stride as usize;
        let col_off = region.rect.x as usize * 4;
        // Blue channel (byte 0 of BGRA) should be non-zero for the default bg (#1e1e2e).
        let blue = buf[row_off + col_off];
        assert_ne!(blue, 0, "expected non-zero blue in filled rect");
    }
}
