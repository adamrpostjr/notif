use std::collections::HashMap;

use notif_types::{Action, ImageSource, NewNotification, RawImage, Urgency};
use zvariant::{OwnedValue, Value};

/// Keys that are parsed into typed fields and excluded from raw_hints.
const KNOWN_KEYS: &[&str] = &[
    "urgency",
    "image-data",
    "image_data",
    "image-path",
    "image_path",
    "icon_data",
    "transient",
    "resident",
    "category",
    "desktop-entry",
];

pub fn parse_hints(
    app_name: String,
    app_icon: String,
    summary: String,
    body: String,
    actions_raw: Vec<String>,
    hints: HashMap<String, OwnedValue>,
    expire_timeout: i32,
) -> NewNotification {
    // Parse urgency
    let urgency = hints
        .get("urgency")
        .and_then(parse_urgency)
        .unwrap_or_default();

    // Parse image with precedence: image-data > image_data > image-path > image_path > app_icon > icon_data
    let image: Option<ImageSource> = 'image: {
        if let Some(v) = hints.get("image-data")
            && let Some(raw) = parse_raw_image(v)
        {
            break 'image Some(ImageSource::Data(raw));
        }
        if let Some(v) = hints.get("image_data")
            && let Some(raw) = parse_raw_image(v)
        {
            break 'image Some(ImageSource::Data(raw));
        }
        if let Some(v) = hints.get("image-path")
            && let Some(s) = parse_string_hint(v)
        {
            break 'image Some(ImageSource::Path(s));
        }
        if let Some(v) = hints.get("image_path")
            && let Some(s) = parse_string_hint(v)
        {
            break 'image Some(ImageSource::Path(s));
        }
        // A non-empty app_icon parameter takes precedence over the legacy
        // icon_data hint, but it is not an image hint itself: it stays in the
        // app_icon field and the image stays None.
        if !app_icon.is_empty() {
            break 'image None;
        }
        if let Some(v) = hints.get("icon_data")
            && let Some(raw) = parse_raw_image(v)
        {
            break 'image Some(ImageSource::Data(raw));
        }
        None
    };

    // Parse transient
    let transient = hints
        .get("transient")
        .and_then(parse_bool_hint)
        .unwrap_or(false);

    // Parse resident
    let resident = hints
        .get("resident")
        .and_then(parse_bool_hint)
        .unwrap_or(false);

    // Parse category
    let category = hints.get("category").and_then(parse_string_hint);

    // Parse desktop-entry
    let desktop_entry = hints.get("desktop-entry").and_then(parse_string_hint);

    // Parse actions: flat [key, label, ...] pairs
    let mut actions = Vec::new();
    let mut iter = actions_raw.into_iter();
    loop {
        match iter.next() {
            None => break,
            Some(key) => match iter.next() {
                None => {
                    log::warn!("notif-dbus: trailing unpaired action key {:?} dropped", key);
                    break;
                }
                Some(label) => actions.push(Action { key, label }),
            },
        }
    }

    // Build raw_hints: everything NOT in known_keys
    let raw_hints = hints
        .into_iter()
        .filter(|(k, _)| !KNOWN_KEYS.contains(&k.as_str()))
        .collect();

    NewNotification {
        app_name,
        app_icon,
        summary,
        body,
        actions,
        urgency,
        expire_timeout: expire_timeout.into(),
        image,
        transient,
        resident,
        category,
        desktop_entry,
        raw_hints,
    }
}

fn parse_urgency(v: &OwnedValue) -> Option<Urgency> {
    match Value::from(v.clone()) {
        Value::U8(n) => match n {
            0 => Some(Urgency::Low),
            1 => Some(Urgency::Normal),
            2 => Some(Urgency::Critical),
            other => {
                log::warn!("notif-dbus: unknown urgency byte {other}, using Normal");
                Some(Urgency::Normal)
            }
        },
        Value::U32(n) => match n {
            0 => Some(Urgency::Low),
            1 => Some(Urgency::Normal),
            2 => Some(Urgency::Critical),
            other => {
                log::warn!("notif-dbus: unknown urgency u32 {other}, using Normal");
                Some(Urgency::Normal)
            }
        },
        other => {
            log::warn!(
                "notif-dbus: urgency hint has unexpected type {:?}, ignoring",
                other.value_signature()
            );
            None
        }
    }
}

fn parse_bool_hint(v: &OwnedValue) -> Option<bool> {
    match Value::from(v.clone()) {
        Value::Bool(b) => Some(b),
        Value::U8(n) => Some(n != 0),
        Value::I32(n) => Some(n != 0),
        Value::U32(n) => Some(n != 0),
        other => {
            log::warn!(
                "notif-dbus: bool hint has unexpected type {:?}, ignoring",
                other.value_signature()
            );
            None
        }
    }
}

