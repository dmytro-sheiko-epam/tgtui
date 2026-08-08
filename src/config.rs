//! Runtime configuration: API credentials and on-disk locations.

use std::path::PathBuf;

use color_eyre::eyre::{Result, eyre};
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

        Ok(Self {
            api_id,
            api_hash,
            session_path: data_dir.join("tgtui.session"),
            log_dir: data_dir,
        })
    }
}
