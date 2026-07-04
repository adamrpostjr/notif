#![forbid(unsafe_code)]

//! `notif-config` — configuration loading, validation, and file watching for
//! the notif daemon.
//!
//! The pure-data configuration types live in [`notif_types::config`] and are
//! re-exported here for convenience.

pub use notif_types::config::*;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use inotify::{Inotify, WatchMask};

/// Errors that can occur while loading or parsing the configuration.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// Failed to read the config file from disk.
    #[error("failed to read config file: {0}")]
    Io(#[from] std::io::Error),
    /// The file content is not valid TOML.
    #[error("failed to parse TOML: {0}")]
    Parse(#[from] toml::de::Error),
    /// A field value is out of its valid range.
    #[error("invalid configuration: {message}")]
    Validation {
        /// Human-readable description of the problem.
        message: String,
    },
}

/// Load configuration from `path`.
///
/// If the file does not exist, the built-in defaults are returned.
/// If the file exists but cannot be parsed or validated, an error is returned.
pub fn load(path: &Path) -> Result<Config, ConfigError> {
    if !path.exists() {
        return Ok(Config::default());
    }
    let text = std::fs::read_to_string(path)?;
    let config: Config = toml::from_str(&text)?;
    validate(&config)?;
    Ok(config)
}

/// Validate a [`Config`], returning a descriptive error for out-of-range values.
pub fn validate(c: &Config) -> Result<(), ConfigError> {
    if c.font_size <= 0.0 {
        return Err(ConfigError::Validation {
            message: format!("font_size must be positive, got {}", c.font_size),
        });
    }
    if c.max_visible == 0 {
        return Err(ConfigError::Validation {
            message: "max_visible must be at least 1".to_owned(),
        });
    }
    if c.max_width == 0 {
        return Err(ConfigError::Validation {
            message: "max_width must be at least 1".to_owned(),
        });
    }
    if c.max_height == 0 {
        return Err(ConfigError::Validation {
            message: "max_height must be at least 1".to_owned(),
        });
    }
    Ok(())
}

/// Watch `config_path` for changes and emit a [`notif_types::ConfigEvent`] on `tx`
/// whenever the file is updated to valid content.
///
/// Watches the parent directory for `IN_CLOSE_WRITE` and `IN_MOVED_TO` events so
/// that editor rename-swap workflows are handled correctly. Invalid reloads are
/// logged via [`log::warn`] and never sent.
///
/// The function returns when `tx` is closed or an unrecoverable inotify error occurs.
pub async fn watch(config_path: PathBuf, tx: async_channel::Sender<notif_types::ConfigEvent>) {
    if let Err(e) = watch_impl(config_path, tx).await {
        log::warn!("config watcher stopped: {e}");
    }
}

async fn watch_impl(
    config_path: PathBuf,
    tx: async_channel::Sender<notif_types::ConfigEvent>,
) -> Result<(), Box<dyn std::error::Error>> {
    use std::os::fd::AsFd;

    let parent = config_path
        .parent()
        .ok_or("config path has no parent directory")?;

    let filename = config_path
        .file_name()
        .ok_or("config path has no file name")?
        .to_owned();

    let mut inotify = Inotify::init()?;
    inotify
        .watches()
        .add(parent, WatchMask::CLOSE_WRITE | WatchMask::MOVED_TO)?;

    // Wrap the raw fd in async-io's Async for reactor integration.
    // We duplicate the fd so async-io can own it independently.
    let raw_fd = inotify.as_fd().try_clone_to_owned()?;
    let async_fd = async_io::Async::new(raw_fd)?;

    let mut buffer = vec![0u8; 4096];

    loop {
        if tx.is_closed() {
            break;
        }

        // Wait until the fd is readable.
        async_fd.readable().await?;

        // Read available events synchronously.
        let events = match inotify.read_events(&mut buffer) {
            Ok(events) => events,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
            Err(e) => {
                log::warn!("inotify read error: {e}");
                break;
            }
        };

        for event in events {
            let matches = event
                .name
                .map(|n| n == filename.as_os_str())
                .unwrap_or(false);
            if matches {
                match load(&config_path) {
                    Ok(cfg) => {
                        let ev = notif_types::ConfigEvent(Arc::new(cfg));
                        if tx.send(ev).await.is_err() {
                            return Ok(());
                        }
                    }
                    Err(e) => {
                        log::warn!("failed to reload config {:?}: {e}", config_path);
                    }
                }
            }
        }
    }

    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn test_missing_file_gives_defaults() {
        let path = PathBuf::from("/nonexistent/path/that/does/not/exist/notif.toml");
        let config = load(&path).unwrap();
        assert_eq!(config.anchor, AnchorCorner::TopRight);
        assert_eq!(config.max_visible, 5);
        assert_eq!(config.margin_x, 12);
        assert_eq!(config.margin_y, 12);
        assert_eq!(config.gap, 8);
        assert_eq!(config.max_width, 400);
        assert_eq!(config.max_height, 200);
        assert_eq!(config.history_limit, 100);
        assert!(config.body_markup);
    }

    #[test]
    fn test_example_config_parses_to_defaults() {
        let path = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/examples/config.toml"));
        let config = load(path).unwrap();
        // The sample documents the defaults, so it must round-trip to them.
        let defaults = Config::default();
        assert_eq!(config.anchor, defaults.anchor);
        assert_eq!(config.max_visible, defaults.max_visible);
        assert_eq!(config.font_family, defaults.font_family);
        assert_eq!(config.normal.border_color, defaults.normal.border_color);
        assert_eq!(
            config.critical.default_timeout_ms,
            defaults.critical.default_timeout_ms
        );
    }

    #[test]
    fn test_invalid_toml_gives_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "this is [not valid] toml !!!").unwrap();
        let result = load(&path);
        assert!(matches!(result, Err(ConfigError::Parse(_))));
    }

    #[test]
    fn test_partial_override_merges_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "max_visible = 3\n").unwrap();
        let config = load(&path).unwrap();
        assert_eq!(config.max_visible, 3);
        assert_eq!(config.anchor, AnchorCorner::TopRight);
        assert_eq!(config.margin_x, 12);
    }

    #[test]
    fn test_watcher_emits_on_rename_swap() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        std::fs::write(&config_path, "max_visible = 1\n").unwrap();

        let (tx, rx) = async_channel::bounded::<notif_types::ConfigEvent>(1);

        let watch_path = config_path.clone();
        let watch_tx = tx.clone();
        std::thread::spawn(move || {
            async_io::block_on(watch(watch_path, watch_tx));
        });

        // Give the watcher a moment to initialize
        std::thread::sleep(Duration::from_millis(100));

        // Write a new config to a temp file and rename it over the watched file
        let tmp_path = dir.path().join("config.toml.tmp");
        std::fs::write(&tmp_path, "max_visible = 7\n").unwrap();
        std::fs::rename(&tmp_path, &config_path).unwrap();

        // Wait for the event with a 2-second timeout
        let result = async_io::block_on(async {
            use futures_lite::future;
            let recv = async { rx.recv().await.ok() };
            let timeout = async {
                async_io::Timer::after(Duration::from_secs(2)).await;
                None
            };
            future::or(recv, timeout).await
        });

        let event = result.expect("expected a ConfigEvent but timed out");
        assert_eq!(event.0.max_visible, 7);
    }

    #[test]
    fn test_watcher_no_emit_on_invalid() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        std::fs::write(&config_path, "max_visible = 1\n").unwrap();

        let (tx, rx) = async_channel::bounded::<notif_types::ConfigEvent>(1);

        let watch_path = config_path.clone();
        let watch_tx = tx.clone();
        std::thread::spawn(move || {
            async_io::block_on(watch(watch_path, watch_tx));
        });

        // Give the watcher a moment to initialize
        std::thread::sleep(Duration::from_millis(100));

        // Write invalid TOML and rename it over the watched file
        let tmp_path = dir.path().join("config.toml.tmp");
        std::fs::write(&tmp_path, "this = [broken toml\n").unwrap();
        std::fs::rename(&tmp_path, &config_path).unwrap();

        // Wait 500ms and assert channel is empty
        async_io::block_on(async {
            async_io::Timer::after(Duration::from_millis(500)).await;
        });

        assert!(
            rx.try_recv().is_err(),
            "expected no ConfigEvent for invalid TOML"
        );
    }
}
