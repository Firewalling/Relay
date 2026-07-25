use clap::Parser;
use relay::{app, cli::Cli};
use std::{env, process::ExitCode};

fn main() -> ExitCode {
    let cli = Cli::parse();
    let project = match env::current_dir() {
        Ok(project) => project,
        Err(error) => {
            eprintln!("error: could not determine the current directory: {error}");
            return ExitCode::FAILURE;
        }
    };
    match app::run(cli.command, &project) {
        Ok(output) => {
            println!("{output}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}
