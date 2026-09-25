use std::process::ExitCode;

use clap::Parser;

fn main() -> ExitCode {
    // Everything we (and mihomo, which inherits it) create holds secrets.
    // SAFETY: umask has no preconditions.
    unsafe { libc::umask(0o077) };
    mihomyak::log::init();
    let cli = mihomyak::cli::Cli::parse();
    match mihomyak::commands::run(cli) {
        Ok(code) => code,
        Err(e) => {
            mihomyak::error!("{e:#}");
            ExitCode::FAILURE
        }
    }
}
