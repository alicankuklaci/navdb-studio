//! Placeholder: the driver is implemented in a follow-up change.

use anyhow::{bail, Result};

use crate::config::ConnectionConfig;

pub struct MsSqlDriver;

impl MsSqlDriver {
    pub fn connect(cfg: &ConnectionConfig) -> Result<super::Unsupported> {
        bail!("{} support is not available yet", cfg.kind.label())
    }
}
