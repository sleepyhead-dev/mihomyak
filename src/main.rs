use std::process::ExitCode;

use clap::Parser;

fn main() -> ExitCode {
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
