use crate::{
    app::RunOutput,
    cli::{WorkflowArgs, WorkflowCommand, WorkflowNameArgs, WorkflowRunArgs, WorkflowValidateArgs},
    orchestrate::{self, RunError, WorkflowRunMetadata},
};
use serde::de::{self, DeserializeSeed, Deserializer, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::Digest;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::{
    collections::{BTreeMap, HashSet},
    fmt,
    fs::{self, File, OpenOptions},
    io::{self, Read},
    path::{Path, PathBuf},
};
use thiserror::Error;

pub const WORKFLOW_DIRECTORY: &str = "relay-workflows";
const WORKFLOW_SCHEMA_VERSION: u32 = 1;
const MAX_WORKFLOW_BYTES: u64 = 1 << 20;
const MAX_WORKFLOWS: usize = 64;
const MAX_WORKFLOW_NAME_BYTES: usize = 48;
const MAX_WORKFLOW_INPUTS: usize = 16;
const MAX_WORKFLOW_INPUT_NAME_BYTES: usize = 48;
const MAX_WORKFLOW_INPUT_STRING_BYTES: usize = 4 * 1024;
const DEFAULT_WORKFLOW_TIMEOUT_MS: i64 = 60_000;

#[derive(Debug, Error)]
pub enum WorkflowError {
    #[error("invalid workflow: {0}")]
    Definition(String),
    #[error(transparent)]
    Run(#[from] RunError),
}

pub enum WorkflowResult {
    Inspection(RunOutput),
    Run(orchestrate::RunResult),
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct WorkflowDefinition {
    schema_version: u32,
    name: String,
    #[serde(default)]
    inputs: BTreeMap<String, InputDefinition>,
    tasks: Vec<WorkflowTask>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct InputDefinition {
    #[serde(rename = "type")]
    input_type: InputType,
    #[serde(default)]
    required: bool,
    #[serde(default)]
    default: Option<Value>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
enum InputType {
    String,
    Integer,
    Boolean,
}

impl InputType {
    fn as_str(self) -> &'static str {
        match self {
            Self::String => "string",
            Self::Integer => "integer",
            Self::Boolean => "boolean",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct WorkflowTask {
    id: String,
    #[serde(default)]
    depends_on: Vec<String>,
    program: String,
    #[serde(default)]
    arguments: Vec<WorkflowArgument>,
    #[serde(default)]
    working_directory: Option<String>,
    #[serde(default = "default_workflow_timeout")]
    timeout_ms: i64,
    #[serde(default = "orchestrate::default_grace_ms")]
    termination_grace_ms: i64,
    #[serde(default = "orchestrate::default_output_limit")]
    stdout_limit_bytes: i64,
    #[serde(default = "orchestrate::default_output_limit")]
    stderr_limit_bytes: i64,
    #[serde(default = "orchestrate::default_log_limit")]
    log_limit_bytes: i64,
    #[serde(default)]
    redact_argument_indexes: Vec<usize>,
    #[serde(
        default,
        deserialize_with = "deserialize_retry_policy",
        skip_serializing_if = "Option::is_none"
    )]
    retry: Option<RetryPolicy>,
    #[serde(default)]
    outputs: Vec<OutputDeclaration>,
    #[serde(default)]
    inputs: Vec<OutputInput>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(untagged)]
enum WorkflowArgument {
    Literal(String),
    Input(WorkflowInputReference),
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct WorkflowInputReference {
    workflow_input: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RetryPolicy {
    max_attempts: u32,
    delay_ms: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct OutputDeclaration {
    name: String,
    #[serde(rename = "type")]
    output_type: OutputType,
    path: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum OutputType {
    Json,
    File,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct OutputInput {
    from_task: String,
    output: String,
    #[serde(rename = "as")]
    argument: String,
}

#[derive(Debug)]
struct LoadedWorkflow {
    definition: WorkflowDefinition,
    definition_sha256: String,
}

pub fn run(
    command: WorkflowArgs,
    invocation_directory: &Path,
    project_root: &Path,
) -> Result<WorkflowResult, WorkflowError> {
    match command.command {
        WorkflowCommand::List => list(project_root),
        WorkflowCommand::Show(args) => show(project_root, args),
        WorkflowCommand::Validate(args) => validate(project_root, invocation_directory, args),
        WorkflowCommand::Run(args) => run_workflow(project_root, invocation_directory, args),
    }
}

fn list(project_root: &Path) -> Result<WorkflowResult, WorkflowError> {
    let workflows = load_all(project_root)?;
    let names: Vec<&str> = workflows
        .iter()
        .map(|workflow| workflow.definition.name.as_str())
        .collect();
    let human = if names.is_empty() {
        "No Relay workflows defined".to_owned()
    } else {
        format!(
            "Workflows:\n{}",
            names
                .iter()
                .map(|name| format!("  {name}"))
                .collect::<Vec<_>>()
                .join("\n")
        )
    };
    Ok(WorkflowResult::Inspection(RunOutput {
        human,
        data: json!({"workflows": names}),
    }))
}

fn show(project_root: &Path, args: WorkflowNameArgs) -> Result<WorkflowResult, WorkflowError> {
    let workflow = find_workflow(project_root, &args.name)?;
    let definition = &workflow.definition;
    let mut human = format!("Workflow: {}\nInputs:", definition.name);
    if definition.inputs.is_empty() {
        human.push_str(" none");
    } else {
        for (name, input) in &definition.inputs {
            human.push_str(&format!(
                "\n  {name}: {}{}",
                input.input_type.as_str(),
                input_default_description(input)
            ));
        }
    }
    human.push_str("\nTasks:");
    for task in &definition.tasks {
        human.push_str(&format!("\n  {}", task.id));
        if !task.depends_on.is_empty() {
            human.push_str(&format!(" <- {}", task.depends_on.join(", ")));
        }
        if !task.outputs.is_empty() {
            human.push_str("\n    outputs:");
            for output in &task.outputs {
                human.push_str(&format!(
                    "\n      {} ({})",
                    output.name,
                    output.output_type.as_str()
                ));
            }
        }
        if !task.inputs.is_empty() {
            human.push_str("\n    consumes:");
            for input in &task.inputs {
                human.push_str(&format!(
                    "\n      {}.{} -> {}",
                    input.from_task, input.output, input.argument
                ));
            }
        }
    }
    let data = serde_json::to_value(definition).map_err(|error| {
        WorkflowError::Definition(format!("workflow cannot be encoded: {error}"))
    })?;
    Ok(WorkflowResult::Inspection(RunOutput {
        human,
        data: json!({
            "workflow": data,
            "definition_sha256": workflow.definition_sha256.clone(),
        }),
    }))
}

fn validate(
    project_root: &Path,
    invocation_directory: &Path,
    args: WorkflowValidateArgs,
) -> Result<WorkflowResult, WorkflowError> {
    let workflow = find_workflow(project_root, &args.name)?;
    let inputs = resolve_inputs(&workflow.definition, &args.inputs)?;
    let plan = compile_plan(&workflow.definition, &inputs, invocation_directory)?;
    let metadata = WorkflowRunMetadata {
        name: workflow.definition.name.clone(),
        schema_version: workflow.definition.schema_version,
        definition_sha256: workflow.definition_sha256.clone(),
        inputs: inputs.clone(),
    };
    let human = format!(
        "Workflow {} is valid\nTasks: {}\nDefinition: {}",
        workflow.definition.name,
        workflow.definition.tasks.len(),
        short_hash(&workflow.definition_sha256)
    );
    Ok(WorkflowResult::Inspection(RunOutput {
        human,
        data: json!({
            "workflow": workflow.definition,
            "definition_sha256": workflow.definition_sha256.clone(),
            "inputs": metadata.inputs,
            "plan": plan,
        }),
    }))
}

fn run_workflow(
    project_root: &Path,
    invocation_directory: &Path,
    args: WorkflowRunArgs,
) -> Result<WorkflowResult, WorkflowError> {
    validate_workflow_name(&args.name)?;
    let run_args = args.clone().into_run_args(project_root);
    if args.resume.is_some() {
        if !args.inputs.is_empty() {
            return Err(WorkflowError::Definition(
                "--input cannot be used with --resume; persisted workflow inputs are authoritative"
                    .into(),
            ));
        }
        let current_definition_sha256 = current_definition_hash(project_root, &args.name);
        return Ok(WorkflowResult::Run(orchestrate::run_workflow_resume(
            run_args,
            invocation_directory,
            project_root,
            args.name,
            current_definition_sha256,
        )?));
    }

    let workflow = find_workflow(project_root, &args.name)?;
    let inputs = resolve_inputs(&workflow.definition, &args.inputs)?;
    let plan = compile_plan(&workflow.definition, &inputs, invocation_directory)?;
    let metadata = WorkflowRunMetadata {
        name: workflow.definition.name,
        schema_version: workflow.definition.schema_version,
        definition_sha256: workflow.definition_sha256,
        inputs,
    };
    Ok(WorkflowResult::Run(orchestrate::run_resolved(
        run_args,
        invocation_directory,
        project_root,
        plan,
        metadata,
    )?))
}

impl WorkflowRunArgs {
    fn into_run_args(self, project_root: &Path) -> crate::cli::RunArgs {
        crate::cli::RunArgs {
            supervisor: self.supervisor,
            jobs: self.jobs,
            state_path: self.state_path,
            resume: self.resume,
            plan: project_root
                .join(WORKFLOW_DIRECTORY)
                .join(format!("{}.json", self.name)),
        }
    }
}
fn load_all(project_root: &Path) -> Result<Vec<LoadedWorkflow>, WorkflowError> {
    let paths = discover_paths(project_root)?;
    let mut names = HashSet::with_capacity(paths.len());
    let mut workflows = Vec::with_capacity(paths.len());
    for path in paths {
        let workflow = load_definition(&path, project_root)?;
        if !names.insert(workflow.definition.name.clone()) {
            return Err(WorkflowError::Definition(format!(
                "duplicate workflow name {:?}",
                workflow.definition.name
            )));
        }
        workflows.push(workflow);
    }
    workflows.sort_by(|left, right| left.definition.name.cmp(&right.definition.name));
    Ok(workflows)
}

fn find_workflow(project_root: &Path, name: &str) -> Result<LoadedWorkflow, WorkflowError> {
    validate_workflow_name(name)?;
    load_all(project_root)?
        .into_iter()
        .find(|workflow| workflow.definition.name == name)
        .ok_or_else(|| {
            WorkflowError::Definition(format!(
                "workflow {:?} not found in {}",
                name,
                project_root.join(WORKFLOW_DIRECTORY).display()
            ))
        })
}

fn current_definition_hash(project_root: &Path, name: &str) -> Option<String> {
    load_all(project_root)
        .ok()?
        .into_iter()
        .find(|workflow| workflow.definition.name == name)
        .map(|workflow| workflow.definition_sha256)
}

fn discover_paths(project_root: &Path) -> Result<Vec<PathBuf>, WorkflowError> {
    let directory = project_root.join(WORKFLOW_DIRECTORY);
    let metadata = match fs::symlink_metadata(&directory) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(workflow_io(&directory, error)),
    };
    if metadata.file_type().is_symlink() {
        return Err(WorkflowError::Definition(format!(
            "workflow directory is symlinked: {}",
            directory.display()
        )));
    }
    if !metadata.is_dir() {
        return Err(WorkflowError::Definition(format!(
            "workflow path is not a directory: {}",
            directory.display()
        )));
    }

    let mut entries = Vec::new();
    for entry in fs::read_dir(&directory).map_err(|error| workflow_io(&directory, error))? {
        let entry = entry.map_err(|error| workflow_io(&directory, error))?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path).map_err(|error| workflow_io(&path, error))?;
        if metadata.file_type().is_symlink() {
            return Err(WorkflowError::Definition(format!(
                "workflow entry is symlinked: {}",
                path.display()
            )));
        }
        if !metadata.is_file() {
            continue;
        }
        if path.extension().and_then(|extension| extension.to_str()) == Some("json") {
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or_else(|| {
                    WorkflowError::Definition(format!(
                        "workflow filename is not valid UTF-8: {}",
                        path.display()
                    ))
                })?;
            entries.push((name.to_owned(), path));
        }
    }
    entries.sort_by(|left, right| left.0.cmp(&right.0));
    if entries.len() > MAX_WORKFLOWS {
        return Err(WorkflowError::Definition(format!(
            "workflow directory contains more than {MAX_WORKFLOWS} definitions"
        )));
    }
    Ok(entries.into_iter().map(|(_, path)| path).collect())
}

fn load_definition(path: &Path, project_root: &Path) -> Result<LoadedWorkflow, WorkflowError> {
    let metadata = fs::symlink_metadata(path).map_err(|error| workflow_io(path, error))?;
    if metadata.file_type().is_symlink() {
        return Err(WorkflowError::Definition(format!(
            "workflow definition is symlinked: {}",
            path.display()
        )));
    }
    if !metadata.is_file() {
        return Err(WorkflowError::Definition(format!(
            "workflow definition is not a regular file: {}",
            path.display()
        )));
    }
    let bytes = read_bounded(path, MAX_WORKFLOW_BYTES)?;
    reject_duplicate_json_keys(&bytes)?;
    let definition: WorkflowDefinition = serde_json::from_slice(&bytes).map_err(|error| {
        WorkflowError::Definition(format!("could not decode {}: {error}", path.display()))
    })?;
    validate_definition(&definition, path)?;
    let placeholders = placeholder_inputs(&definition);
    compile_plan(&definition, &placeholders, project_root)?;
    let canonical = serde_json::to_vec(&definition).map_err(|error| {
        WorkflowError::Definition(format!("workflow cannot be canonicalized: {error}"))
    })?;
    let definition_sha256 = orchestrate::hex_digest(sha2::Sha256::digest(&canonical));
    Ok(LoadedWorkflow {
        definition,
        definition_sha256,
    })
}

fn validate_definition(definition: &WorkflowDefinition, path: &Path) -> Result<(), WorkflowError> {
    if definition.schema_version != WORKFLOW_SCHEMA_VERSION {
        return Err(WorkflowError::Definition(format!(
            "unsupported workflow schema_version {} in {}",
            definition.schema_version,
            path.display()
        )));
    }
    validate_workflow_name(&definition.name)?;
    if !(1..=orchestrate::MAX_TASKS).contains(&definition.tasks.len()) {
        return Err(WorkflowError::Definition(format!(
            "tasks must contain between 1 and {} entries",
            orchestrate::MAX_TASKS
        )));
    }
    if definition.inputs.len() > MAX_WORKFLOW_INPUTS {
        return Err(WorkflowError::Definition(format!(
            "workflow declares more than {MAX_WORKFLOW_INPUTS} inputs"
        )));
    }
    for (name, input) in &definition.inputs {
        validate_input_name(name)?;
        if input.required && input.default.is_some() {
            return Err(WorkflowError::Definition(format!(
                "required input {:?} cannot have a default",
                name
            )));
        }
        if let Some(default) = &input.default {
            validate_input_value(name, input.input_type, default)?;
        }
    }
    for task in &definition.tasks {
        for argument in &task.arguments {
            if let WorkflowArgument::Input(reference) = argument {
                validate_input_name(&reference.workflow_input)?;
                if !definition.inputs.contains_key(&reference.workflow_input) {
                    return Err(WorkflowError::Definition(format!(
                        "task {:?} references unknown workflow input {:?}",
                        task.id, reference.workflow_input
                    )));
                }
            }
        }
    }
    Ok(())
}

fn resolve_inputs(
    definition: &WorkflowDefinition,
    raw_values: &[String],
) -> Result<BTreeMap<String, Value>, WorkflowError> {
    let mut provided = BTreeMap::new();
    for raw in raw_values {
        let (name, value) = raw.split_once('=').ok_or_else(|| {
            WorkflowError::Definition(format!("workflow input {:?} must use NAME=VALUE", raw))
        })?;
        validate_input_name(name)?;
        if !definition.inputs.contains_key(name) {
            return Err(WorkflowError::Definition(format!(
                "unknown workflow input {:?}",
                name
            )));
        }
        if provided.contains_key(name) {
            return Err(WorkflowError::Definition(format!(
                "workflow input {:?} was provided more than once",
                name
            )));
        }
        let definition_input = &definition.inputs[name];
        provided.insert(
            name.to_owned(),
            parse_input_value(name, definition_input.input_type, value)?,
        );
    }

    for (name, input) in &definition.inputs {
        if provided.contains_key(name) {
            continue;
        }
        if let Some(default) = &input.default {
            provided.insert(name.clone(), default.clone());
        } else if input.required {
            return Err(WorkflowError::Definition(format!(
                "required workflow input {:?} is missing",
                name
            )));
        }
    }
    Ok(provided)
}

fn parse_input_value(
    name: &str,
    input_type: InputType,
    value: &str,
) -> Result<Value, WorkflowError> {
    if value.len() > MAX_WORKFLOW_INPUT_STRING_BYTES {
        return Err(WorkflowError::Definition(format!(
            "workflow input {:?} exceeds {MAX_WORKFLOW_INPUT_STRING_BYTES} bytes",
            name
        )));
    }
    match input_type {
        InputType::String => Ok(Value::String(value.to_owned())),
        InputType::Integer => value.parse::<i64>().map(|value| json!(value)).map_err(|_| {
            WorkflowError::Definition(format!("workflow input {:?} must be an integer", name))
        }),
        InputType::Boolean => match value {
            "true" => Ok(Value::Bool(true)),
            "false" => Ok(Value::Bool(false)),
            _ => Err(WorkflowError::Definition(format!(
                "workflow input {:?} must be true or false",
                name
            ))),
        },
    }
}

fn validate_input_value(
    name: &str,
    input_type: InputType,
    value: &Value,
) -> Result<(), WorkflowError> {
    match input_type {
        InputType::String => {
            let Some(value) = value.as_str() else {
                return Err(WorkflowError::Definition(format!(
                    "default for workflow input {:?} must be a string",
                    name
                )));
            };
            if value.len() > MAX_WORKFLOW_INPUT_STRING_BYTES {
                return Err(WorkflowError::Definition(format!(
                    "default for workflow input {:?} exceeds {MAX_WORKFLOW_INPUT_STRING_BYTES} bytes",
                    name
                )));
            }
        }
        InputType::Integer => {
            if value.as_i64().is_none() {
                return Err(WorkflowError::Definition(format!(
                    "default for workflow input {:?} must be an integer",
                    name
                )));
            }
        }
        InputType::Boolean => {
            if value.as_bool().is_none() {
                return Err(WorkflowError::Definition(format!(
                    "default for workflow input {:?} must be a boolean",
                    name
                )));
            }
        }
    }
    Ok(())
}

fn compile_plan(
    definition: &WorkflowDefinition,
    inputs: &BTreeMap<String, Value>,
    invocation_directory: &Path,
) -> Result<Value, WorkflowError> {
    let tasks = definition
        .tasks
        .iter()
        .map(|task| {
            let arguments = task
                .arguments
                .iter()
                .map(|argument| match argument {
                    WorkflowArgument::Literal(value) => Ok(Value::String(value.clone())),
                    WorkflowArgument::Input(reference) => inputs
                        .get(&reference.workflow_input)
                        .map(input_value_as_argument)
                        .ok_or_else(|| {
                            WorkflowError::Definition(format!(
                                "workflow input {:?} has no value",
                                reference.workflow_input
                            ))
                        }),
                })
                .collect::<Result<Vec<_>, _>>()?;
            let mut value = Map::new();
            value.insert("id".into(), json!(task.id));
            value.insert("depends_on".into(), json!(task.depends_on));
            value.insert("program".into(), json!(task.program));
            value.insert("arguments".into(), Value::Array(arguments));
            value.insert("working_directory".into(), json!(task.working_directory));
            value.insert("timeout_ms".into(), json!(task.timeout_ms));
            value.insert(
                "termination_grace_ms".into(),
                json!(task.termination_grace_ms),
            );
            value.insert("stdout_limit_bytes".into(), json!(task.stdout_limit_bytes));
            value.insert("stderr_limit_bytes".into(), json!(task.stderr_limit_bytes));
            value.insert("log_limit_bytes".into(), json!(task.log_limit_bytes));
            value.insert(
                "redact_argument_indexes".into(),
                json!(task.redact_argument_indexes),
            );
            value.insert(
                "outputs".into(),
                serde_json::to_value(&task.outputs).map_err(|error| {
                    WorkflowError::Definition(format!(
                        "workflow outputs cannot be encoded: {error}"
                    ))
                })?,
            );
            value.insert(
                "inputs".into(),
                serde_json::to_value(&task.inputs).map_err(|error| {
                    WorkflowError::Definition(format!("workflow inputs cannot be encoded: {error}"))
                })?,
            );
            if let Some(retry) = &task.retry {
                value.insert(
                    "retry".into(),
                    serde_json::to_value(retry).map_err(|error| {
                        WorkflowError::Definition(format!(
                            "workflow retry cannot be encoded: {error}"
                        ))
                    })?,
                );
            }
            Ok(Value::Object(value))
        })
        .collect::<Result<Vec<_>, WorkflowError>>()?;
    let plan = json!({
        "schema_version": orchestrate::PLAN_SCHEMA_VERSION,
        "tasks": tasks,
    });
    let encoded = serde_json::to_vec(&plan).map_err(|error| {
        WorkflowError::Definition(format!("resolved workflow plan cannot be encoded: {error}"))
    })?;
    if encoded.len() as u64 > orchestrate::MAX_PLAN_BYTES {
        return Err(WorkflowError::Definition(format!(
            "resolved workflow plan exceeds {} bytes",
            orchestrate::MAX_PLAN_BYTES
        )));
    }
    orchestrate::validate_plan_value(plan.clone(), invocation_directory)
        .map_err(|error| WorkflowError::Definition(error.to_string()))?;
    Ok(plan)
}

fn input_value_as_argument(value: &Value) -> Value {
    match value {
        Value::String(value) => Value::String(value.clone()),
        Value::Bool(value) => Value::String(value.to_string()),
        Value::Number(value) => Value::String(value.to_string()),
        Value::Null | Value::Array(_) | Value::Object(_) => Value::String(String::new()),
    }
}

fn placeholder_inputs(definition: &WorkflowDefinition) -> BTreeMap<String, Value> {
    definition
        .inputs
        .iter()
        .map(|(name, input)| {
            let value = input
                .default
                .clone()
                .unwrap_or_else(|| match input.input_type {
                    InputType::String => Value::String(String::new()),
                    InputType::Integer => json!(0),
                    InputType::Boolean => Value::Bool(false),
                });
            (name.clone(), value)
        })
        .collect()
}

fn validate_workflow_name(name: &str) -> Result<(), WorkflowError> {
    let bytes = name.as_bytes();
    if bytes.is_empty()
        || bytes.len() > MAX_WORKFLOW_NAME_BYTES
        || !bytes[0].is_ascii_alphanumeric()
        || !bytes
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err(WorkflowError::Definition(format!(
            "workflow name {:?} must match [A-Za-z0-9][A-Za-z0-9._-]{{0,47}}",
            name
        )));
    }
    Ok(())
}

fn validate_input_name(name: &str) -> Result<(), WorkflowError> {
    let bytes = name.as_bytes();
    if bytes.is_empty()
        || bytes.len() > MAX_WORKFLOW_INPUT_NAME_BYTES
        || !bytes[0].is_ascii_lowercase()
        || !bytes
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'_')
    {
        return Err(WorkflowError::Definition(format!(
            "workflow input name {:?} must match [a-z][a-z0-9_]{{0,47}}",
            name
        )));
    }
    Ok(())
}

fn input_default_description(input: &InputDefinition) -> String {
    match (&input.default, input.required) {
        (Some(default), _) => format!(" = {}", default),
        (None, true) => " (required)".to_owned(),
        (None, false) => String::new(),
    }
}

impl OutputType {
    fn as_str(self) -> &'static str {
        match self {
            Self::Json => "json",
            Self::File => "file",
        }
    }
}

fn default_workflow_timeout() -> i64 {
    DEFAULT_WORKFLOW_TIMEOUT_MS
}

fn deserialize_retry_policy<'de, D>(deserializer: D) -> Result<Option<RetryPolicy>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Value::deserialize(deserializer)?;
    if value.is_null() {
        return Err(de::Error::custom("retry must be an object"));
    }
    serde_json::from_value(value)
        .map(Some)
        .map_err(de::Error::custom)
}

fn open_workflow_file(path: &Path) -> io::Result<File> {
    #[cfg(unix)]
    {
        OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)
    }
    #[cfg(not(unix))]
    {
        File::open(path)
    }
}
fn read_bounded(path: &Path, maximum: u64) -> Result<Vec<u8>, WorkflowError> {
    let file = open_workflow_file(path).map_err(|error| workflow_io(path, error))?;
    let length = file
        .metadata()
        .map_err(|error| workflow_io(path, error))?
        .len();
    if length > maximum {
        return Err(WorkflowError::Definition(format!(
            "workflow definition {} exceeds {maximum} bytes",
            path.display()
        )));
    }
    let mut bytes = Vec::with_capacity(length as usize);
    io::Read::take(file, maximum + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| workflow_io(path, error))?;
    if bytes.len() as u64 > maximum {
        return Err(WorkflowError::Definition(format!(
            "workflow definition {} exceeds {maximum} bytes",
            path.display()
        )));
    }
    Ok(bytes)
}

fn workflow_io(path: &Path, error: io::Error) -> WorkflowError {
    WorkflowError::Definition(format!("could not access {}: {error}", path.display()))
}

fn short_hash(value: &str) -> &str {
    value.get(..12).unwrap_or(value)
}

fn reject_duplicate_json_keys(bytes: &[u8]) -> Result<(), WorkflowError> {
    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    deserializer
        .deserialize_any(DuplicateKeyVisitor { depth: 0 })
        .map_err(|error| WorkflowError::Definition(format!("workflow JSON is invalid: {error}")))?;
    deserializer
        .end()
        .map_err(|error| WorkflowError::Definition(format!("workflow JSON is invalid: {error}")))
}

const MAX_WORKFLOW_JSON_DEPTH: usize = 64;

struct DuplicateKeyVisitor {
    depth: usize,
}

impl<'de> Visitor<'de> for DuplicateKeyVisitor {
    type Value = ();

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("valid JSON")
    }

    fn visit_map<M>(self, mut map: M) -> Result<(), M::Error>
    where
        M: MapAccess<'de>,
    {
        if self.depth >= MAX_WORKFLOW_JSON_DEPTH {
            return Err(de::Error::custom("workflow JSON nesting exceeds its bound"));
        }
        let mut keys = HashSet::new();
        while let Some(key) = map.next_key::<String>()? {
            if !keys.insert(key.clone()) {
                return Err(de::Error::custom(format!("duplicate JSON key {key:?}")));
            }
            map.next_value_seed(DuplicateKeySeed {
                depth: self.depth + 1,
            })?;
        }
        Ok(())
    }

    fn visit_seq<S>(self, mut sequence: S) -> Result<(), S::Error>
    where
        S: SeqAccess<'de>,
    {
        if self.depth >= MAX_WORKFLOW_JSON_DEPTH {
            return Err(de::Error::custom("workflow JSON nesting exceeds its bound"));
        }
        while sequence
            .next_element_seed(DuplicateKeySeed {
                depth: self.depth + 1,
            })?
            .is_some()
        {}
        Ok(())
    }

    fn visit_bool<E>(self, _value: bool) -> Result<(), E> {
        Ok(())
    }

    fn visit_i64<E>(self, _value: i64) -> Result<(), E> {
        Ok(())
    }

    fn visit_u64<E>(self, _value: u64) -> Result<(), E> {
        Ok(())
    }

    fn visit_f64<E>(self, _value: f64) -> Result<(), E> {
        Ok(())
    }

    fn visit_str<E>(self, _value: &str) -> Result<(), E> {
        Ok(())
    }

    fn visit_string<E>(self, _value: String) -> Result<(), E> {
        Ok(())
    }

    fn visit_none<E>(self) -> Result<(), E> {
        Ok(())
    }

    fn visit_unit<E>(self) -> Result<(), E> {
        Ok(())
    }
}

struct DuplicateKeySeed {
    depth: usize,
}

impl<'de> DeserializeSeed<'de> for DuplicateKeySeed {
    type Value = ();

    fn deserialize<D>(self, deserializer: D) -> Result<(), D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(DuplicateKeyVisitor { depth: self.depth })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn definition(json: &str) -> WorkflowDefinition {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn duplicate_json_keys_are_rejected_recursively() {
        let error = reject_duplicate_json_keys(br#"{"a": {"b": 1, "b": 2}}"#).unwrap_err();
        assert!(error.to_string().contains("duplicate JSON key"));
        let deep = format!(
            "{}0{}",
            "[".repeat(MAX_WORKFLOW_JSON_DEPTH + 1),
            "]".repeat(MAX_WORKFLOW_JSON_DEPTH + 1)
        );
        let error = reject_duplicate_json_keys(deep.as_bytes()).unwrap_err();
        assert!(error.to_string().contains("nesting exceeds"));
    }

    #[test]
    fn typed_input_values_are_literal_arguments() {
        let workflow = definition(
            r#"{
                "schema_version": 1,
                "name": "verify",
                "inputs": {
                    "profile": {"type": "string", "required": true}
                },
                "tasks": [{
                    "id": "test",
                    "program": "/bin/echo",
                    "arguments": [{"workflow_input": "profile"}]
                }]
            }"#,
        );
        validate_definition(&workflow, Path::new("verify.json")).unwrap();
        let inputs = resolve_inputs(&workflow, &["profile=$(touch pwned)".into()]).unwrap();
        let plan = compile_plan(&workflow, &inputs, Path::new("/tmp")).unwrap();
        assert_eq!(plan["tasks"][0]["arguments"][0], "$(touch pwned)");
    }

    #[test]
    fn canonical_hash_ignores_json_formatting() {
        let first = definition(
            r#"{"schema_version":1,"name":"verify","tasks":[{"id":"test","program":"/bin/echo"}]}"#,
        );
        let second = definition(
            r#"{
                "tasks": [{"program": "/bin/echo", "id": "test"}],
                "name": "verify", "schema_version": 1
            }"#,
        );
        let first_bytes = serde_json::to_vec(&first).unwrap();
        let second_bytes = serde_json::to_vec(&second).unwrap();
        assert_eq!(first_bytes, second_bytes);
        let directory = tempfile::tempdir().unwrap();
        let first_path = directory.path().join("first.json");
        let second_path = directory.path().join("second.json");
        fs::write(&first_path, serde_json::to_vec(&first).unwrap()).unwrap();
        fs::write(&second_path, serde_json::to_vec_pretty(&second).unwrap()).unwrap();
        let first_loaded = load_definition(&first_path, directory.path()).unwrap();
        let second_loaded = load_definition(&second_path, directory.path()).unwrap();
        assert_eq!(
            first_loaded.definition_sha256,
            second_loaded.definition_sha256
        );
    }
    fn write_workflow(directory: &Path, file_name: &str, body: &str) {
        fs::write(directory.join(file_name), body).unwrap();
    }

    fn minimal(name: &str) -> String {
        format!(
            r#"{{"schema_version":1,"name":"{name}","tasks":[{{"id":"task","program":"/bin/true"}}]}}"#
        )
    }

    #[test]
    fn discovery_is_sorted_and_rejects_duplicate_names() {
        let directory = tempfile::tempdir().unwrap();
        let workflows = directory.path().join(WORKFLOW_DIRECTORY);
        fs::create_dir(&workflows).unwrap();
        write_workflow(&workflows, "zeta.json", &minimal("zeta"));
        write_workflow(&workflows, "alpha.json", &minimal("alpha"));

        let paths = discover_paths(directory.path()).unwrap();
        assert_eq!(
            paths
                .iter()
                .map(|path| path.file_name().unwrap().to_str().unwrap())
                .collect::<Vec<_>>(),
            ["alpha.json", "zeta.json"]
        );
        assert_eq!(
            load_all(directory.path())
                .unwrap()
                .iter()
                .map(|workflow| workflow.definition.name.as_str())
                .collect::<Vec<_>>(),
            ["alpha", "zeta"]
        );

        write_workflow(&workflows, "duplicate.json", &minimal("alpha"));
        let error = load_all(directory.path()).unwrap_err();
        assert!(error.to_string().contains("duplicate workflow name"));
    }
    #[test]
    fn workflow_definition_count_is_bounded() {
        let directory = tempfile::tempdir().unwrap();
        let workflows = directory.path().join(WORKFLOW_DIRECTORY);
        fs::create_dir(&workflows).unwrap();
        for index in 0..=MAX_WORKFLOWS {
            let name = format!("workflow-{index}");
            write_workflow(&workflows, &format!("{name}.json"), &minimal(&name));
        }

        let error = discover_paths(directory.path()).unwrap_err();
        assert!(error.to_string().contains("more than 64 definitions"));
    }
    #[cfg(unix)]
    #[test]
    fn symlinked_workflow_definition_is_rejected() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let workflows = directory.path().join(WORKFLOW_DIRECTORY);
        fs::create_dir(&workflows).unwrap();
        write_workflow(&workflows, "real.json", &minimal("real"));
        symlink(workflows.join("real.json"), workflows.join("linked.json")).unwrap();

        let error = load_all(directory.path()).unwrap_err();
        assert!(error.to_string().contains("symlinked"));
    }

    #[test]
    fn scalar_inputs_support_defaults_and_reject_bad_values() {
        let workflow = definition(
            r#"{
                "schema_version": 1,
                "name": "inputs",
                "inputs": {
                    "profile": {"type":"string", "default":"dev"},
                    "count": {"type":"integer", "default":2},
                    "enabled": {"type":"boolean", "default":true},
                    "required_value": {"type":"string", "required":true}
                },
                "tasks": [{"id":"task","program":"/bin/true"}]
            }"#,
        );
        validate_definition(&workflow, Path::new("inputs.json")).unwrap();
        let defaults = resolve_inputs(&workflow, &["required_value=ok".into()]).unwrap();
        assert_eq!(defaults["profile"], "dev");
        assert_eq!(defaults["count"], 2);
        assert_eq!(defaults["enabled"], true);
        assert!(resolve_inputs(&workflow, &[]).is_err());
        assert!(
            resolve_inputs(
                &workflow,
                &["count=nope".into(), "required_value=ok".into()]
            )
            .is_err()
        );
        assert!(resolve_inputs(&workflow, &["unknown=x".into()]).is_err());
        assert!(
            resolve_inputs(
                &workflow,
                &[
                    "profile=one".into(),
                    "profile=two".into(),
                    "required_value=ok".into()
                ]
            )
            .is_err()
        );
        assert!(
            parse_input_value(
                "profile",
                InputType::String,
                &"x".repeat(MAX_WORKFLOW_INPUT_STRING_BYTES + 1)
            )
            .is_err()
        );
    }

    #[test]
    fn output_references_compile_with_workflow_arguments() {
        let workflow = definition(
            r#"{
                "schema_version": 1,
                "name": "outputs",
                "inputs": {"profile": {"type":"string", "default":"dev"}},
                "tasks": [
                    {
                        "id":"inspect",
                        "program":"/bin/true",
                        "arguments":[{"workflow_input":"profile"}],
                        "outputs":[{"name":"report","type":"file","path":"report.txt"}]
                    },
                    {
                        "id":"consume",
                        "depends_on":["inspect"],
                        "program":"/bin/true",
                        "inputs":[{"from_task":"inspect","output":"report","as":"--report"}]
                    }
                ]
            }"#,
        );
        let inputs = resolve_inputs(&workflow, &[]).unwrap();
        let plan = compile_plan(&workflow, &inputs, Path::new("/tmp")).unwrap();
        assert_eq!(plan["tasks"][0]["arguments"][0], "dev");
        assert_eq!(plan["tasks"][1]["inputs"][0]["from_task"], "inspect");
    }

    #[test]
    fn malformed_schema_is_rejected_before_resolution() {
        let unknown = serde_json::from_str::<WorkflowDefinition>(
            r#"{"schema_version":1,"name":"bad","tasks":[],"extra":true}"#,
        );
        assert!(unknown.is_err());
        let unsupported = definition(
            r#"{"schema_version":2,"name":"bad","tasks":[{"id":"task","program":"/bin/true"}]}"#,
        );
        assert!(validate_definition(&unsupported, Path::new("bad.json")).is_err());
    }
    #[test]
    fn missing_directory_lists_empty_and_missing_name_is_clear() {
        let directory = tempfile::tempdir().unwrap();
        let WorkflowResult::Inspection(output) = list(directory.path()).unwrap() else {
            panic!("workflow list returned execution result");
        };
        assert_eq!(output.data["workflows"], json!([]));
        let error = find_workflow(directory.path(), "missing").unwrap_err();
        assert!(error.to_string().contains("not found"));
    }

    #[test]
    fn workflow_file_and_task_bounds_are_enforced() {
        let directory = tempfile::tempdir().unwrap();
        let workflows = directory.path().join(WORKFLOW_DIRECTORY);
        fs::create_dir(&workflows).unwrap();
        let oversized = workflows.join("oversized.json");
        fs::write(&oversized, vec![b'x'; MAX_WORKFLOW_BYTES as usize + 1]).unwrap();
        assert!(load_definition(&oversized, directory.path()).is_err());

        let tasks = (0..=orchestrate::MAX_TASKS)
            .map(|index| json!({"id": format!("task{index}"), "program":"/bin/true"}))
            .collect::<Vec<_>>();
        let too_many = definition(
            &serde_json::to_string(&json!({
                "schema_version": 1,
                "name": "too-many",
                "tasks": tasks
            }))
            .unwrap(),
        );
        assert!(validate_definition(&too_many, Path::new("too-many.json")).is_err());
    }
    #[test]
    fn invalid_output_reference_and_cycle_are_rejected() {
        let missing_output = definition(
            r#"{
                "schema_version":1,
                "name":"bad-output",
                "tasks":[
                    {"id":"producer","program":"/bin/true"},
                    {"id":"consumer","depends_on":["producer"],"program":"/bin/true","inputs":[{"from_task":"producer","output":"missing","as":"--value"}]}
                ]
            }"#,
        );
        let empty = BTreeMap::new();
        assert!(compile_plan(&missing_output, &empty, Path::new("/tmp")).is_err());

        let cycle = definition(
            r#"{
                "schema_version":1,
                "name":"cycle",
                "tasks":[
                    {"id":"first","depends_on":["second"],"program":"/bin/true"},
                    {"id":"second","depends_on":["first"],"program":"/bin/true"}
                ]
            }"#,
        );
        assert!(compile_plan(&cycle, &empty, Path::new("/tmp")).is_err());
    }
    #[test]
    fn expanded_plan_bound_applies_after_input_substitution() {
        let arguments = (0..300)
            .map(|_| json!({"workflow_input":"payload"}))
            .collect::<Vec<_>>();
        let workflow = definition(
            &serde_json::to_string(&json!({
                "schema_version":1,
                "name":"expanded",
                "inputs":{"payload":{"type":"string","required":true}},
                "tasks":[{"id":"task","program":"/bin/true","arguments":arguments}]
            }))
            .unwrap(),
        );
        validate_definition(&workflow, Path::new("expanded.json")).unwrap();
        let input = "x".repeat(MAX_WORKFLOW_INPUT_STRING_BYTES);
        let inputs = resolve_inputs(&workflow, &[format!("payload={input}")]).unwrap();
        let error = compile_plan(&workflow, &inputs, Path::new("/tmp")).unwrap_err();
        assert!(error.to_string().contains("resolved workflow plan exceeds"));
    }
}
