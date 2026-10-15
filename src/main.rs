use clap::Parser;
use std::io;
use std::process::ExitCode;

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    /// Git revisions or ranges to include; prefix a revision with ^ to exclude it
    #[arg(value_name = "REVISION", required_unless_present = "all")]
    revisions: Vec<String>,
    /// Include history reachable from all Git refs and HEAD
    #[arg(long)]
    all: bool,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let revisions: Vec<_> = cli.revisions.iter().map(String::as_str).collect();
    match validate_commits::run_revisions(&revisions, cli.all, &mut io::stdout()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}
