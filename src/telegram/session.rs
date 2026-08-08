//! Session persistence, so a successful login survives across runs.

use std::path::Path;
use std::sync::Arc;

use color_eyre::eyre::{Result, WrapErr};
use grammers_session::storages::SqliteSession;

/// Open (creating if needed) the SQLite-backed session at `path`.
pub async fn open(path: &Path) -> Result<Arc<SqliteSession>> {
    let session = SqliteSession::open(path)
        .await
        .wrap_err_with(|| format!("could not open session file at {}", path.display()))?;
    // Runs on every start, not just creation, so a session written by an older build (or by a
    // careless `chmod`) gets tightened too.
    crate::config::restrict_to_owner(path)?;

    Ok(Arc::new(session))
}
