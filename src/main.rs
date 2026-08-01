use std::process::ExitCode;

use herdr_context::host::LaunchContext;

fn main() -> ExitCode {
    match LaunchContext::from_env() {
        Ok(_) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("herdr-context: {error}");
            ExitCode::from(2)
        }
    }
}
