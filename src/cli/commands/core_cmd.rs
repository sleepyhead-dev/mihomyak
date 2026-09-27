//! Core binary management (`core install`/`core version`) and the health check.

use std::process::ExitCode;

use anyhow::Result;

use crate::cli::CoreCommand;
use crate::config::Config;

pub(super) fn core_cmd(config: &Config, cmd: CoreCommand) -> Result<ExitCode> {
    let store = crate::service::store::Store::open(&config.data_dir)?;
    match cmd {
        CoreCommand::Install {
            version,
            dest,
            mirror,
            sha256,
        } => {
            let dest = dest.unwrap_or_else(|| store.root().join("bin/mihomo"));
            let tag = crate::mihomo::core::install(
                version.as_deref(),
                &dest,
                mirror.as_deref(),
                sha256.as_deref(),
            )?;
            println!("installed mihomo {tag} to {}", dest.display());
        }
        CoreCommand::Version => {
            let bin = crate::mihomo::core::resolve_bin(config, &store);
            println!("{} ({})", crate::mihomo::core::version(&bin)?, bin.display());
        }
    }
    Ok(ExitCode::SUCCESS)
}

pub(super) fn health(config: &Config) -> ExitCode {
    match super::api(config).and_then(|a| a.version()) {
        Ok(_) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("unhealthy: {e:#}");
            ExitCode::FAILURE
        }
    }
}
