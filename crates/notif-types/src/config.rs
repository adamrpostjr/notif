//! Pure-data configuration types.
//!
//! These are plain serde data structures — loading, validation, and file
//! watching live in `notif-config`.

/// Corner to anchor the notification stack to.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize, Default,
)]
#[serde(rename_all = "snake_case")]
pub enum AnchorCorner {
    /// Top-left corner.
    TopLeft,
    /// Top-right corner (default).
    #[default]
    TopRight,
    /// Bottom-left corner.
    BottomLeft,
    /// Bottom-right corner.
    BottomRight,
}

/// RGBA color parsed from `"#rrggbb"` or `"#rrggbbaa"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Rgba {
    /// Red channel (0–255).
    pub r: u8,
    /// Green channel (0–255).
    pub g: u8,
    /// Blue channel (0–255).
    pub b: u8,
    /// Alpha channel (0–255, 255 = fully opaque).
    pub a: u8,
}

impl Rgba {
    /// Construct a fully-opaque color from RGB components.
    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b, a: 255 }
    }
}

fn parse_hex_byte(s: &str, start: usize, end: usize) -> Result<u8, String> {
    let slice = s
        .get(start..end)
        .ok_or_else(|| format!("color string too short: {s:?}"))?;
    u8::from_str_radix(slice, 16).map_err(|e| format!("invalid hex byte in {s:?}: {e}"))
}

fn parse_rgba(s: &str) -> Result<Rgba, String> {
    let hex = s
        .strip_prefix('#')
        .ok_or_else(|| format!("missing # prefix in {s:?}"))?;
    match hex.len() {
        6 => {
            let r = parse_hex_byte(hex, 0, 2)?;
            let g = parse_hex_byte(hex, 2, 4)?;
            let b = parse_hex_byte(hex, 4, 6)?;
            Ok(Rgba { r, g, b, a: 255 })
        }
        8 => {
            let r = parse_hex_byte(hex, 0, 2)?;
            let g = parse_hex_byte(hex, 2, 4)?;
            let b = parse_hex_byte(hex, 4, 6)?;
            let a = parse_hex_byte(hex, 6, 8)?;
            Ok(Rgba { r, g, b, a })
        }
        _ => Err(format!(
            "invalid color length in {s:?}: expected 6 or 8 hex digits after #"
        )),
    }
}

impl serde::Serialize for Rgba {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let hex = if self.a == 255 {
            format!("#{:02x}{:02x}{:02x}", self.r, self.g, self.b)
        } else {
            format!("#{:02x}{:02x}{:02x}{:02x}", self.r, self.g, self.b, self.a)
        };
        s.serialize_str(&hex)
    }
}

impl<'de> serde::Deserialize<'de> for Rgba {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        parse_rgba(&s).map_err(serde::de::Error::custom)
    }
}

/// Per-urgency visual style.
#[derive(Debug, Clone, Hash, serde::Serialize, serde::Deserialize)]
pub struct UrgencyStyle {
    /// Background fill color.
    pub background: Rgba,
    /// Foreground (text) color.
    pub foreground: Rgba,
    /// Border color.
    pub border_color: Rgba,
    /// Border width in pixels.
    pub border_width: u32,
    /// Corner radius in pixels.
    pub corner_radius: u32,
    /// Default auto-dismiss timeout in milliseconds (0 = never).
    pub default_timeout_ms: u32,
    /// If true, ignore the per-notification timeout and use `default_timeout_ms`.
    pub ignore_timeout: bool,
}

impl Default for UrgencyStyle {
    fn default() -> Self {
        Self {
            background: Rgba::rgb(0x1e, 0x1e, 0x2e),
            foreground: Rgba::rgb(0xcd, 0xd6, 0xf4),
            border_color: Rgba::rgb(0x31, 0x32, 0x44),
            border_width: 1,
            corner_radius: 8,
            default_timeout_ms: 5000,
            ignore_timeout: false,
        }
    }
}

fn default_normal_style() -> UrgencyStyle {
    UrgencyStyle {
        border_color: Rgba::rgb(0x89, 0xb4, 0xfa),
        default_timeout_ms: 8000,
        ..UrgencyStyle::default()
    }
}

fn default_critical_style() -> UrgencyStyle {
    UrgencyStyle {
        foreground: Rgba::rgb(0xf3, 0x8b, 0xa8),
        border_color: Rgba::rgb(0xf3, 0x8b, 0xa8),
        border_width: 2,
        default_timeout_ms: 0,
        ignore_timeout: true,
        ..UrgencyStyle::default()
    }
}

