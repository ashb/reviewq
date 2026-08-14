//! Who this queue belongs to, on each host it reaches.
//!
//! Not one login but one per host: the same person is `ashb` on one forge and
//! something else on another, and every reason — a mention, a review of mine, a
//! PR of mine — is computed relative to whichever name that host knows.
//!
//! Config can say, and usually does not need to. A token knows whose it is, so
//! the answer is asked of the host once and remembered for the run; `[identity]
//! login` overrides it everywhere and `[forge."host"] login` on one host, for
//! the cases where the token's answer is not the account you mean.

use std::collections::HashMap;

use anyhow::{Context, Result};
use reviewq_forge::Forge;

use crate::config::Config;

/// Logins already resolved, so a sync of six repos on one host asks once.
#[derive(Debug, Default)]
pub struct Logins(HashMap<String, String>);

impl Logins {
    /// A fresh, empty cache.
    pub fn new() -> Self {
        Self::default()
    }

    /// Who I am on `host`, asking `forge` only if config has not said and this
    /// has not already asked.
    ///
    /// The query is one point and one round trip, which is why it is cached for
    /// the run rather than per call — and not cached in the ledger, because a
    /// token can be replaced between runs and a stale login would attribute my
    /// reviews to somebody else.
    pub async fn on(&mut self, cfg: &Config, host: &str, forge: &dyn Forge) -> Result<String> {
        if let Some(login) = cfg.configured_login(host) {
            return Ok(login);
        }
        if let Some(known) = self.0.get(host) {
            return Ok(known.clone());
        }
        let viewer = forge
            .viewer()
            .await
            .with_context(|| format!("asking {host} who the token belongs to"))?;
        self.0.insert(host.to_string(), viewer.login.clone());
        Ok(viewer.login)
    }
}
