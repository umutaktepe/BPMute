use std::io::Write;

use clap::CommandFactory;
use clap_complete::{Shell, generate};

use crate::cli::args::{Cli, CompletionShell};
use crate::exit_code::ExitCode;

/// Generates shell completion scripts for `bpmute` onto `stdout`.
pub fn run_completions<WOut: Write>(shell: CompletionShell, stdout: &mut WOut) -> ExitCode {
    let mut cmd = Cli::command();
    let target_shell = match shell {
        CompletionShell::Bash => Shell::Bash,
        CompletionShell::Zsh => Shell::Zsh,
        CompletionShell::Fish => Shell::Fish,
    };
    generate(target_shell, &mut cmd, "bpmute", stdout);
    ExitCode::Success
}
