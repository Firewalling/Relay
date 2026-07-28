use clap::Parser;
use relay::{
    app,
    cli::{Cli, Command, generate_completions},
    model::{ErrorEnvelope, OUTPUT_SCHEMA_VERSION, OutputError, SuccessEnvelope},
    project,
};
use std::{env, process::ExitCode};

fn main() -> ExitCode {
    let cli = Cli::parse();
    if let Command::Completions(args) = &cli.command {
        generate_completions(args.shell);
        return ExitCode::SUCCESS;
    }
    if matches!(&cli.command, Command::Protocol) {
        return emit_success(cli.json, cli.command.name(), app::protocol_output());
    }
    let json_output = cli.json;
    let command_name = cli.command.name();
    let invocation_directory = match env::current_dir() {
        Ok(directory) => directory,
        Err(error) => {
            eprintln!("error: could not determine the current directory: {error}");
            return ExitCode::FAILURE;
        }
    };
    let project = match project::discover(&invocation_directory, cli.scope) {
        Ok(project) => project,
        Err(error) => {
            emit_error(
                json_output,
                command_name,
                "project_discovery",
                &error.to_string(),
            );
            return ExitCode::FAILURE;
        }
    };
    match app::run(cli.command, &project.relay_project_root) {
        Ok(output) => emit_success(json_output, command_name, output),
        Err(error) => {
            emit_error(
                json_output,
                command_name,
                error_code(&error),
                &error.to_string(),
            );
            ExitCode::FAILURE
        }
    }
}

fn emit_success(json_output: bool, command: &str, output: app::RunOutput) -> ExitCode {
    if json_output {
        println!(
            "{}",
            serde_json::to_string(&SuccessEnvelope {
                output_schema_version: OUTPUT_SCHEMA_VERSION,
                ok: true,
                command: command.into(),
                data: output.data,
            })
            .expect("serializing a Relay output envelope cannot fail")
        );
    } else {
        println!("{}", output.human);
    }
    ExitCode::SUCCESS
}

fn emit_error(json_output: bool, command: &str, code: &str, message: &str) {
    if json_output {
        eprintln!(
            "{}",
            serde_json::to_string(&ErrorEnvelope {
                output_schema_version: OUTPUT_SCHEMA_VERSION,
                ok: false,
                command: command.into(),
                error: OutputError {
                    code: code.into(),
                    message: message.into(),
                },
            })
            .expect("serializing a Relay error envelope cannot fail")
        );
    } else {
        eprintln!("error: {message}");
    }
}

fn error_code(error: &app::AppError) -> &'static str {
    match error {
        app::AppError::Lifecycle(_) => "lifecycle_error",
        app::AppError::UnsupportedSchema { .. } => "unsupported_schema",
        app::AppError::Storage(_) => "storage_error",
        app::AppError::Git(_) => "git_error",
        app::AppError::Lock(_) => "project_busy",
        app::AppError::Io { .. } => "io_error",
        app::AppError::CorruptSession { .. } => "corrupt_state",
        app::AppError::SymlinkState { .. } => "unsafe_state_path",
        app::AppError::InjectedFault(_) => "operation_interrupted",
    }
}
