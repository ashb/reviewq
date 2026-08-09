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

/// The directory [`database_file`] sits in, `$XDG_DATA_HOME/reviewq`. Created
/// by `Ledger::open` when it first writes the ledger.
pub fn data_dir() -> Result<PathBuf> {
    let base = etcetera::choose_base_strategy().context("cannot determine home directory")?;
    Ok(base.data_dir().join("reviewq"))
}
