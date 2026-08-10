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
    forbid_the_real_thing("config", "REVIEWQ_CONFIG");
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
    forbid_the_real_thing("ledger", "REVIEWQ_DB");
    Ok(data_dir()?.join("reviewq.db"))
}

/// Refuse the XDG fallback in a test build.
///
/// A test that reaches it would read the developer's own config, and
/// `Ledger::open` would *create* their ledger — or worse, write to the one they
/// use. No test has any business there, so falling through is a bug in the test
/// rather than something to tolerate: it fails loudly, naming the variable that
/// should have been set.
///
/// Nothing in a release build, which is why the production path above is
/// untouched by it.
#[cfg_attr(not(test), expect(unused_variables))]
fn forbid_the_real_thing(what: &str, var: &str) {
    #[cfg(test)]
    panic!(
        "a test asked for the real {what} path — set ${var} to something \
         temporary; tests must never read the developer's own {what}"
    );
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