fn parse_string_hint(v: &OwnedValue) -> Option<String> {
    match Value::from(v.clone()) {
        Value::Str(s) => Some(s.to_string()),
        other => {
            log::warn!(
                "notif-dbus: string hint has unexpected type {:?}, ignoring",
                other.value_signature()
            );
            None
        }
    }
}

/// Unwrap a `Value::Value` (variant) wrapper, returning the inner value.
/// If not a variant, returns the value unchanged.
fn unwrap_variant(v: Value<'_>) -> Value<'_> {
    match v {
        Value::Value(inner) => unwrap_variant(*inner),
        other => other,
    }
}

fn parse_raw_image(v: &OwnedValue) -> Option<RawImage> {
    let val = unwrap_variant(Value::from(v.clone()));
    let structure = match val {
        Value::Structure(s) => s,
        other => {
            log::warn!(
                "notif-dbus: image hint has unexpected type {:?}, ignoring",
                other.value_signature()
            );
            return None;
        }
    };

    let fields = structure.into_fields();
    if fields.len() != 7 {
        log::warn!(
            "notif-dbus: image structure has {} fields, expected 7, ignoring",
            fields.len()
        );
        return None;
    }

    // Fields may be wrapped in Value::Value (variant) when built via StructureBuilder.
    let width = match fields.first().map(|f| unwrap_variant(f.clone())) {
        Some(Value::I32(n)) => n,
        other => {
            log::warn!(
                "notif-dbus: image field 0 (width) unexpected: {:?}",
                other.as_ref().map(|v| v.value_signature())
            );
            return None;
        }
    };
    let height = match fields.get(1).map(|f| unwrap_variant(f.clone())) {
        Some(Value::I32(n)) => n,
        other => {
            log::warn!(
                "notif-dbus: image field 1 (height) unexpected: {:?}",
                other.as_ref().map(|v| v.value_signature())
            );
            return None;
        }
    };
    let rowstride = match fields.get(2).map(|f| unwrap_variant(f.clone())) {
        Some(Value::I32(n)) => n,
        other => {
            log::warn!(
                "notif-dbus: image field 2 (rowstride) unexpected: {:?}",
                other.as_ref().map(|v| v.value_signature())
            );
            return None;
        }
    };
    let has_alpha = match fields.get(3).map(|f| unwrap_variant(f.clone())) {
        Some(Value::Bool(b)) => b,
        other => {
            log::warn!(
                "notif-dbus: image field 3 (has_alpha) unexpected: {:?}",
                other.as_ref().map(|v| v.value_signature())
            );
            return None;
        }
    };
    let bits_per_sample = match fields.get(4).map(|f| unwrap_variant(f.clone())) {
        Some(Value::I32(n)) => n,
        other => {
            log::warn!(
                "notif-dbus: image field 4 (bits_per_sample) unexpected: {:?}",
                other.as_ref().map(|v| v.value_signature())
            );
            return None;
        }
    };
    let channels = match fields.get(5).map(|f| unwrap_variant(f.clone())) {
        Some(Value::I32(n)) => n,
        other => {
            log::warn!(
                "notif-dbus: image field 5 (channels) unexpected: {:?}",
                other.as_ref().map(|v| v.value_signature())
            );
            return None;
        }
    };
    let data: Vec<u8> = match fields.get(6).map(|f| unwrap_variant(f.clone())) {
        Some(Value::Array(arr)) => {
            let mut bytes = Vec::new();
            for item in arr.iter() {
                match unwrap_variant(item.clone()) {
                    Value::U8(b) => bytes.push(b),
                    other => {
                        log::warn!(
                            "notif-dbus: image data array item unexpected type {:?}, ignoring image",
                            other.value_signature()
                        );
                        return None;
                    }
                }
            }
            bytes
        }
        other => {
            log::warn!(
                "notif-dbus: image field 6 (data) unexpected: {:?}",
                other.as_ref().map(|v| v.value_signature())
            );
            return None;
        }
    };

    // Validate bits_per_sample
    if bits_per_sample != 8 {
        log::warn!("notif-dbus: image bits_per_sample={bits_per_sample}, expected 8, ignoring");
        return None;
    }

    // Validate channels
    if channels != 3 && channels != 4 {
        log::warn!("notif-dbus: image channels={channels}, expected 3 or 4, ignoring");
        return None;
    }

    // Validate data length: must be >= rowstride*(height-1) + width*channels
    let min_len = if height > 0 {
        let last_row_start = rowstride.saturating_mul(height.saturating_sub(1));
        let last_row_len = width.saturating_mul(channels);
        last_row_start.saturating_add(last_row_len)
    } else {
        0
    };

    if (data.len() as i32) < min_len {
        log::warn!(
            "notif-dbus: image data too small: {} bytes, need >= {min_len}, ignoring",
            data.len()
        );
        return None;
    }

    Some(RawImage {
        width,
        height,
        rowstride,
        has_alpha,
        bits_per_sample,
        channels,
        data,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use notif_types::Urgency;
    use zvariant::{Array, StructureBuilder};

    fn make_image_owned(
        width: i32,
        height: i32,
        rowstride: i32,
        has_alpha: bool,
        bps: i32,
        channels: i32,
        data: Vec<u8>,
    ) -> OwnedValue {
        // Build the byte array as zvariant Array<u8>
        let byte_array: Array<'_> = data
            .into_iter()
            .map(Value::U8)
            .collect::<Vec<_>>()
            .into_iter()
            .fold(Array::new(&zvariant::Signature::U8), |mut arr, v| {
                arr.append(v).unwrap();
                arr
            });

        let structure = StructureBuilder::new()
            .add_field(Value::I32(width))
            .add_field(Value::I32(height))
            .add_field(Value::I32(rowstride))
            .add_field(Value::Bool(has_alpha))
            .add_field(Value::I32(bps))
            .add_field(Value::I32(channels))
            .add_field(Value::Array(byte_array))
            .build()
            .unwrap();

        OwnedValue::try_from(Value::Structure(structure)).unwrap()
    }

    fn make_urgency_u8(n: u8) -> OwnedValue {
        OwnedValue::try_from(Value::U8(n)).unwrap()
    }

    fn make_urgency_u32(n: u32) -> OwnedValue {
        OwnedValue::try_from(Value::U32(n)).unwrap()
    }

    #[test]
    fn test_urgency_byte() {
        assert_eq!(parse_urgency(&make_urgency_u8(0)), Some(Urgency::Low));
        assert_eq!(parse_urgency(&make_urgency_u8(1)), Some(Urgency::Normal));
        assert_eq!(parse_urgency(&make_urgency_u8(2)), Some(Urgency::Critical));
    }

    #[test]
    fn test_urgency_u32() {
        assert_eq!(parse_urgency(&make_urgency_u32(0)), Some(Urgency::Low));
        assert_eq!(parse_urgency(&make_urgency_u32(2)), Some(Urgency::Critical));
    }

    #[test]
    fn test_image_data_rgb() {
        // 2x2 RGB, rowstride=6
        let data = vec![0u8; 12];
        let owned = make_image_owned(2, 2, 6, false, 8, 3, data);
        let result = parse_raw_image(&owned);
        assert!(result.is_some());
        let raw = result.unwrap();
        assert_eq!(raw.width, 2);
        assert_eq!(raw.height, 2);
        assert!(!raw.has_alpha);
    }

    #[test]
    fn test_image_data_rgba() {
        // 2x2 RGBA, rowstride=8
        let data = vec![0u8; 16];
        let owned = make_image_owned(2, 2, 8, true, 8, 4, data);
        let result = parse_raw_image(&owned);
        assert!(result.is_some());
        let raw = result.unwrap();
        assert!(raw.has_alpha);
        assert_eq!(raw.channels, 4);
    }

    #[test]
    fn test_image_data_bogus_rowstride() {
        // rowstride=6, height=2, width=2, channels=3 => min_len = 6*(2-1) + 2*3 = 12
        // but we only give 5 bytes => should return None
        let data = vec![0u8; 5];
        let owned = make_image_owned(2, 2, 6, false, 8, 3, data);
        let result = parse_raw_image(&owned);
        assert!(result.is_none());
    }

    #[test]
    fn test_image_precedence() {
        // Both image-data and image-path in hints; image-data should win
        let mut hints = HashMap::new();
        let image_data = make_image_owned(2, 2, 6, false, 8, 3, vec![0u8; 12]);
        hints.insert("image-data".to_string(), image_data);
        hints.insert(
            "image-path".to_string(),
            OwnedValue::try_from(Value::Str("/some/path.png".into())).unwrap(),
        );

        let n = parse_hints(
            "app".to_string(),
            "".to_string(),
            "summary".to_string(),
            "body".to_string(),
            vec![],
            hints,
            -1,
        );

        assert!(matches!(n.image, Some(ImageSource::Data(_))));
    }

    #[test]
    fn test_odd_actions() {
        // ["default", "Default", "orphan"] => 1 action, "orphan" dropped
        let n = parse_hints(
            "app".to_string(),
            "".to_string(),
            "summary".to_string(),
            "body".to_string(),
            vec![
                "default".to_string(),
                "Default".to_string(),
                "orphan".to_string(),
            ],
            HashMap::new(),
            -1,
        );
        assert_eq!(n.actions.len(), 1);
        assert_eq!(n.actions[0].key, "default");
    }

    #[test]
    fn test_unknown_hints_preserved() {
        let mut hints = HashMap::new();
        hints.insert(
            "x-custom-hint".to_string(),
            OwnedValue::try_from(Value::Str("custom-value".into())).unwrap(),
        );

        let n = parse_hints(
            "app".to_string(),
            "".to_string(),
            "summary".to_string(),
            "body".to_string(),
            vec![],
            hints,
            -1,
        );

        assert!(n.raw_hints.contains_key("x-custom-hint"));
    }
}
