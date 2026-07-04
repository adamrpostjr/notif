//! Golden and unit tests for `notif-render`'s `SkiaRenderer`.
//!
//! Golden tests render offscreen with a fixed DejaVu font set (in `tests/fonts/`)
//! and compare byte-exact against reference PNGs in `tests/golden/`.  On first
//! run (when the reference does not exist) the reference is generated and the
//! test passes.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

use notif_render::{HitTarget, Renderer, SkiaRenderer};
use notif_types::{
    Action, DisplayNotification, ImageSource, Notification, RawImage, Timeout, Urgency,
    config::Config,
};
use std::path::{Path, PathBuf};
use std::{collections::HashMap, time::SystemTime};

// ── Helpers ──────────────────────────────────────────────────────────────────

fn test_fonts() -> Vec<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fonts");
    vec![dir.join("DejaVuSans.ttf"), dir.join("DejaVuSans-Bold.ttf")]
}

fn test_renderer() -> SkiaRenderer {
    SkiaRenderer::with_font_files(&test_fonts())
}

fn make_notif(id: u32, summary: &str, body: &str, urgency: Urgency) -> DisplayNotification {
    make_notif_actions(id, summary, body, urgency, &[])
}

fn make_notif_actions(
    id: u32,
    summary: &str,
    body: &str,
    urgency: Urgency,
    actions: &[(&str, &str)],
) -> DisplayNotification {
    DisplayNotification::new(Notification {
        id,
        app_name: "test-app".into(),
        app_icon: String::new(),
        summary: summary.into(),
        body: body.into(),
        actions: actions
            .iter()
            .map(|(k, l)| Action {
                key: (*k).into(),
                label: (*l).into(),
            })
            .collect(),
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

/// Render `items` offscreen and return `(buf_w, buf_h, rgba_bytes)`.
fn render_rgba(
    renderer: &mut SkiaRenderer,
    items: &[DisplayNotification],
    cfg: &Config,
    scale: f64,
) -> (u32, u32, Vec<u8>) {
    let layout = renderer.measure(items, cfg, scale);
    let buf_w = (layout.width as f64 * scale).ceil() as u32;
    let buf_h = (layout.height as f64 * scale).ceil() as u32;
    let stride = buf_w * 4;
    let mut buf = vec![0u8; (stride * buf_h) as usize];
    renderer.render(&mut buf, stride, &layout, items, cfg, scale, None);

    // BGRA → RGBA
    let mut rgba = buf;
    for px in rgba.chunks_exact_mut(4) {
        px.swap(0, 2);
    }
    (buf_w, buf_h, rgba)
}

/// Compare against `tests/golden/<name>.png`, generating it on first run.
fn assert_golden(name: &str, w: u32, h: u32, rgba: Vec<u8>) {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden");
    let path = dir.join(format!("{name}.png"));
    let img = image::RgbaImage::from_raw(w, h, rgba).expect("failed to build image from buffer");

    if !path.exists() {
        std::fs::create_dir_all(&dir).unwrap();
        img.save(&path).unwrap();
        eprintln!("golden {name}: reference generated at {path:?}");
        return;
    }

    let expected = image::open(&path).unwrap().to_rgba8();
    assert_eq!(
        expected.dimensions(),
        (w, h),
        "golden {name}: dimensions differ"
    );
    assert_eq!(
        expected.as_raw(),
        img.as_raw(),
        "golden {name}: pixel data differs from {path:?}"
    );
}

// ── Golden PNG tests ─────────────────────────────────────────────────────────

#[test]
fn golden_single_normal() {
    let mut r = test_renderer();
    let cfg = Config::default();
    let items = vec![make_notif(
        1,
        "Build finished",
        "All 42 tests passed in 3.2 seconds.",
        Urgency::Normal,
    )];
    let (w, h, rgba) = render_rgba(&mut r, &items, &cfg, 1.0);
    assert_golden("single_normal", w, h, rgba);
}

#[test]
fn golden_critical_actions_markup() {
    let mut r = test_renderer();
    let cfg = Config::default();
    let items = vec![make_notif_actions(
        2,
        "Disk almost full",
        "<b>Bold</b> &amp; <i>italic</i> text",
        Urgency::Critical,
        &[("ok", "OK"), ("dismiss", "Dismiss"), ("default", "Default")],
    )];
    let (w, h, rgba) = render_rgba(&mut r, &items, &cfg, 1.0);
    assert_golden("critical_actions_markup", w, h, rgba);
}

#[test]
fn golden_two_stack() {
    let mut r = test_renderer();
    let cfg = Config::default();
    let items = vec![
        make_notif(1, "First notification", "First body line", Urgency::Low),
        make_notif(
            2,
            "Second notification",
            "Second body line",
            Urgency::Normal,
        ),
    ];
    let (w, h, rgba) = render_rgba(&mut r, &items, &cfg, 1.0);
    assert_golden("two_stack", w, h, rgba);
}

// ── Measure / hit-region tests ───────────────────────────────────────────────

#[test]
fn skia_measure_empty() {
    let mut r = test_renderer();
    let cfg = Config::default();
    let layout = r.measure(&[], &cfg, 1.0);
    assert_eq!(layout.width, 0);
    assert_eq!(layout.height, 0);
    assert!(layout.hit_regions.is_empty());
}

#[test]
fn skia_measure_single_notification() {
    let mut r = test_renderer();
    let cfg = Config::default();
    let items = vec![make_notif(
        1,
        "Test notification",
        "Body text",
        Urgency::Normal,
    )];
    let layout = r.measure(&items, &cfg, 1.0);
    assert_eq!(layout.width, cfg.max_width, "layout width is logical");
    assert!(layout.height > 0);
    // One notification → CloseButton + Body regions.
    assert!(
        layout
            .hit_regions
            .iter()
            .any(|hr| hr.target == HitTarget::Body(1))
    );
    assert!(
        layout
            .hit_regions
            .iter()
            .any(|hr| hr.target == HitTarget::CloseButton(1))
    );
}

#[test]
fn skia_measure_two_notifications_stack_down() {
    let mut r = test_renderer();
    let cfg = Config::default();
    let items = vec![
        make_notif(1, "First", "First body", Urgency::Normal),
        make_notif(2, "Second", "Second body", Urgency::Critical),
    ];
    let layout = r.measure(&items, &cfg, 1.0);
    let body1 = layout
        .hit_regions
        .iter()
        .find(|hr| hr.target == HitTarget::Body(1))
        .unwrap();
    let body2 = layout
        .hit_regions
        .iter()
        .find(|hr| hr.target == HitTarget::Body(2))
        .unwrap();
    assert!(
        body2.rect.y > body1.rect.y,
        "second notif must be below first"
    );
}

#[test]
fn skia_hit_regions_buffer_pixels_at_scale_2() {
    let mut r = test_renderer();
    let cfg = Config::default();
    let scale = 2.0_f64;
    let items = vec![make_notif_actions(
        7,
        "Scaled",
        "Body",
        Urgency::Normal,
        &[("ok", "OK"), ("default", "Default")],
    )];
    let layout = r.measure(&items, &cfg, scale);

    // Layout dimensions are logical.
    assert_eq!(layout.width, cfg.max_width);

    // Hit regions are buffer pixels: body width == ceil(max_width * 2).
    let body = layout
        .hit_regions
        .iter()
        .find(|hr| hr.target == HitTarget::Body(7))
        .unwrap();
    assert_eq!(
        body.rect.width,
        (cfg.max_width as f64 * scale).ceil() as u32
    );

    // Close button exists in the top-right of the scaled body rect.
    let close = layout
        .hit_regions
        .iter()
        .find(|hr| hr.target == HitTarget::CloseButton(7))
        .unwrap();
    assert!(
        close.rect.x > body.rect.width as i32 / 2,
        "close is on the right"
    );
    assert!(close.rect.y >= body.rect.y, "close is inside the body");
    assert_eq!(close.rect.width, (20.0 * scale) as u32);

    // Non-default action gets a button; "default" does not.
    let action_regions: Vec<_> = layout
        .hit_regions
        .iter()
        .filter(|hr| matches!(&hr.target, HitTarget::ActionButton { .. }))
        .collect();
    assert_eq!(action_regions.len(), 1);
    assert!(matches!(
        &action_regions[0].target,
        HitTarget::ActionButton { id: 7, key } if key == "ok"
    ));
    // Action button sits near the bottom of the body.
    let btn = action_regions[0];
    assert!(
        btn.rect.y + btn.rect.height as i32 <= body.rect.y + body.rect.height as i32,
        "button is inside the body rect"
    );
    assert!(
        btn.rect.y > body.rect.y + body.rect.height as i32 / 2,
        "button is in the lower half"
    );
}

#[test]
fn skia_specific_regions_precede_body() {
    // notif-wl uses .find(), so CloseButton/ActionButton must come before Body.
    let mut r = test_renderer();
    let cfg = Config::default();
    let items = vec![make_notif_actions(
        3,
        "Ordering",
        "Body",
        Urgency::Normal,
        &[("yes", "Yes")],
    )];
    let layout = r.measure(&items, &cfg, 1.0);
    let body_idx = layout
        .hit_regions
        .iter()
        .position(|hr| hr.target == HitTarget::Body(3))
        .unwrap();
    let close_idx = layout
        .hit_regions
        .iter()
        .position(|hr| hr.target == HitTarget::CloseButton(3))
        .unwrap();
    let action_idx = layout
        .hit_regions
        .iter()
        .position(|hr| matches!(&hr.target, HitTarget::ActionButton { .. }))
        .unwrap();
    assert!(close_idx < body_idx);
    assert!(action_idx < body_idx);
}

// ── Render tests ─────────────────────────────────────────────────────────────

#[test]
fn skia_render_produces_non_empty_output() {
    let mut r = test_renderer();
    let cfg = Config::default();
    let items = vec![make_notif(1, "Hello", "World", Urgency::Normal)];
    let (_, _, rgba) = render_rgba(&mut r, &items, &cfg, 1.0);
    assert!(
        rgba.iter().any(|&b| b != 0),
        "rendered buffer should not be all zeros"
    );
}

#[test]
fn skia_render_bgra_format() {
    // For the catppuccin default background #1e1e2e, in the raw BGRA buffer
    // byte[0] (B) = 0x2e and byte[2] (R) = 0x1e.
    let mut r = test_renderer();
    let cfg = Config::default();
    let items = vec![make_notif(42, "BGRA check", "", Urgency::Normal)];
    let layout = r.measure(&items, &cfg, 1.0);
    let buf_w = layout.width;
    let buf_h = layout.height;
    let stride = buf_w * 4;
    let mut buf = vec![0u8; (stride * buf_h) as usize];
    r.render(&mut buf, stride, &layout, &items, &cfg, 1.0, None);

    let body = layout
        .hit_regions
        .iter()
        .find(|hr| hr.target == HitTarget::Body(42))
        .unwrap();
    let mid_x = body.rect.x as usize + body.rect.width as usize / 2;
    let mid_y = body.rect.y as usize + body.rect.height as usize / 2;
    let idx = mid_y * stride as usize + mid_x * 4;

    assert_eq!(buf[idx + 3], 0xff, "pixel should be fully opaque");
    assert_eq!(buf[idx], 0x2e, "expected BGRA byte order with B=0x2e");
    assert_eq!(buf[idx + 2], 0x1e, "expected BGRA byte order with R=0x1e");
}

#[test]
fn skia_render_hi_dpi() {
    let mut r = test_renderer();
    let cfg = Config::default();
    let items = vec![make_notif(5, "HiDPI test", "Scale 2x", Urgency::Normal)];
    let (w, h, rgba) = render_rgba(&mut r, &items, &cfg, 2.0);
    assert!(w >= cfg.max_width * 2, "buffer width should be scaled");
    let _ = h;
    assert!(rgba.iter().any(|&b| b != 0));
}

// ── Markup stripping unit tests ──────────────────────────────────────────────

#[test]
fn strip_markup_tags_and_entities() {
    assert_eq!(
        SkiaRenderer::strip_markup("<b>Bold</b> &amp; <i>italic</i>"),
        "Bold & italic"
    );
    assert_eq!(SkiaRenderer::strip_markup("&lt;tag&gt;"), "<tag>");
    assert_eq!(
        SkiaRenderer::strip_markup("&quot;quoted&quot;"),
        "\"quoted\""
    );
    assert_eq!(SkiaRenderer::strip_markup("it&apos;s"), "it's");
    assert_eq!(
        SkiaRenderer::strip_markup("<a href=\"http://x\">link</a>"),
        "link"
    );
}

#[test]
fn strip_markup_malformed_input() {
    // Unclosed tag: content after '<' is treated as inside the tag.
    assert_eq!(SkiaRenderer::strip_markup("<b>unclosed"), "unclosed");
    // Bare '<' swallows the rest (acceptable degradation, must not panic).
    let out = SkiaRenderer::strip_markup("a < b");
    assert!(out.starts_with('a'));
    // Bare ampersand is preserved.
    assert_eq!(SkiaRenderer::strip_markup("fish & chips"), "fish & chips");
    // Unknown entity preserved verbatim.
    assert_eq!(SkiaRenderer::strip_markup("&bogus;"), "&bogus;");
    // Unicode passes through.
    assert_eq!(SkiaRenderer::strip_markup("héllo wörld"), "héllo wörld");
}

// ── RawImage rowstride test ──────────────────────────────────────────────────

#[test]
fn raw_image_padded_rowstride() {
    // 4 px wide, 2 px tall, RGB (3 channels), rowstride padded to 16 bytes
    // (natural would be 12).
    let mut data = vec![0u8; 32];
    // Row 0: red, green, blue, white
    let row0: [(u8, u8, u8); 4] = [(255, 0, 0), (0, 255, 0), (0, 0, 255), (255, 255, 255)];
    for (i, (r, g, b)) in row0.iter().enumerate() {
        data[i * 3] = *r;
        data[i * 3 + 1] = *g;
        data[i * 3 + 2] = *b;
    }
    // Row 1 (offset 16): all gray 0x80
    for i in 0..4 {
        data[16 + i * 3] = 0x80;
        data[16 + i * 3 + 1] = 0x80;
        data[16 + i * 3 + 2] = 0x80;
    }

    let raw = RawImage {
        width: 4,
        height: 2,
        rowstride: 16,
        has_alpha: false,
        bits_per_sample: 8,
        channels: 3,
        data,
    };

    let pixmap = SkiaRenderer::raw_image_to_pixmap(&raw).expect("pixmap should be created");
    assert_eq!(pixmap.width(), 4);
    assert_eq!(pixmap.height(), 2);
    let px = pixmap.data();
    // Pixel (0,0) = red, opaque
    assert_eq!(&px[0..4], &[255, 0, 0, 255]);
    // Pixel (1,0) = green
    assert_eq!(&px[4..8], &[0, 255, 0, 255]);
    // Pixel (2,0) = blue
    assert_eq!(&px[8..12], &[0, 0, 255, 255]);
    // Pixel (0,1) = gray — verifies the padded rowstride was honoured
    assert_eq!(&px[16..20], &[0x80, 0x80, 0x80, 255]);
}

#[test]
fn raw_image_via_render_does_not_panic() {
    let raw = RawImage {
        width: 4,
        height: 4,
        rowstride: 16,
        has_alpha: true,
        bits_per_sample: 8,
        channels: 4,
        data: vec![0x7f; 64],
    };
    let mut n = make_notif(9, "With image", "body", Urgency::Normal);
    n.notification.image = Some(ImageSource::Data(raw));
    let mut r = test_renderer();
    let cfg = Config::default();
    let (_, _, rgba) = render_rgba(&mut r, &[n], &cfg, 1.0);
    assert!(rgba.iter().any(|&b| b != 0));
}

// ── CJK / emoji smoke test (system fonts; ignored by default) ────────────────

#[test]
#[ignore = "depends on system fonts providing CJK/emoji coverage"]
fn cjk_emoji_render_smoke() {
    // With system fonts, CJK and emoji should render (or fall back to .notdef)
    // without panicking. Precisely checking glyph coverage is impractical, so
    // we only assert non-empty output.
    let mut r = SkiaRenderer::new();
    let cfg = Config::default();
    let items = vec![make_notif(
        99,
        "你好世界 🎉 こんにちは",
        "混合 text with emoji 🚀 and かな",
        Urgency::Normal,
    )];
    let (_, _, rgba) = render_rgba(&mut r, &items, &cfg, 1.0);
    assert!(rgba.iter().any(|&b| b != 0));
}