/// Optional per-field overrides for the notification-center panel.
///
/// Every field falls back to the corresponding main-config value (or
/// `normal`'s style, for colors/border/radius) when unset. See
/// [`Config::center_resolved`].
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct CenterConfig {
    /// Corner to anchor the panel to. Falls back to the main `anchor`.
    pub anchor: Option<AnchorCorner>,
    /// Horizontal margin in pixels. Falls back to the main `margin_x`.
    pub margin_x: Option<u32>,
    /// Vertical margin in pixels. Falls back to the main `margin_y`.
    pub margin_y: Option<u32>,
    /// Panel width in logical pixels. Falls back to `center_width`.
    pub width: Option<u32>,
    /// Maximum number of entries (active + history) shown. Falls back to
    /// `history_limit`.
    pub max_entries: Option<usize>,
    /// Font family name. Falls back to the main `font_family`.
    pub font_family: Option<String>,
    /// Font size in points. Falls back to the main `font_size`.
    pub font_size: Option<f32>,
    /// Panel background color. Falls back to `normal.background`.
    pub background: Option<Rgba>,
    /// Panel text color. Falls back to `normal.foreground`.
    pub foreground: Option<Rgba>,
    /// Panel border color. Falls back to `normal.border_color`.
    pub border_color: Option<Rgba>,
    /// Panel border width in pixels. Falls back to `normal.border_width`.
    pub border_width: Option<u32>,
    /// Panel corner radius in pixels. Falls back to `normal.corner_radius`.
    pub corner_radius: Option<u32>,
}

impl std::hash::Hash for CenterConfig {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.anchor.hash(state);
        self.margin_x.hash(state);
        self.margin_y.hash(state);
        self.width.hash(state);
        self.max_entries.hash(state);
        self.font_family.hash(state);
        self.font_size.map(f32::to_bits).hash(state);
        self.background.hash(state);
        self.foreground.hash(state);
        self.border_color.hash(state);
        self.border_width.hash(state);
        self.corner_radius.hash(state);
    }
}

/// Fully-resolved notification-center style after applying fallbacks from
/// the main [`Config`]. See [`Config::center_resolved`].
#[derive(Debug, Clone, Copy)]
pub struct CenterResolved<'a> {
    /// Corner to anchor the panel to.
    pub anchor: AnchorCorner,
    /// Horizontal margin in pixels.
    pub margin_x: u32,
    /// Vertical margin in pixels.
    pub margin_y: u32,
    /// Panel width in logical pixels.
    pub width: u32,
    /// Maximum number of entries (active + history) shown.
    pub max_entries: usize,
    /// Font family name.
    pub font_family: &'a str,
    /// Font size in points.
    pub font_size: f32,
    /// Panel background color.
    pub background: Rgba,
    /// Panel text color.
    pub foreground: Rgba,
    /// Panel border color.
    pub border_color: Rgba,
    /// Panel border width in pixels.
    pub border_width: u32,
    /// Panel corner radius in pixels.
    pub corner_radius: u32,
}

/// Top-level daemon configuration.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Config {
    /// Corner to anchor the notification stack to.
    pub anchor: AnchorCorner,
    /// Horizontal margin from the screen edge in pixels.
    pub margin_x: u32,
    /// Vertical margin from the screen edge in pixels.
    pub margin_y: u32,
    /// Gap between notifications in pixels.
    pub gap: u32,
    /// Maximum notification width in pixels.
    pub max_width: u32,
    /// Maximum notification height in pixels.
    pub max_height: u32,
    /// Maximum number of notifications visible at once.
    pub max_visible: usize,
    /// Style applied to low-urgency notifications.
    pub low: UrgencyStyle,
    /// Style applied to normal-urgency notifications.
    pub normal: UrgencyStyle,
    /// Style applied to critical-urgency notifications.
    pub critical: UrgencyStyle,
    /// Font family name.
    pub font_family: String,
    /// Font size in points.
    pub font_size: f32,
    /// Icon size in pixels.
    pub icon_size: u32,
    /// Wayland output name to display on, or `None` for the focused output.
    pub output: Option<String>,
    /// Maximum number of notifications retained in history.
    pub history_limit: usize,
    /// Whether to process HTML/Pango markup in notification bodies.
    pub body_markup: bool,
    /// Icon theme name for freedesktop icon lookup.
    #[serde(default)]
    pub icon_theme: Option<String>,
    /// Width of the notification center panel in logical pixels.
    ///
    /// Deprecated: use `[center].width` instead. Valid range: 1–8192.
    /// Default: 400.
    pub center_width: u32,
    /// Notification-center panel overrides. Unset fields fall back to the
    /// corresponding top-level value.
    #[serde(default)]
    pub center: CenterConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            anchor: AnchorCorner::TopRight,
            margin_x: 12,
            margin_y: 12,
            gap: 8,
            max_width: 400,
            max_height: 200,
            max_visible: 5,
            low: UrgencyStyle::default(),
            normal: default_normal_style(),
            critical: default_critical_style(),
            font_family: "sans-serif".to_owned(),
            font_size: 13.0,
            icon_size: 48,
            output: None,
            history_limit: 100,
            body_markup: true,
            icon_theme: None,
            center_width: 400,
            center: CenterConfig::default(),
        }
    }
}

