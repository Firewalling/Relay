use clap::{Parser, error::Error};
use relay::{
    app,
    cli::{Cli, Command, generate_completions},
    model::{ErrorEnvelope, OUTPUT_SCHEMA_VERSION, OutputError, SuccessEnvelope},
    orchestrate, project, supervise,
};
use std::{env, ffi::OsStr, io::Write, process::ExitCode};

fn main() -> ExitCode {
    let args_os = env::args_os().collect::<Vec<_>>();
    let json_requested = requests_json(&args_os);
    let cli = match Cli::try_parse_from(args_os) {
        Ok(cli) => cli,
        Err(error) => return handle_parser_error(error, json_requested),
    };
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
            let message = format!("could not determine the current directory: {error}");
            emit_error(json_output, command_name, "current_directory", &message);
            return ExitCode::FAILURE;
        }
    };
    if let Command::Run(args) = cli.command {
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
        return match orchestrate::run(args, &invocation_directory, &project.relay_project_root) {
            Ok(output) => emit_success(json_output, command_name, output),
            Err(error) => {
                emit_error(json_output, command_name, error.code(), &error.to_string());
                ExitCode::FAILURE
            }
        };
    }
    if let Command::Supervise(args) = cli.command {
        return match supervise::run(args, &invocation_directory) {
            Ok(output) => emit_success(json_output, command_name, output),
            Err(error) => {
                emit_error(json_output, command_name, error.code(), &error.to_string());
                ExitCode::FAILURE
            }
        };
    }
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

fn requests_json(args: &[impl AsRef<OsStr>]) -> bool {
    args.iter()
        .skip(1)
        .map(AsRef::as_ref)
        .take_while(|arg| *arg != OsStr::new("--"))
        .any(|arg| arg == OsStr::new("--json"))
}

fn handle_parser_error(error: Error, json_requested: bool) -> ExitCode {
    let exit_code = ExitCode::from(error.exit_code() as u8);
    if json_requested && error.use_stderr() {
        let rendered = error.to_string();
        let message = rendered.trim();
        let message = message.strip_prefix("error: ").unwrap_or(message).trim();
        emit_error(true, "cli", "argument_error", message);
    } else {
        let _ = error.print();
    }
    exit_code
}

fn emit_success(json_output: bool, command: &str, output: app::RunOutput) -> ExitCode {
    let stdout = std::io::stdout();
    let mut stdout = stdout.lock();
    if json_output {
        let _ = writeln!(
            stdout,
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
        let _ = writeln!(stdout, "{}", output.human);
    }
    ExitCode::SUCCESS
}

fn emit_error(json_output: bool, command: &str, code: &str, message: &str) {
    let stderr = std::io::stderr();
    let mut stderr = stderr.lock();
    if json_output {
        let _ = writeln!(
            stderr,
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
        let _ = writeln!(stderr, "error: {message}");
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
