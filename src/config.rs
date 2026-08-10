//! Runtime configuration: API credentials and on-disk locations.

use std::path::{Path, PathBuf};

use color_eyre::eyre::{Result, WrapErr, eyre};
use directories::ProjectDirs;

/// Publicly known Telegram Desktop API credentials.
///
/// Telegram does not hand these out per-application for third-party clients, and these values
/// ship in Telegram Desktop's open source, so they are widely reused by FOSS clients. Override
/// them with `TG_API_ID` / `TG_API_HASH` if you have your own from <https://my.telegram.org>.
const DEFAULT_API_ID: i32 = 17349;
const DEFAULT_API_HASH: &str = "344583e45741c457fe1862106095a5eb";

pub struct Config {
    pub api_id: i32,
    pub api_hash: String,
    pub session_path: PathBuf,
    pub log_dir: PathBuf,
    pub images: ImageMode,
    /// `TGTUI_IMAGE_ROWS`: an absolute cap on how tall an inline picture may be. Unset means the
    /// transcript decides, as a fraction of its own height.
    pub image_rows: Option<u16>,
}

/// How hard to try to draw pictures in the transcript.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ImageMode {
    /// Ask the terminal what it supports, and fall back to half-blocks if it says nothing.
    #[default]
    Auto,
    /// Never download or draw; media stays labelled, as it was before images existed.
    Off,
    /// Skip the capability query and use coloured half-blocks. For terminals where detection
    /// misfires, or over a link where the query round-trip is unreliable.
    Halfblocks,
}

impl ImageMode {
    fn from_env() -> Result<Self> {
        match std::env::var("TGTUI_IMAGES") {
            Err(_) => Ok(Self::Auto),
            Ok(raw) => match raw.trim().to_ascii_lowercase().as_str() {
                "" | "auto" => Ok(Self::Auto),
                "off" | "none" => Ok(Self::Off),
                "halfblocks" => Ok(Self::Halfblocks),
                _ => Err(eyre!(
                    "TGTUI_IMAGES must be auto, off or halfblocks, got {raw:?}"
                )),
            },
        }
    }
}

impl Config {
    pub fn load() -> Result<Self> {
        let api_id = match std::env::var("TG_API_ID") {
            Ok(raw) => raw
                .trim()
                .parse()
                .map_err(|_| eyre!("TG_API_ID must be an integer, got {raw:?}"))?,
            Err(_) => DEFAULT_API_ID,
        };
        let api_hash =
            std::env::var("TG_API_HASH").unwrap_or_else(|_| DEFAULT_API_HASH.to_string());

        let dirs = ProjectDirs::from("", "", "tgtui")
            .ok_or_else(|| eyre!("could not determine a home directory for the session file"))?;
        let data_dir = dirs.data_dir().to_path_buf();
        // `SqliteSession::open` creates the file but not the directories leading to it.
        std::fs::create_dir_all(&data_dir)?;
        // Covers the logs and the sidecar files SQLite writes next to the database, which
        // would otherwise be created world-readable regardless of the database's own mode.
        restrict_to_owner(&data_dir)?;

        Ok(Self {
            api_id,
            api_hash,
            session_path: data_dir.join("tgtui.session"),
            log_dir: data_dir,
            images: ImageMode::from_env()?,
            image_rows: image_rows_from_env()?,
        })
    }
}

fn image_rows_from_env() -> Result<Option<u16>> {
    match std::env::var("TGTUI_IMAGE_ROWS") {
        Err(_) => Ok(None),
        Ok(raw) => match raw.trim().parse::<u16>() {
            // Zero would leave no room to draw in, which reads as "off" but silently isn't.
            Ok(rows) if rows > 0 => Ok(Some(rows)),
            _ => Err(eyre!(
                "TGTUI_IMAGE_ROWS must be a positive number of rows, got {raw:?}"
            )),
        },
    }
}

/// Lock a path down to its owner: `0700` for directories, `0600` for files.
///
/// The session database holds the account's authorization key, and that key is a bearer
/// credential — anyone who can read it can act as the logged-in user without ever seeing the
/// phone number, the login code, or the 2FA password.
pub fn restrict_to_owner(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let mode = if path.is_dir() { 0o700 } else { 0o600 };
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
            .wrap_err_with(|| format!("could not restrict permissions on {}", path.display()))?;
    }
    // Windows has no mode bits; the user profile directory is already ACL-scoped to the user.
    #[cfg(not(unix))]
    let _ = path;

    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    fn mode_of(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn a_session_file_is_readable_only_by_its_owner() {
        let dir = std::env::temp_dir().join(format!("tgtui-perm-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("tgtui.session");
        std::fs::write(&file, b"auth key").unwrap();
        // Start from the permissive mode a default umask would produce.
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();

        restrict_to_owner(&file).unwrap();
        restrict_to_owner(&dir).unwrap();

        assert_eq!(
            mode_of(&file),
            0o600,
            "the auth key must not be group or world readable"
        );
        assert_eq!(mode_of(&dir), 0o700);

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
