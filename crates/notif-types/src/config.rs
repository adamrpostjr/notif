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
    /// Valid range: 1–8192. Default: 400.
    pub center_width: u32,
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
    }
}
