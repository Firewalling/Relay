use clap::{Parser, error::Error};
use relay::{
    app,
    cli::{Cli, Command, generate_completions},
    model::{ErrorEnvelope, OUTPUT_SCHEMA_VERSION, OutputError, SuccessEnvelope},
    orchestrate, project, supervise, workflow,
};
use serde::Serialize;
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
    if let Command::Workflow(args) = cli.command {
        let inspection = matches!(&args.command, relay::cli::WorkflowCommand::Explain(_))
            || matches!(&args.command, relay::cli::WorkflowCommand::Run(run) if run.dry_run);
        let discovery = if inspection {
            project::discover_inspection(&invocation_directory, cli.scope, true)
        } else {
            project::discover_workflow(&invocation_directory, cli.scope)
        };
        let project = match discovery {
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
        return match workflow::run(args, &invocation_directory, &project.relay_project_root) {
            Ok(workflow::WorkflowResult::Inspection(output)) => {
                emit_success(json_output, command_name, output)
            }
            Ok(workflow::WorkflowResult::Run(result)) => {
                let exit_code = if result.succeeded() {
                    ExitCode::SUCCESS
                } else {
                    ExitCode::FAILURE
                };
                emit_success(json_output, command_name, result.output);
                exit_code
            }
            Err(workflow::WorkflowError::Run(error)) => {
                emit_run_error(json_output, command_name, &error);
                ExitCode::FAILURE
            }
            Err(error) => {
                emit_error(
                    json_output,
                    command_name,
                    "workflow_invalid",
                    &error.to_string(),
                );
                ExitCode::FAILURE
            }
        };
    }
    if let Command::Run(args) = cli.command {
        let discovery = if args.command.is_some() {
            project::discover_inspection(&invocation_directory, cli.scope, false)
        } else {
            project::discover(&invocation_directory, cli.scope)
        };
        let project = match discovery {
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
        if let Some(relay::cli::RunCommand::Explain { state_or_run }) = &args.command {
            return match orchestrate::explain(state_or_run, &project.relay_project_root) {
                Ok(output) => emit_success(json_output, command_name, output),
                Err(error) => {
                    emit_run_error(json_output, command_name, &error);
                    ExitCode::FAILURE
                }
            };
        }
        return match orchestrate::run(args, &invocation_directory, &project.relay_project_root) {
            Ok(result) => {
                let exit_code = if result.succeeded() {
                    ExitCode::SUCCESS
                } else {
                    ExitCode::FAILURE
                };
                emit_success(json_output, command_name, result.output);
                exit_code
            }
            Err(error) => {
                emit_run_error(json_output, command_name, &error);
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

#[derive(Serialize)]
struct RunErrorEnvelope<'a> {
    output_schema_version: u32,
    ok: bool,
    command: String,
    error: OutputError,
    data: RunErrorData<'a>,
}

#[derive(Serialize)]
struct RunErrorData<'a> {
    state_path: &'a str,
    run: &'a orchestrate::RunState,
    reconciliation: &'a [orchestrate::ReconciliationReport],
}

fn emit_run_error(json_output: bool, command: &str, error: &orchestrate::RunError) {
    let message = error.to_string();
    let Some(state) = error.state() else {
        emit_error(json_output, command, error.code(), &message);
        return;
    };
    if !json_output {
        emit_error(false, command, error.code(), &message);
        return;
    }

    let stderr = std::io::stderr();
    let mut stderr = stderr.lock();
    let _ = writeln!(
        stderr,
        "{}",
        serde_json::to_string(&RunErrorEnvelope {
            output_schema_version: OUTPUT_SCHEMA_VERSION,
            ok: false,
            command: command.into(),
            error: OutputError {
                code: error.code().into(),
                message,
            },
            data: RunErrorData {
                state_path: &state.state_path,
                run: state,
                reconciliation: error.reconciliation(),
            },
        })
        .expect("serializing a Relay run error envelope cannot fail")
    );
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
        let mut safe = String::with_capacity(message.len());
        for ch in message.chars() {
            if ch.is_control() || matches!(ch, '\u{2028}'..='\u{202e}' | '\u{2066}'..='\u{2069}') {
                safe.extend(ch.escape_default());
            } else {
                safe.push(ch);
            }
        }
        let _ = writeln!(stderr, "error: {safe}");
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
