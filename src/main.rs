use std::io;

use bpmute::cli::{Cli, execute_cli};
use bpmute::exit_code::ExitCode;
use bpmute::paths::ProcessEnv;
use clap::Parser;
use clap::error::ErrorKind;

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(err) => {
            let code = match err.kind() {
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion => ExitCode::Success,
                _ => ExitCode::UsageOrConfigError,
            };
            let _ = err.print();
            return code.into();
        }
    };

    let mut stdout = io::stdout();
    let mut stderr = io::stderr();
    execute_cli(cli, &ProcessEnv, &mut stdout, &mut stderr)
        .await
        .into()
}