impl Config {
    /// Resolve the notification-center style by applying `[center]`
    /// overrides on top of the main config's fallback values.
    pub fn center_resolved(&self) -> CenterResolved<'_> {
        let c = &self.center;
        CenterResolved {
            anchor: c.anchor.unwrap_or(self.anchor),
            margin_x: c.margin_x.unwrap_or(self.margin_x),
            margin_y: c.margin_y.unwrap_or(self.margin_y),
            width: c.width.unwrap_or(self.center_width),
            // `history_limit` may legitimately be 0 (disable history
            // retention entirely); that's an orthogonal knob and must not
            // also silently empty the center panel's *active* section, so
            // floor the fallback at 1.
            max_entries: c.max_entries.unwrap_or(self.history_limit).max(1),
            font_family: c.font_family.as_deref().unwrap_or(&self.font_family),
            font_size: c.font_size.unwrap_or(self.font_size),
            background: c.background.unwrap_or(self.normal.background),
            foreground: c.foreground.unwrap_or(self.normal.foreground),
            border_color: c.border_color.unwrap_or(self.normal.border_color),
            border_width: c.border_width.unwrap_or(self.normal.border_width),
            corner_radius: c.corner_radius.unwrap_or(self.normal.corner_radius),
        }
    }
}

impl std::hash::Hash for Config {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.anchor.hash(state);
        self.margin_x.hash(state);
        self.margin_y.hash(state);
        self.gap.hash(state);
        self.max_width.hash(state);
        self.max_height.hash(state);
        self.max_visible.hash(state);
        self.low.hash(state);
        self.normal.hash(state);
        self.critical.hash(state);
        self.font_family.hash(state);
        // f32 has no Hash impl; use the bit pattern instead.
        self.font_size.to_bits().hash(state);
        self.icon_size.hash(state);
        self.output.hash(state);
        self.history_limit.hash(state);
        self.body_markup.hash(state);
        self.icon_theme.hash(state);
        self.center_width.hash(state);
        self.center.hash(state);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn center_config_default_is_all_none() {
        assert_eq!(CenterConfig::default(), CenterConfig::default());
        let c = CenterConfig::default();
        assert!(c.anchor.is_none());
        assert!(c.width.is_none());
        assert!(c.background.is_none());
    }

    #[test]
    fn center_resolved_falls_back_to_main_config_when_unset() {
        let cfg = Config::default();
        let r = cfg.center_resolved();
        assert_eq!(r.anchor, cfg.anchor);
        assert_eq!(r.margin_x, cfg.margin_x);
        assert_eq!(r.margin_y, cfg.margin_y);
        assert_eq!(r.width, cfg.center_width);
        assert_eq!(r.max_entries, cfg.history_limit);
        assert_eq!(r.font_family, cfg.font_family);
        assert_eq!(r.font_size, cfg.font_size);
        assert_eq!(r.background, cfg.normal.background);
        assert_eq!(r.foreground, cfg.normal.foreground);
        assert_eq!(r.border_color, cfg.normal.border_color);
        assert_eq!(r.border_width, cfg.normal.border_width);
        assert_eq!(r.corner_radius, cfg.normal.corner_radius);
    }

    #[test]
    fn center_resolved_max_entries_floors_at_one_when_history_limit_is_zero() {
        let cfg = Config {
            history_limit: 0,
            ..Config::default()
        };
        assert_eq!(
            cfg.center_resolved().max_entries,
            1,
            "history_limit=0 (disable history retention) must not also blank the \
             center panel's active section"
        );
    }

    #[test]
    fn center_resolved_per_field_override() {
        let cfg = Config {
            center: CenterConfig {
                width: Some(999),
                background: Some(Rgba::rgb(1, 2, 3)),
                ..CenterConfig::default()
            },
            ..Config::default()
        };
        let r = cfg.center_resolved();
        assert_eq!(r.width, 999);
        assert_eq!(r.background, Rgba::rgb(1, 2, 3));
        // Untouched fields still fall back.
        assert_eq!(r.margin_x, cfg.margin_x);
        assert_eq!(r.anchor, cfg.anchor);
    }

    #[test]
    fn center_width_field_beats_deprecated_top_level_center_width() {
        let cfg = Config {
            center_width: 123,
            center: CenterConfig {
                width: Some(456),
                ..CenterConfig::default()
            },
            ..Config::default()
        };
        assert_eq!(cfg.center_resolved().width, 456);
    }
}
