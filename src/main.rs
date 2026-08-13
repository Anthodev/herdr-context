use std::error::Error;
use std::io::{self, IsTerminal};
use std::process::ExitCode;

use herdr_context::host::LaunchContext;
use herdr_context::runtime::run_files_terminal;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("herdr-context: {error}");
            ExitCode::from(2)
        }
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    let context = LaunchContext::from_env()?;
    if io::stdout().is_terminal() {
        run_files_terminal(context)?;
    }
    Ok(())
}
