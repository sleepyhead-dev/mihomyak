use std::process::ExitCode;

use clap::Parser;

fn main() -> ExitCode {
    // Everything we (and mihomo, which inherits it) create holds secrets.
    // SAFETY: umask has no preconditions.
    unsafe { libc::umask(0o077) };
    mihomyak::log::init();
    let cli = mihomyak::cli::Cli::parse();
    if !matches!(cli.command, mihomyak::cli::Command::Run) {
        // One-shot commands behave like other CLI tools in a pipeline
        // (`mihomyak fetch --body | head`): exit quietly on a closed stdout
        // instead of panicking. The supervisor keeps SIGPIPE ignored.
        // SAFETY: restoring the default disposition before any threads exist.
        unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
    }
    match mihomyak::commands::run(cli) {
        Ok(code) => code,
        Err(e) => {
            mihomyak::error!("{e:#}");
            ExitCode::FAILURE
        }
    }
}
