//! Resolution of on-disk locations. XDG on every platform, so that
//! `$XDG_CONFIG_HOME` / `$XDG_DATA_HOME` overrides work on macOS too.

use std::path::PathBuf;

use anyhow::{Context, Result};
use etcetera::BaseStrategy as _;

/// Config file path, honouring `REVIEWQ_CONFIG` and then `$XDG_CONFIG_HOME`.
pub fn config_file() -> Result<PathBuf> {
    if let Some(explicit) = std::env::var_os("REVIEWQ_CONFIG") {
        return Ok(PathBuf::from(explicit));
    }
    Ok(config_dir()?.join("config.toml"))
}

/// The directory [`config_file`] sits in, `$XDG_CONFIG_HOME/reviewq`. Reported
/// by `doctor` and created on first run.
pub fn config_dir() -> Result<PathBuf> {
    let base = etcetera::choose_base_strategy().context("cannot determine home directory")?;
    Ok(base.config_dir().join("reviewq"))
}

/// Ledger path, honouring `REVIEWQ_DB` and then `$XDG_DATA_HOME`.
pub fn database_file() -> Result<PathBuf> {
    if let Some(explicit) = std::env::var_os("REVIEWQ_DB") {
        return Ok(PathBuf::from(explicit));
    }
    Ok(data_dir()?.join("reviewq.db"))
}

/// Expand a leading `~` to the home directory, leaving every other path alone.
///
/// A path in a config file is typed by a person, and a person writes `~/code/foo`.
/// Nothing else expands it — a shell would have, but config is read straight off
/// disk — so a literal `~` directory would be looked for and not found.
///
/// Only a leading `~` or `~/`, and only the current user's home: `~someone/x` is
/// a shell feature that needs the password database, and guessing at it would be
/// worse than leaving it as typed.
pub fn expand_tilde(path: &std::path::Path) -> Result<PathBuf> {
    let Ok(rest) = path.strip_prefix("~") else {
        return Ok(path.to_path_buf());
    };
    let base = etcetera::choose_base_strategy().context("cannot determine home directory")?;
    Ok(base.home_dir().join(rest))
}

/// The directory [`database_file`] sits in, `$XDG_DATA_HOME/reviewq`. Created
/// by `Ledger::open` when it first writes the ledger.
pub fn data_dir() -> Result<PathBuf> {
    let base = etcetera::choose_base_strategy().context("cannot determine home directory")?;
    Ok(base.data_dir().join("reviewq"))
}
