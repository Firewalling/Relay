use clap::{Parser, error::Error};
use relay::{
    app,
    cli::{Cli, Command, generate_completions},
    model::{ErrorEnvelope, OUTPUT_SCHEMA_VERSION, OutputError, SuccessEnvelope},
    project,
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

#[cfg(test)]
mod tests {
    use super::*;
    use relay::{git, lock, storage};
    use std::{io, path::PathBuf};

    #[test]
    fn json_is_only_requested_by_a_global_flag_before_a_trailing_separator() {
        for args in [
            vec!["relay", "--json", "status"],
            vec!["relay", "status", "--json"],
        ] {
            assert!(requests_json(&args), "{args:?}");
        }
        for args in [
            vec!["relay", "status"],
            vec!["relay", "note", "progress", "--", "--json"],
            Vec::new(),
        ] {
            assert!(!requests_json(&args), "{args:?}");
        }
    }

    #[test]
    fn every_application_error_maps_to_a_stable_machine_code() {
        let path = PathBuf::from(".relay/session.json");
        let cases = [
            (
                app::AppError::Lifecycle("no session".into()),
                "lifecycle_error",
            ),
            (
                app::AppError::UnsupportedSchema {
                    kind: "session state",
                    found: 99,
                    supported: 1,
                },
                "unsupported_schema",
            ),
            (
                app::AppError::Storage(storage::StorageError::PartialJsonLine {
                    path: path.clone(),
                    line: 2,
                }),
                "storage_error",
            ),
            (
                app::AppError::Git(git::GitError::Command {
                    directory: path.clone(),
                    message: "git failed".into(),
                }),
                "git_error",
            ),
            (
                app::AppError::Lock(lock::LockError::Busy {
                    owner: " (PID 1, command `start`)".into(),
                }),
                "project_busy",
            ),
            (
                app::AppError::Io {
                    path: path.clone(),
                    source: io::Error::from(io::ErrorKind::NotFound),
                },
                "io_error",
            ),
            (
                app::AppError::CorruptSession {
                    path: path.clone(),
                    message: "conflicting fields".into(),
                },
                "corrupt_state",
            ),
            (app::AppError::SymlinkState { path }, "unsafe_state_path"),
            (
                app::AppError::InjectedFault("session_record".into()),
                "operation_interrupted",
            ),
        ];

        for (error, expected) in &cases {
            assert_eq!(error_code(error), *expected, "{error}");
        }
        let mut codes = cases.iter().map(|(_, code)| *code).collect::<Vec<_>>();
        codes.sort_unstable();
        codes.dedup();
        assert_eq!(codes.len(), cases.len());
    }
}
