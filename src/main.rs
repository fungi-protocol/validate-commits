use clap::Parser;
use std::io;
use std::process::ExitCode;

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    /// Revisions to check: X includes X and its history, ^X excludes it,
    /// and A..B means B but not A
    #[arg(value_name = "REVISION", required = true)]
    revisions: Vec<String>,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let revisions: Vec<_> = cli.revisions.iter().map(String::as_str).collect();
    match validate_commits::run_revisions(&revisions, &mut io::stdout()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}
