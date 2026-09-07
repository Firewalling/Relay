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
    collections::{BTreeMap, HashMap, HashSet},
    fmt,
    fs::{self, File, OpenOptions},
    io::{self, Read},
    path::{Path, PathBuf},
};
use thiserror::Error;

pub const WORKFLOW_DIRECTORY: &str = "relay-workflows";
const WORKFLOW_SCHEMA_VERSION_V1: u32 = 1;
const WORKFLOW_SCHEMA_VERSION_V2: u32 = 2;
const MAX_WORKFLOW_BYTES: u64 = 1 << 20;
const MAX_WORKFLOWS: usize = 64;
const MAX_WORKFLOW_NAME_BYTES: usize = 48;
const MAX_WORKFLOW_INPUTS: usize = 16;
const MAX_WORKFLOW_INPUT_NAME_BYTES: usize = 48;
const MAX_WORKFLOW_INPUT_STRING_BYTES: usize = 4 * 1024;
const MAX_COMPOSITION_DEPTH: usize = 8;
const MAX_COMPOSITION_INSTANCES_PER_WORKFLOW: usize = 16;
const MAX_COMPOSITION_INSTANCES: usize = 64;
const MAX_COMPOSITION_DEFINITIONS: usize = 64;
const MAX_COMPOSITION_DEPENDENCIES: usize = 512;
const MAX_COMPOSITION_OUTPUTS: usize = 256;
const MAX_COMPOSITION_EXPORTS: usize = 64;
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
    tasks: Vec<WorkflowNode>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    exports: BTreeMap<String, WorkflowExport>,
    #[serde(
        default,
        skip_serializing_if = "BTreeMap::is_empty",
        rename = "outputs"
    )]
    workflow_outputs: BTreeMap<String, WorkflowExport>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(untagged)]
enum WorkflowNode {
    Task(WorkflowTask),
    Workflow(WorkflowInstance),
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct WorkflowInstance {
    id: String,
    workflow: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    depends_on: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    inputs: BTreeMap<String, WorkflowInputBinding>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(untagged)]
enum WorkflowInputBinding {
    Parent(WorkflowInputReference),
    Literal(Value),
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct WorkflowExport {
    from_task: String,
    output: String,
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

#[derive(Clone, Debug)]
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
    for node in &definition.tasks {
        match node {
            WorkflowNode::Task(task) => {
                human.push_str(&format!("\n  {}", task.id));
                if !task.depends_on.is_empty() {
                    human.push_str(&format!(" <- {}", task.depends_on.join(", ")));
                }
            }
            WorkflowNode::Workflow(instance) => {
                human.push_str(&format!(
                    "\n  {} -> workflow {}",
                    instance.id, instance.workflow
                ));
                if !instance.depends_on.is_empty() {
                    human.push_str(&format!(" <- {}", instance.depends_on.join(", ")));
                }
            }
        }
    }
    let source = serde_json::to_value(definition).map_err(|error| {
        WorkflowError::Definition(format!("workflow cannot be encoded: {error}"))
    })?;
    let mut data = json!({
        "workflow": source,
        "definition_sha256": workflow.definition_sha256,
    });
    if args.resolved {
        let workflows = load_all(project_root)?;
        let inputs = placeholder_inputs(definition);
        let resolved =
            WorkflowResolver::new(&workflows).resolve(&definition.name, &inputs, project_root)?;
        data["resolved"] = json!({
            "plan": resolved.plan,
            "definition_sha256": resolved.definition_sha256,
            "resolved_plan_sha256": resolved.resolved_plan_sha256,
            "composition": resolved.composition,
        });
        human.push_str("\nResolved plan: available");
    }
    Ok(WorkflowResult::Inspection(RunOutput { human, data }))
}

fn validate(
    project_root: &Path,
    invocation_directory: &Path,
    args: WorkflowValidateArgs,
) -> Result<WorkflowResult, WorkflowError> {
    let workflows = load_all(project_root)?;
    let workflow = workflows
        .iter()
        .find(|workflow| workflow.definition.name == args.name)
        .cloned()
        .ok_or_else(|| WorkflowError::Definition(format!("workflow {:?} not found", args.name)))?;
    let inputs = resolve_inputs(&workflow.definition, &args.inputs)?;
    let resolved = WorkflowResolver::new(&workflows).resolve(
        &workflow.definition.name,
        &inputs,
        invocation_directory,
    )?;
    let metadata = WorkflowRunMetadata {
        name: workflow.definition.name.clone(),
        schema_version: workflow.definition.schema_version,
        definition_sha256: resolved.definition_sha256.clone(),
        inputs: inputs.clone(),
        composition: resolved.composition.clone(),
        resolved_plan_sha256: resolved
            .composition
            .as_ref()
            .map(|_| resolved.resolved_plan_sha256.clone()),
    };
    let task_count = resolved.plan["tasks"].as_array().map_or(0, Vec::len);
    let human = format!(
        "Workflow {} is valid\nTasks: {}\nDefinition: {}",
        workflow.definition.name,
        task_count,
        short_hash(&resolved.definition_sha256)
    );
    Ok(WorkflowResult::Inspection(RunOutput {
        human,
        data: json!({
            "workflow": workflow.definition,
            "definition_sha256": metadata.definition_sha256,
            "inputs": metadata.inputs,
            "composition": metadata.composition,
            "resolved_plan_sha256": metadata.resolved_plan_sha256,
            "plan": resolved.plan,
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
        let persisted_inputs = args
            .resume
            .as_deref()
            .and_then(|path| orchestrate::read_workflow_resume_metadata(path, project_root).ok())
            .map(|metadata| metadata.inputs);
        let current_definition_sha256 =
            current_definition_hash(project_root, &args.name, persisted_inputs.as_ref());
        return Ok(WorkflowResult::Run(orchestrate::run_workflow_resume(
            run_args,
            invocation_directory,
            project_root,
            args.name,
            current_definition_sha256,
        )?));
    }

    let workflows = load_all(project_root)?;
    let workflow = workflows
        .iter()
        .find(|workflow| workflow.definition.name == args.name)
        .cloned()
        .ok_or_else(|| WorkflowError::Definition(format!("workflow {:?} not found", args.name)))?;
    let inputs = resolve_inputs(&workflow.definition, &args.inputs)?;
    let resolved = WorkflowResolver::new(&workflows).resolve(
        &workflow.definition.name,
        &inputs,
        invocation_directory,
    )?;
    let metadata = WorkflowRunMetadata {
        name: workflow.definition.name,
        schema_version: workflow.definition.schema_version,
        definition_sha256: resolved.definition_sha256.clone(),
        inputs,
        composition: resolved.composition.clone(),
        resolved_plan_sha256: resolved
            .composition
            .as_ref()
            .map(|_| resolved.resolved_plan_sha256.clone()),
    };
    Ok(WorkflowResult::Run(orchestrate::run_resolved(
        run_args,
        invocation_directory,
        project_root,
        resolved.plan,
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
#[derive(Clone, Debug)]
struct ExpandedTask {
    task: WorkflowTask,
    arguments: Vec<String>,
}

#[derive(Clone, Debug)]
struct ResolvedOutput {
    producer_task: String,
    output: String,
}

#[derive(Clone, Debug)]
struct NodeGroup {
    roots: Vec<String>,
    terminals: Vec<String>,
    task_ids: Vec<String>,
    exports: BTreeMap<String, ResolvedOutput>,
    composed: bool,
}

#[derive(Clone, Debug, Serialize)]
struct CompositionHashInstance {
    instance: String,
    workflow: String,
    definition_sha256: String,
    resolved_inputs: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, Serialize)]
struct CompositionHashSource {
    root_workflow: String,
    root_definition_sha256: String,
    root_inputs: BTreeMap<String, Value>,
    instances: Vec<CompositionHashInstance>,
}

#[derive(Clone, Debug)]
struct ResolvedWorkflow {
    plan: Value,
    definition_sha256: String,
    composition: Option<orchestrate::WorkflowCompositionMetadata>,
    resolved_plan_sha256: String,
}

struct WorkflowResolver<'a> {
    workflows: &'a [LoadedWorkflow],
}

struct ExpansionContext<'a> {
    workflows: &'a [LoadedWorkflow],
    invocation_directory: &'a Path,
    tasks: Vec<ExpandedTask>,
    stack: Vec<String>,
    definitions: HashSet<String>,
    instances: Vec<orchestrate::WorkflowCompositionInstance>,
    hash_instances: Vec<CompositionHashInstance>,
    instances_by_workflow: HashMap<String, usize>,
    total_instances: usize,
    output_count: usize,
    export_count: usize,
}

impl<'a> WorkflowResolver<'a> {
    fn new(workflows: &'a [LoadedWorkflow]) -> Self {
        Self { workflows }
    }

    fn resolve(
        &self,
        root_name: &str,
        inputs: &BTreeMap<String, Value>,
        invocation_directory: &Path,
    ) -> Result<ResolvedWorkflow, WorkflowError> {
        let root = self
            .workflows
            .iter()
            .find(|workflow| workflow.definition.name == root_name)
            .ok_or_else(|| {
                WorkflowError::Definition(format!("workflow {:?} not found", root_name))
            })?;
        let root_definition_sha256 = root.definition_sha256.clone();
        let root_schema_version = root.definition.schema_version;
        let mut context = ExpansionContext {
            workflows: self.workflows,
            invocation_directory,
            tasks: Vec::new(),
            stack: Vec::new(),
            definitions: HashSet::new(),
            instances: Vec::new(),
            hash_instances: Vec::new(),
            instances_by_workflow: HashMap::new(),
            total_instances: 0,
            output_count: 0,
            export_count: 0,
        };
        let _ = context.expand(root_name, inputs, None, 0)?;
        let plan = context.plan_value()?;
        let encoded_plan = serde_json::to_vec(&plan).map_err(|error| {
            WorkflowError::Definition(format!("resolved workflow plan cannot be encoded: {error}"))
        })?;
        let resolved_plan_sha256 = orchestrate::hex_digest(sha2::Sha256::digest(&encoded_plan));
        let composition =
            if root_schema_version == WORKFLOW_SCHEMA_VERSION_V2 || !context.instances.is_empty() {
                Some(orchestrate::WorkflowCompositionMetadata {
                    root_workflow: root_name.to_owned(),
                    root_definition_sha256: root_definition_sha256.clone(),
                    instances: context.instances,
                    resolved_plan_sha256: resolved_plan_sha256.clone(),
                })
            } else {
                None
            };
        if let Some(metadata) = composition.as_ref() {
            let bytes = serde_json::to_vec(metadata).map_err(|error| {
                WorkflowError::Definition(format!(
                    "composition metadata cannot be encoded: {error}"
                ))
            })?;
            if bytes.len() > 16 * 1024 {
                return Err(WorkflowError::Definition(
                    "composition metadata exceeds 16384 bytes".into(),
                ));
            }
        }
        let definition_sha256 = if composition.is_some() {
            let source = CompositionHashSource {
                root_workflow: root_name.to_owned(),
                root_definition_sha256,
                root_inputs: inputs.clone(),
                instances: context.hash_instances,
            };
            let canonical = serde_json::to_vec(&source).map_err(|error| {
                WorkflowError::Definition(format!("composition cannot be canonicalized: {error}"))
            })?;
            orchestrate::hex_digest(sha2::Sha256::digest(&canonical))
        } else {
            root_definition_sha256
        };
        Ok(ResolvedWorkflow {
            plan,
            definition_sha256,
            composition,
            resolved_plan_sha256,
        })
    }
}

impl<'a> ExpansionContext<'a> {
    fn workflow(&self, name: &str, scope: &str) -> Result<LoadedWorkflow, WorkflowError> {
        self.workflows
            .iter()
            .find(|workflow| workflow.definition.name == name)
            .cloned()
            .ok_or_else(|| {
                WorkflowError::Definition(format!(
                    "workflow {scope}: instance references missing workflow {name:?}"
                ))
            })
    }

    fn expand(
        &mut self,
        name: &str,
        inputs: &BTreeMap<String, Value>,
        prefix: Option<&str>,
        depth: usize,
    ) -> Result<NodeGroup, WorkflowError> {
        if depth > MAX_COMPOSITION_DEPTH {
            return Err(WorkflowError::Definition(format!(
                "workflow composition exceeds maximum depth of {MAX_COMPOSITION_DEPTH}"
            )));
        }
        if let Some(start) = self.stack.iter().position(|current| current == name) {
            let mut cycle = self.stack[start..].to_vec();
            cycle.push(name.to_owned());
            return Err(WorkflowError::Definition(format!(
                "workflow composition cycle: {}",
                cycle.join(" -> ")
            )));
        }
        self.definitions.insert(name.to_owned());
        if self.definitions.len() > MAX_COMPOSITION_DEFINITIONS {
            return Err(WorkflowError::Definition(format!(
                "workflow composition references more than {MAX_COMPOSITION_DEFINITIONS} definitions"
            )));
        }
        let scope = prefix.unwrap_or(name).to_owned();
        let workflow = self.workflow(name, &scope)?;
        self.stack.push(name.to_owned());
        let result = self.expand_inner(&workflow, inputs, prefix, depth, &scope);
        self.stack.pop();
        result
    }

    fn expand_inner(
        &mut self,
        workflow: &LoadedWorkflow,
        inputs: &BTreeMap<String, Value>,
        prefix: Option<&str>,
        depth: usize,
        scope: &str,
    ) -> Result<NodeGroup, WorkflowError> {
        let mut groups = Vec::<(String, NodeGroup, Vec<String>)>::new();
        let mut node_ids = HashSet::new();
        for node in &workflow.definition.tasks {
            match node {
                WorkflowNode::Task(source) => {
                    if prefix.is_some() && source.id.contains('.') {
                        return Err(WorkflowError::Definition(format!(
                            "workflow {scope}: task id {:?} cannot contain '.' inside a composition",
                            source.id
                        )));
                    }
                    validate_source_task_id(&source.id, &format!("workflow {scope}"))?;
                    if !node_ids.insert(source.id.clone()) {
                        return Err(WorkflowError::Definition(format!(
                            "workflow {scope}: duplicate node id {:?}",
                            source.id
                        )));
                    }
                    let id = namespaced_id(prefix, &source.id, scope)?;
                    let mut task = source.clone();
                    task.id = id.clone();
                    task.depends_on.clear();
                    let arguments = resolve_arguments(source, inputs, scope)?;
                    self.output_count += task.outputs.len();
                    if self.output_count > MAX_COMPOSITION_OUTPUTS {
                        return Err(WorkflowError::Definition(format!(
                            "workflow composition exceeds {MAX_COMPOSITION_OUTPUTS} outputs"
                        )));
                    }
                    if self.tasks.len() >= orchestrate::MAX_TASKS {
                        return Err(WorkflowError::Definition(format!(
                            "expanded workflow exceeds {} tasks",
                            orchestrate::MAX_TASKS
                        )));
                    }
                    self.tasks.push(ExpandedTask { task, arguments });
                    groups.push((
                        source.id.clone(),
                        NodeGroup {
                            roots: vec![id.clone()],
                            terminals: vec![id.clone()],
                            task_ids: vec![id],
                            exports: BTreeMap::new(),
                            composed: false,
                        },
                        source.depends_on.clone(),
                    ));
                }
                WorkflowNode::Workflow(instance) => {
                    if workflow.definition.schema_version != WORKFLOW_SCHEMA_VERSION_V2 {
                        return Err(WorkflowError::Definition(format!(
                            "workflow {scope}: composition requires schema_version {WORKFLOW_SCHEMA_VERSION_V2}"
                        )));
                    }
                    validate_instance_id(&instance.id, &format!("workflow {scope}"))?;
                    validate_workflow_name(&instance.workflow)?;
                    if !node_ids.insert(instance.id.clone()) {
                        return Err(WorkflowError::Definition(format!(
                            "workflow {scope}: duplicate node id {:?}",
                            instance.id
                        )));
                    }
                    let child = self.workflow(&instance.workflow, scope)?;
                    let child_inputs = resolve_child_inputs(
                        inputs,
                        instance,
                        &child.definition,
                        &format!("workflow {scope}.{}", instance.id),
                    )?;
                    let child_path = namespaced_id(prefix, &instance.id, scope)?;
                    let count = self
                        .instances_by_workflow
                        .entry(workflow.definition.name.clone())
                        .or_default();
                    *count += 1;
                    if *count > MAX_COMPOSITION_INSTANCES_PER_WORKFLOW {
                        return Err(WorkflowError::Definition(format!(
                            "workflow {scope} contains more than {MAX_COMPOSITION_INSTANCES_PER_WORKFLOW} instances"
                        )));
                    }
                    self.total_instances += 1;
                    if self.total_instances > MAX_COMPOSITION_INSTANCES {
                        return Err(WorkflowError::Definition(format!(
                            "workflow composition exceeds {MAX_COMPOSITION_INSTANCES} instances"
                        )));
                    }
                    self.instances
                        .push(orchestrate::WorkflowCompositionInstance {
                            instance: child_path.clone(),
                            workflow: child.definition.name.clone(),
                            schema_version: child.definition.schema_version,
                            definition_sha256: child.definition_sha256.clone(),
                            resolved_inputs: child_inputs.clone(),
                        });
                    self.hash_instances.push(CompositionHashInstance {
                        instance: child_path.clone(),
                        workflow: child.definition.name.clone(),
                        definition_sha256: child.definition_sha256.clone(),
                        resolved_inputs: child_inputs.clone(),
                    });
                    let child_group = self.expand(
                        &child.definition.name,
                        &child_inputs,
                        Some(&child_path),
                        depth + 1,
                    )?;
                    groups.push((
                        instance.id.clone(),
                        NodeGroup {
                            composed: true,
                            ..child_group
                        },
                        instance.depends_on.clone(),
                    ));
                }
            }
        }

        let group_map: HashMap<String, NodeGroup> = groups
            .iter()
            .map(|(id, group, _)| (id.clone(), group.clone()))
            .collect();
        for (node_id, group, dependencies) in &groups {
            let mut seen = HashSet::new();
            for dependency in dependencies {
                if !seen.insert(dependency.as_str()) {
                    return Err(WorkflowError::Definition(format!(
                        "workflow {scope}: node {:?} repeats dependency {:?}",
                        node_id, dependency
                    )));
                }
                let dependency_group = group_map.get(dependency).ok_or_else(|| {
                    WorkflowError::Definition(format!(
                        "workflow {scope}: node {:?} has unknown dependency {:?}",
                        node_id, dependency
                    ))
                })?;
                for root in &group.roots {
                    for terminal in &dependency_group.terminals {
                        self.add_dependency(root, terminal, scope)?;
                    }
                }
            }
        }

        for (_, group, _) in &groups {
            if group.composed {
                continue;
            }
            for task_id in &group.task_ids {
                let index = self.task_index(task_id, scope)?;
                let raw_inputs = self.tasks[index].task.inputs.clone();
                let mut resolved_inputs = Vec::with_capacity(raw_inputs.len());
                for input in raw_inputs {
                    let source_group = group_map.get(&input.from_task).ok_or_else(|| {
                        WorkflowError::Definition(format!(
                            "workflow {scope}: task {:?} references unknown producer {:?}",
                            task_id, input.from_task
                        ))
                    })?;
                    let resolved = if source_group.composed {
                        source_group.exports.get(&input.output).cloned().ok_or_else(|| {
                            WorkflowError::Definition(format!(
                                "workflow {scope}: task {:?} references unexported output {:?} from workflow instance {:?}",
                                task_id, input.output, input.from_task
                            ))
                        })?
                    } else {
                        let producer = source_group.task_ids.first().ok_or_else(|| {
                            WorkflowError::Definition(format!(
                                "workflow {scope}: producer {:?} has no task",
                                input.from_task
                            ))
                        })?;
                        let producer_index = self.task_index(producer, scope)?;
                        if !self.tasks[producer_index]
                            .task
                            .outputs
                            .iter()
                            .any(|output| output.name == input.output)
                        {
                            return Err(WorkflowError::Definition(format!(
                                "workflow {scope}: task {:?} references unknown output {:?} from {:?}",
                                task_id, input.output, input.from_task
                            )));
                        }
                        ResolvedOutput {
                            producer_task: producer.clone(),
                            output: input.output.clone(),
                        }
                    };
                    resolved_inputs.push(OutputInput {
                        from_task: resolved.producer_task,
                        output: resolved.output,
                        argument: input.argument,
                    });
                }
                self.tasks[index].task.inputs = resolved_inputs;
            }
        }

        let mut exports = BTreeMap::new();
        let declared_exports = effective_exports(&workflow.definition, scope)?;
        for (name, export) in &declared_exports {
            if !valid_output_name_local(name) {
                return Err(WorkflowError::Definition(format!(
                    "workflow {scope}: export name {:?} is invalid",
                    name
                )));
            }
            let source_group = group_map.get(&export.from_task).ok_or_else(|| {
                WorkflowError::Definition(format!(
                    "workflow {scope}: export {:?} references unknown node {:?}",
                    name, export.from_task
                ))
            })?;
            let resolved = if source_group.composed {
                source_group.exports.get(&export.output).cloned().ok_or_else(|| {
                    WorkflowError::Definition(format!(
                        "workflow {scope}: export {:?} references unexported output {:?} from workflow instance {:?}",
                        name, export.output, export.from_task
                    ))
                })?
            } else {
                let producer = source_group.task_ids.first().ok_or_else(|| {
                    WorkflowError::Definition(format!(
                        "workflow {scope}: export {:?} has no producer",
                        name
                    ))
                })?;
                let index = self.task_index(producer, scope)?;
                if !self.tasks[index]
                    .task
                    .outputs
                    .iter()
                    .any(|output| output.name == export.output)
                {
                    return Err(WorkflowError::Definition(format!(
                        "workflow {scope}: export {:?} references unknown output {:?}",
                        name, export.output
                    )));
                }
                ResolvedOutput {
                    producer_task: producer.clone(),
                    output: export.output.clone(),
                }
            };
            self.export_count += 1;
            if self.export_count > MAX_COMPOSITION_EXPORTS {
                return Err(WorkflowError::Definition(format!(
                    "workflow composition exceeds {MAX_COMPOSITION_EXPORTS} exports"
                )));
            }
            exports.insert(name.clone(), resolved);
        }
        Ok(NodeGroup {
            roots: root_nodes(&groups),
            terminals: terminal_nodes(&groups),
            task_ids: groups
                .iter()
                .flat_map(|(_, group, _)| group.task_ids.clone())
                .collect(),
            exports,
            composed: false,
        })
    }

    fn task_index(&self, id: &str, scope: &str) -> Result<usize, WorkflowError> {
        self.tasks
            .iter()
            .position(|task| task.task.id == id)
            .ok_or_else(|| {
                WorkflowError::Definition(format!("workflow {scope}: task {:?} is missing", id))
            })
    }

    fn add_dependency(
        &mut self,
        task_id: &str,
        dependency: &str,
        scope: &str,
    ) -> Result<(), WorkflowError> {
        if task_id == dependency {
            return Err(WorkflowError::Definition(format!(
                "workflow {scope}: task {:?} cannot depend on itself",
                task_id
            )));
        }
        let index = self.task_index(task_id, scope)?;
        if !self.tasks[index]
            .task
            .depends_on
            .iter()
            .any(|current| current == dependency)
        {
            self.tasks[index]
                .task
                .depends_on
                .push(dependency.to_owned());
        }
        Ok(())
    }

    fn plan_value(&self) -> Result<Value, WorkflowError> {
        let dependency_count = self.tasks.iter().try_fold(0usize, |total, task| {
            total
                .checked_add(task.task.depends_on.len())
                .ok_or_else(|| {
                    WorkflowError::Definition("expanded dependency count overflow".into())
                })
        })?;
        if dependency_count > MAX_COMPOSITION_DEPENDENCIES {
            return Err(WorkflowError::Definition(format!(
                "workflow composition exceeds {MAX_COMPOSITION_DEPENDENCIES} dependencies"
            )));
        }
        let tasks = self
            .tasks
            .iter()
            .map(task_to_value)
            .collect::<Result<Vec<_>, _>>()?;
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
        orchestrate::validate_plan_value(plan.clone(), self.invocation_directory)
            .map_err(|error| WorkflowError::Definition(error.to_string()))?;
        Ok(plan)
    }
}

fn root_nodes(groups: &[(String, NodeGroup, Vec<String>)]) -> Vec<String> {
    groups
        .iter()
        .filter(|(_, _, dependencies)| dependencies.is_empty())
        .flat_map(|(_, group, _)| group.roots.clone())
        .collect()
}

fn terminal_nodes(groups: &[(String, NodeGroup, Vec<String>)]) -> Vec<String> {
    let depended_on: HashSet<&str> = groups
        .iter()
        .flat_map(|(_, _, dependencies)| dependencies.iter().map(String::as_str))
        .collect();
    groups
        .iter()
        .filter(|(id, _, _)| !depended_on.contains(id.as_str()))
        .flat_map(|(_, group, _)| group.terminals.clone())
        .collect()
}

fn task_to_value(expanded: &ExpandedTask) -> Result<Value, WorkflowError> {
    let task = &expanded.task;
    let mut value = Map::new();
    value.insert("id".into(), json!(task.id));
    value.insert("depends_on".into(), json!(task.depends_on));
    value.insert("program".into(), json!(task.program));
    value.insert("arguments".into(), json!(expanded.arguments));
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
            WorkflowError::Definition(format!("workflow outputs cannot be encoded: {error}"))
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
                WorkflowError::Definition(format!("workflow retry cannot be encoded: {error}"))
            })?,
        );
    }
    Ok(Value::Object(value))
}

fn namespaced_id(prefix: Option<&str>, id: &str, scope: &str) -> Result<String, WorkflowError> {
    let value = prefix.map_or_else(|| id.to_owned(), |prefix| format!("{prefix}.{id}"));
    if !valid_plan_task_id(&value) {
        return Err(WorkflowError::Definition(format!(
            "workflow {scope}: namespaced task id {:?} exceeds the task-id bound",
            value
        )));
    }
    Ok(value)
}

fn validate_source_task_id(id: &str, scope: &str) -> Result<(), WorkflowError> {
    if !valid_plan_task_id(id) {
        return Err(WorkflowError::Definition(format!(
            "{scope}: task id {:?} must match [A-Za-z0-9][A-Za-z0-9._-]{{0,47}}",
            id
        )));
    }
    Ok(())
}

fn validate_instance_id(id: &str, scope: &str) -> Result<(), WorkflowError> {
    validate_source_task_id(id, scope)?;
    if id.contains('.') {
        return Err(WorkflowError::Definition(format!(
            "{scope}: composition instance id {:?} cannot contain '.'",
            id
        )));
    }
    Ok(())
}

fn valid_plan_task_id(value: &str) -> bool {
    let bytes = value.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 48
        && bytes[0].is_ascii_alphanumeric()
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn valid_output_name_local(value: &str) -> bool {
    let bytes = value.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 64
        && bytes[0].is_ascii_lowercase()
        && bytes[1..]
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'_')
}
fn effective_exports(
    definition: &WorkflowDefinition,
    scope: &str,
) -> Result<BTreeMap<String, WorkflowExport>, WorkflowError> {
    if !definition.exports.is_empty() && !definition.workflow_outputs.is_empty() {
        return Err(WorkflowError::Definition(format!(
            "workflow {scope}: exports and top-level outputs cannot both be set"
        )));
    }
    if definition.exports.is_empty() {
        Ok(definition.workflow_outputs.clone())
    } else {
        Ok(definition.exports.clone())
    }
}

fn resolve_arguments(
    task: &WorkflowTask,
    inputs: &BTreeMap<String, Value>,
    scope: &str,
) -> Result<Vec<String>, WorkflowError> {
    task.arguments
        .iter()
        .map(|argument| match argument {
            WorkflowArgument::Literal(value) => Ok(value.clone()),
            WorkflowArgument::Input(reference) => inputs
                .get(&reference.workflow_input)
                .map(|value| {
                    input_value_as_argument(value)
                        .as_str()
                        .unwrap_or_default()
                        .to_owned()
                })
                .ok_or_else(|| {
                    WorkflowError::Definition(format!(
                        "workflow {scope}: input {:?} has no value",
                        reference.workflow_input
                    ))
                }),
        })
        .collect()
}

fn resolve_child_inputs(
    parent_inputs: &BTreeMap<String, Value>,
    instance: &WorkflowInstance,
    child: &WorkflowDefinition,
    scope: &str,
) -> Result<BTreeMap<String, Value>, WorkflowError> {
    let mut resolved = BTreeMap::new();
    for (name, binding) in &instance.inputs {
        validate_input_name(name)?;
        let definition = child.inputs.get(name).ok_or_else(|| {
            WorkflowError::Definition(format!("workflow {scope}: unknown child input {:?}", name))
        })?;
        let value = match binding {
            WorkflowInputBinding::Parent(reference) => {
                validate_input_name(&reference.workflow_input)?;
                parent_inputs
                    .get(&reference.workflow_input)
                    .cloned()
                    .ok_or_else(|| {
                        WorkflowError::Definition(format!(
                            "workflow {scope}: parent input {:?} is unavailable",
                            reference.workflow_input
                        ))
                    })?
            }
            WorkflowInputBinding::Literal(value) => value.clone(),
        };
        validate_input_value(name, definition.input_type, &value)?;
        resolved.insert(name.clone(), value);
    }
    for (name, definition) in &child.inputs {
        if resolved.contains_key(name) {
            continue;
        }
        if let Some(default) = &definition.default {
            resolved.insert(name.clone(), default.clone());
        } else if definition.required {
            return Err(WorkflowError::Definition(format!(
                "workflow {scope}: missing required input {:?}",
                name
            )));
        }
    }
    Ok(resolved)
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

fn current_definition_hash(
    project_root: &Path,
    name: &str,
    persisted_inputs: Option<&BTreeMap<String, Value>>,
) -> Option<String> {
    let workflows = load_all(project_root).ok()?;
    let workflow = workflows
        .iter()
        .find(|workflow| workflow.definition.name == name)?;
    let inputs = persisted_inputs
        .cloned()
        .unwrap_or_else(|| placeholder_inputs(&workflow.definition));
    WorkflowResolver::new(&workflows)
        .resolve(name, &inputs, project_root)
        .ok()
        .map(|resolved| resolved.definition_sha256)
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

fn load_definition(path: &Path, _project_root: &Path) -> Result<LoadedWorkflow, WorkflowError> {
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
    if definition.schema_version == WORKFLOW_SCHEMA_VERSION_V1 {
        let placeholders = placeholder_inputs(&definition);
        compile_plan(&definition, &placeholders, _project_root)?;
    }
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
    if !matches!(
        definition.schema_version,
        WORKFLOW_SCHEMA_VERSION_V1 | WORKFLOW_SCHEMA_VERSION_V2
    ) {
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
    let mut node_ids = HashSet::new();
    for node in &definition.tasks {
        match node {
            WorkflowNode::Task(task) => {
                validate_source_task_id(&task.id, &format!("workflow {}", definition.name))?;
                if !node_ids.insert(task.id.clone()) {
                    return Err(WorkflowError::Definition(format!(
                        "workflow {}: duplicate node id {:?}",
                        definition.name, task.id
                    )));
                }
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
            WorkflowNode::Workflow(instance) => {
                if definition.schema_version != WORKFLOW_SCHEMA_VERSION_V2 {
                    return Err(WorkflowError::Definition(format!(
                        "workflow {}: composition requires schema_version {WORKFLOW_SCHEMA_VERSION_V2}",
                        definition.name
                    )));
                }
                validate_instance_id(&instance.id, &format!("workflow {}", definition.name))?;
                validate_workflow_name(&instance.workflow)?;
                if !node_ids.insert(instance.id.clone()) {
                    return Err(WorkflowError::Definition(format!(
                        "workflow {}: duplicate node id {:?}",
                        definition.name, instance.id
                    )));
                }
                for (name, binding) in &instance.inputs {
                    validate_input_name(name)?;
                    if let WorkflowInputBinding::Parent(reference) = binding {
                        validate_input_name(&reference.workflow_input)?;
                    }
                }
            }
        }
    }
    let has_exports = !definition.exports.is_empty() || !definition.workflow_outputs.is_empty();
    if definition.schema_version == WORKFLOW_SCHEMA_VERSION_V1 && has_exports {
        return Err(WorkflowError::Definition(
            "workflow exports require schema_version 2".into(),
        ));
    }
    let declared_exports = effective_exports(definition, &definition.name)?;
    if declared_exports.len() > MAX_COMPOSITION_EXPORTS {
        return Err(WorkflowError::Definition(format!(
            "workflow declares more than {MAX_COMPOSITION_EXPORTS} exports"
        )));
    }
    for (name, export) in &declared_exports {
        if !valid_output_name_local(name)
            || export.from_task.is_empty()
            || !valid_output_name_local(&export.output)
        {
            return Err(WorkflowError::Definition(format!(
                "workflow {}: export {:?} is invalid",
                definition.name, name
            )));
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
    let canonical = serde_json::to_vec(definition).map_err(|error| {
        WorkflowError::Definition(format!("workflow cannot be canonicalized: {error}"))
    })?;
    let loaded = LoadedWorkflow {
        definition: definition.clone(),
        definition_sha256: orchestrate::hex_digest(sha2::Sha256::digest(&canonical)),
    };
    let workflows = [loaded];
    WorkflowResolver::new(&workflows)
        .resolve(&definition.name, inputs, invocation_directory)
        .map(|resolved| resolved.plan)
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
            r#"{"schema_version":3,"name":"bad","tasks":[{"id":"task","program":"/bin/true"}]}"#,
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
    #[test]
    fn composition_expands_namespaces_dependencies_and_exports() {
        let directory = tempfile::tempdir().unwrap();
        let workflows = directory.path().join(WORKFLOW_DIRECTORY);
        fs::create_dir(&workflows).unwrap();
        let verify = json!({
            "schema_version": 2,
            "name": "verify",
            "inputs": {"profile": {"type": "string", "required": true}},
            "tasks": [
                {"id": "inspect", "program": "/bin/true"},
                {"id": "test", "depends_on": ["inspect"], "program": "/bin/true",
                 "outputs": [{"name": "report", "type": "file", "path": "report.txt"}]}
            ],
            "exports": {"report": {"from_task": "test", "output": "report"}}
        });
        let release = json!({
            "schema_version": 2,
            "name": "release",
            "inputs": {"profile": {"type": "string", "required": true}},
            "tasks": [
                {"id": "verify", "workflow": "verify", "inputs": {"profile": {"workflow_input": "profile"}}},
                {"id": "package", "depends_on": ["verify"], "program": "/bin/true",
                 "inputs": [{"from_task": "verify", "output": "report", "as": "--report"}]}
            ]
        });
        write_workflow(
            &workflows,
            "verify.json",
            &serde_json::to_string(&verify).unwrap(),
        );
        write_workflow(
            &workflows,
            "release.json",
            &serde_json::to_string(&release).unwrap(),
        );
        let catalog = load_all(directory.path()).unwrap();
        let root = catalog
            .iter()
            .find(|workflow| workflow.definition.name == "release")
            .unwrap();
        let inputs = resolve_inputs(&root.definition, &["profile=release".into()]).unwrap();
        let resolved = WorkflowResolver::new(&catalog)
            .resolve("release", &inputs, directory.path())
            .unwrap();
        let tasks = resolved.plan["tasks"].as_array().unwrap();
        assert_eq!(
            tasks
                .iter()
                .map(|task| task["id"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["verify.inspect", "verify.test", "package"]
        );
        assert_eq!(tasks[1]["depends_on"], json!(["verify.inspect"]));
        assert_eq!(tasks[2]["depends_on"], json!(["verify.test"]));
        assert_eq!(tasks[2]["inputs"][0]["from_task"], "verify.test");
        assert_eq!(tasks[2]["inputs"][0]["output"], "report");
        let composition = resolved.composition.unwrap();
        assert_eq!(composition.instances[0].instance, "verify");
        assert_eq!(
            composition.instances[0].resolved_inputs["profile"],
            "release"
        );
        assert_eq!(
            composition.resolved_plan_sha256,
            resolved.resolved_plan_sha256
        );
    }

    #[test]
    fn composition_supports_nested_and_repeated_instances() {
        let directory = tempfile::tempdir().unwrap();
        let workflows = directory.path().join(WORKFLOW_DIRECTORY);
        fs::create_dir(&workflows).unwrap();
        let unit = json!({
            "schema_version": 2,
            "name": "unit",
            "tasks": [{"id": "test", "program": "/bin/true", "outputs": [{"name": "report", "type": "file", "path": "report.txt"}]}],
            "outputs": {"report": {"from_task": "test", "output": "report"}},
        });
        let verify = json!({
            "schema_version": 2,
            "name": "verify",
            "tasks": [{"id": "unit", "workflow": "unit"}],
            "exports": {"report": {"from_task": "unit", "output": "report"}}
        });
        let release = json!({
            "schema_version": 2,
            "name": "release",
            "tasks": [
                {"id": "backend", "workflow": "verify"},
                {"id": "frontend", "workflow": "verify"}
            ]
        });
        for (name, value) in [("unit", unit), ("verify", verify), ("release", release)] {
            write_workflow(
                &workflows,
                &format!("{name}.json"),
                &serde_json::to_string(&value).unwrap(),
            );
        }
        let catalog = load_all(directory.path()).unwrap();
        let empty = BTreeMap::new();
        let resolved = WorkflowResolver::new(&catalog)
            .resolve("release", &empty, directory.path())
            .unwrap();
        let ids = resolved.plan["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .map(|task| task["id"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(ids, ["backend.unit.test", "frontend.unit.test"]);
        assert_eq!(
            resolved
                .composition
                .unwrap()
                .instances
                .iter()
                .map(|instance| instance.instance.as_str())
                .collect::<Vec<_>>(),
            ["backend", "backend.unit", "frontend", "frontend.unit"]
        );
    }

    #[test]
    fn composition_cycles_and_missing_exports_are_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let workflows = directory.path().join(WORKFLOW_DIRECTORY);
        fs::create_dir(&workflows).unwrap();
        let a = json!({"schema_version": 2, "name": "a", "tasks": [{"id": "b", "workflow": "b"}]});
        let b = json!({"schema_version": 2, "name": "b", "tasks": [{"id": "a", "workflow": "a"}]});
        write_workflow(&workflows, "a.json", &serde_json::to_string(&a).unwrap());
        write_workflow(&workflows, "b.json", &serde_json::to_string(&b).unwrap());
        let catalog = load_all(directory.path()).unwrap();
        let error = WorkflowResolver::new(&catalog)
            .resolve("a", &BTreeMap::new(), directory.path())
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("workflow composition cycle: a -> b -> a")
        );

        let bad = json!({
            "schema_version": 2,
            "name": "bad",
            "tasks": [{"id": "child", "workflow": "missing"}]
        });
        write_workflow(
            &workflows,
            "bad.json",
            &serde_json::to_string(&bad).unwrap(),
        );
        let catalog = load_all(directory.path()).unwrap();
        let error = WorkflowResolver::new(&catalog)
            .resolve("bad", &BTreeMap::new(), directory.path())
            .unwrap_err();
        assert!(error.to_string().contains("missing workflow"));
    }

    #[test]
    fn composition_hash_changes_when_child_changes() {
        let directory = tempfile::tempdir().unwrap();
        let workflows = directory.path().join(WORKFLOW_DIRECTORY);
        fs::create_dir(&workflows).unwrap();
        let child = json!({"schema_version": 2, "name": "child", "tasks": [{"id": "task", "program": "/bin/true"}]});
        let root = json!({"schema_version": 2, "name": "root", "tasks": [{"id": "child", "workflow": "child"}]});
        write_workflow(
            &workflows,
            "child.json",
            &serde_json::to_string(&child).unwrap(),
        );
        write_workflow(
            &workflows,
            "root.json",
            &serde_json::to_string(&root).unwrap(),
        );
        let first_catalog = load_all(directory.path()).unwrap();
        let first = WorkflowResolver::new(&first_catalog)
            .resolve("root", &BTreeMap::new(), directory.path())
            .unwrap();
        let changed = json!({"schema_version": 2, "name": "child", "tasks": [{"id": "task", "program": "/bin/false"}]});
        write_workflow(
            &workflows,
            "child.json",
            &serde_json::to_string(&changed).unwrap(),
        );
        let second_catalog = load_all(directory.path()).unwrap();
        let second = WorkflowResolver::new(&second_catalog)
            .resolve("root", &BTreeMap::new(), directory.path())
            .unwrap();
        assert_ne!(first.definition_sha256, second.definition_sha256);
        assert_ne!(first.resolved_plan_sha256, second.resolved_plan_sha256);
    }

    #[test]
    fn composition_rejects_ambiguous_nested_task_ids() {
        let directory = tempfile::tempdir().unwrap();
        let workflows = directory.path().join(WORKFLOW_DIRECTORY);
        fs::create_dir(&workflows).unwrap();
        let child = json!({"schema_version": 2, "name": "child", "tasks": [{"id": "part.one", "program": "/bin/true"}]});
        let root = json!({"schema_version": 2, "name": "root", "tasks": [{"id": "child", "workflow": "child"}]});
        write_workflow(
            &workflows,
            "child.json",
            &serde_json::to_string(&child).unwrap(),
        );
        write_workflow(
            &workflows,
            "root.json",
            &serde_json::to_string(&root).unwrap(),
        );
        let catalog = load_all(directory.path()).unwrap();
        let error = WorkflowResolver::new(&catalog)
            .resolve("root", &BTreeMap::new(), directory.path())
            .unwrap_err();
        assert!(error.to_string().contains("cannot contain '.'"));
    }
    #[test]
    fn composition_depth_bound_accepts_limit_and_rejects_above() {
        let write_chain = |directory: &Path, depth: usize| {
            let workflows = directory.join(WORKFLOW_DIRECTORY);
            fs::create_dir(&workflows).unwrap();
            for index in 0..=depth {
                let name = format!("w{index}");
                let value = if index == depth {
                    json!({"schema_version": 2, "name": name, "tasks": [{"id": "task", "program": "/bin/true"}]})
                } else {
                    json!({"schema_version": 2, "name": name, "tasks": [{"id": "next", "workflow": format!("w{}", index + 1)}]})
                };
                write_workflow(
                    &workflows,
                    &format!("{name}.json"),
                    &serde_json::to_string(&value).unwrap(),
                );
            }
        };
        let at_limit = tempfile::tempdir().unwrap();
        write_chain(at_limit.path(), MAX_COMPOSITION_DEPTH);
        let catalog = load_all(at_limit.path()).unwrap();
        assert!(
            WorkflowResolver::new(&catalog)
                .resolve("w0", &BTreeMap::new(), at_limit.path())
                .is_ok()
        );
        let above_limit = tempfile::tempdir().unwrap();
        write_chain(above_limit.path(), MAX_COMPOSITION_DEPTH + 1);
        let catalog = load_all(above_limit.path()).unwrap();
        let error = WorkflowResolver::new(&catalog)
            .resolve("w0", &BTreeMap::new(), above_limit.path())
            .unwrap_err();
        assert!(error.to_string().contains("maximum depth"));
    }
    #[test]
    fn resolved_inputs_change_composition_identity() {
        let directory = tempfile::tempdir().unwrap();
        let workflows = directory.path().join(WORKFLOW_DIRECTORY);
        fs::create_dir(&workflows).unwrap();
        let child = json!({
            "schema_version": 2,
            "name": "child",
            "inputs": {"profile": {"type":"string", "required":true}},
            "tasks": [{"id":"task","program":"/bin/true","arguments":[{"workflow_input":"profile"}]}]
        });
        let root = json!({
            "schema_version": 2,
            "name": "root",
            "inputs": {"profile": {"type":"string", "required":true}},
            "tasks": [{"id":"child","workflow":"child","inputs":{"profile":{"workflow_input":"profile"}}}]
        });
        write_workflow(
            &workflows,
            "child.json",
            &serde_json::to_string(&child).unwrap(),
        );
        write_workflow(
            &workflows,
            "root.json",
            &serde_json::to_string(&root).unwrap(),
        );
        let catalog = load_all(directory.path()).unwrap();
        let first_inputs =
            BTreeMap::from([(String::from("profile"), Value::String(String::from("dev")))]);
        let second_inputs = BTreeMap::from([(
            String::from("profile"),
            Value::String(String::from("release")),
        )]);
        let first = WorkflowResolver::new(&catalog)
            .resolve("root", &first_inputs, directory.path())
            .unwrap();
        let second = WorkflowResolver::new(&catalog)
            .resolve("root", &second_inputs, directory.path())
            .unwrap();
        assert_ne!(first.definition_sha256, second.definition_sha256);
        assert_ne!(first.resolved_plan_sha256, second.resolved_plan_sha256);
    }
    fn resolved_plan(directory: &Path, name: &str) -> Value {
        let catalog = load_all(directory).unwrap();
        WorkflowResolver::new(&catalog)
            .resolve(name, &BTreeMap::new(), directory)
            .unwrap()
            .plan
    }

    fn dependencies(plan: &Value, id: &str) -> Vec<String> {
        let task = plan["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|task| task["id"].as_str() == Some(id))
            .unwrap_or_else(|| panic!("missing task {id:?}"));
        task["depends_on"]
            .as_array()
            .unwrap()
            .iter()
            .map(|dependency| dependency.as_str().unwrap().to_owned())
            .collect()
    }

    fn boundary_group(id: &str, dependencies: &[&str]) -> (String, NodeGroup, Vec<String>) {
        let id = id.to_owned();
        (
            id.clone(),
            NodeGroup {
                roots: vec![id.clone()],
                terminals: vec![id.clone()],
                task_ids: vec![id.clone()],
                exports: BTreeMap::new(),
                composed: false,
            },
            dependencies
                .iter()
                .map(|dependency| (*dependency).to_owned())
                .collect(),
        )
    }

    #[test]
    fn root_and_terminal_helpers_have_distinct_graph_meanings() {
        let linear = vec![
            boundary_group("a", &[]),
            boundary_group("b", &["a"]),
            boundary_group("c", &["b"]),
        ];
        assert_eq!(root_nodes(&linear), vec!["a"]);
        assert_eq!(terminal_nodes(&linear), vec!["c"]);

        let fork = vec![
            boundary_group("a", &[]),
            boundary_group("b", &["a"]),
            boundary_group("c", &["a"]),
        ];
        assert_eq!(root_nodes(&fork), vec!["a"]);
        assert_eq!(terminal_nodes(&fork), vec!["b", "c"]);

        let join = vec![
            boundary_group("a", &[]),
            boundary_group("b", &[]),
            boundary_group("c", &["a", "b"]),
        ];
        assert_eq!(root_nodes(&join), vec!["a", "b"]);
        assert_eq!(terminal_nodes(&join), vec!["c"]);

        let disconnected = vec![
            boundary_group("a", &[]),
            boundary_group("b", &["a"]),
            boundary_group("c", &[]),
        ];
        assert_eq!(root_nodes(&disconnected), vec!["a", "c"]);
        assert_eq!(terminal_nodes(&disconnected), vec!["b", "c"]);
    }

    #[test]
    fn composition_wires_parent_to_all_child_roots_and_terminals() {
        let directory = tempfile::tempdir().unwrap();
        let workflows = directory.path().join(WORKFLOW_DIRECTORY);
        fs::create_dir(&workflows).unwrap();
        let verify = json!({
            "schema_version": 1,
            "name": "verify",
            "tasks": [
                {"id": "lint", "program": "/bin/true"},
                {"id": "inspect", "program": "/bin/true"},
                {"id": "test", "depends_on": ["lint", "inspect"], "program": "/bin/true"}
            ]
        });
        let fanout = json!({
            "schema_version": 1,
            "name": "fanout",
            "tasks": [
                {"id": "inspect", "program": "/bin/true"},
                {"id": "test", "depends_on": ["inspect"], "program": "/bin/true"},
                {"id": "lint", "depends_on": ["inspect"], "program": "/bin/true"}
            ]
        });
        let root = json!({
            "schema_version": 2,
            "name": "root",
            "tasks": [
                {"id": "prepare", "program": "/bin/true"},
                {"id": "verify-instance", "workflow": "verify", "depends_on": ["prepare"]},
                {"id": "fanout-instance", "workflow": "fanout", "depends_on": ["prepare"]},
                {"id": "package", "depends_on": ["fanout-instance"], "program": "/bin/true"}
            ]
        });
        for (name, value) in [("verify", verify), ("fanout", fanout), ("root", root)] {
            write_workflow(
                &workflows,
                &format!("{name}.json"),
                &serde_json::to_string(&value).unwrap(),
            );
        }

        let plan = resolved_plan(directory.path(), "root");
        assert_eq!(dependencies(&plan, "verify-instance.lint"), vec!["prepare"]);
        assert_eq!(
            dependencies(&plan, "verify-instance.inspect"),
            vec!["prepare"]
        );
        assert_eq!(
            dependencies(&plan, "verify-instance.test"),
            vec!["verify-instance.lint", "verify-instance.inspect"]
        );
        assert_eq!(
            dependencies(&plan, "fanout-instance.inspect"),
            vec!["prepare"]
        );
        assert_eq!(
            dependencies(&plan, "fanout-instance.test"),
            vec!["fanout-instance.inspect"]
        );
        assert_eq!(
            dependencies(&plan, "fanout-instance.lint"),
            vec!["fanout-instance.inspect"]
        );
        assert_eq!(
            dependencies(&plan, "package"),
            vec!["fanout-instance.test", "fanout-instance.lint"]
        );
        assert!(!dependencies(&plan, "package").contains(&"fanout-instance.inspect".into()));
    }

    #[test]
    fn composition_preserves_nested_sibling_and_repeated_boundaries() {
        let directory = tempfile::tempdir().unwrap();
        let workflows = directory.path().join(WORKFLOW_DIRECTORY);
        fs::create_dir(&workflows).unwrap();
        let inner = json!({
            "schema_version": 1,
            "name": "inner",
            "tasks": [
                {"id": "inspect", "program": "/bin/true"},
                {"id": "test", "depends_on": ["inspect"], "program": "/bin/true"}
            ]
        });
        let outer = json!({
            "schema_version": 2,
            "name": "outer",
            "tasks": [{"id": "inner-instance", "workflow": "inner"}]
        });
        let root = json!({
            "schema_version": 2,
            "name": "root",
            "tasks": [
                {"id": "prepare", "program": "/bin/true"},
                {"id": "backend", "workflow": "outer", "depends_on": ["prepare"]},
                {"id": "frontend", "workflow": "outer", "depends_on": ["prepare"]}
            ]
        });
        for (name, value) in [("inner", inner), ("outer", outer), ("root", root)] {
            write_workflow(
                &workflows,
                &format!("{name}.json"),
                &serde_json::to_string(&value).unwrap(),
            );
        }

        let plan = resolved_plan(directory.path(), "root");
        assert_eq!(
            dependencies(&plan, "backend.inner-instance.inspect"),
            vec!["prepare"]
        );
        assert_eq!(
            dependencies(&plan, "backend.inner-instance.test"),
            vec!["backend.inner-instance.inspect"]
        );
        assert_eq!(
            dependencies(&plan, "frontend.inner-instance.inspect"),
            vec!["prepare"]
        );
        assert_eq!(
            dependencies(&plan, "frontend.inner-instance.test"),
            vec!["frontend.inner-instance.inspect"]
        );
        assert!(
            !dependencies(&plan, "backend.inner-instance.test")
                .iter()
                .any(|dependency| dependency.starts_with("frontend."))
        );
    }

    #[test]
    fn downstream_composed_workflow_waits_for_upstream_terminals() {
        let directory = tempfile::tempdir().unwrap();
        let workflows = directory.path().join(WORKFLOW_DIRECTORY);
        fs::create_dir(&workflows).unwrap();
        let verify = json!({
            "schema_version": 1,
            "name": "verify",
            "tasks": [
                {"id": "inspect", "program": "/bin/true"},
                {"id": "test", "depends_on": ["inspect"], "program": "/bin/true"}
            ]
        });
        let package = json!({
            "schema_version": 1,
            "name": "package",
            "tasks": [
                {"id": "prepare", "program": "/bin/true"},
                {"id": "bundle", "depends_on": ["prepare"], "program": "/bin/true"}
            ]
        });
        let release = json!({
            "schema_version": 2,
            "name": "release",
            "tasks": [
                {"id": "verify-instance", "workflow": "verify"},
                {"id": "package-instance", "workflow": "package", "depends_on": ["verify-instance"]}
            ]
        });
        for (name, value) in [
            ("verify", verify),
            ("package", package),
            ("release", release),
        ] {
            write_workflow(
                &workflows,
                &format!("{name}.json"),
                &serde_json::to_string(&value).unwrap(),
            );
        }

        let plan = resolved_plan(directory.path(), "release");
        assert_eq!(
            dependencies(&plan, "verify-instance.test"),
            vec!["verify-instance.inspect"]
        );
        assert_eq!(
            dependencies(&plan, "package-instance.prepare"),
            vec!["verify-instance.test"]
        );
        assert_eq!(
            dependencies(&plan, "package-instance.bundle"),
            vec!["package-instance.prepare"]
        );
    }

    #[test]
    fn exported_nonterminal_output_keeps_exact_producer_identity() {
        let directory = tempfile::tempdir().unwrap();
        let workflows = directory.path().join(WORKFLOW_DIRECTORY);
        fs::create_dir(&workflows).unwrap();
        let verify = json!({
            "schema_version": 2,
            "name": "verify",
            "tasks": [
                {"id": "build", "program": "/bin/true", "outputs": [{"name": "report", "type": "file", "path": "report.txt"}]},
                {"id": "test", "depends_on": ["build"], "program": "/bin/true"}
            ],
            "exports": {"report": {"from_task": "build", "output": "report"}}
        });
        let root = json!({
            "schema_version": 2,
            "name": "root",
            "tasks": [
                {"id": "verify-instance", "workflow": "verify"},
                {"id": "publish", "depends_on": ["verify-instance"], "program": "/bin/true",
                 "inputs": [{"from_task": "verify-instance", "output": "report", "as": "--report"}]}
            ]
        });
        write_workflow(
            &workflows,
            "verify.json",
            &serde_json::to_string(&verify).unwrap(),
        );
        write_workflow(
            &workflows,
            "root.json",
            &serde_json::to_string(&root).unwrap(),
        );

        let plan = resolved_plan(directory.path(), "root");
        assert_eq!(dependencies(&plan, "publish"), vec!["verify-instance.test"]);
        assert_eq!(
            plan["tasks"][2]["inputs"][0]["from_task"],
            "verify-instance.build"
        );
        assert_eq!(plan["tasks"][2]["inputs"][0]["output"], "report");
    }

    #[test]
    fn empty_workflows_are_rejected_before_composition() {
        let empty = definition(r#"{"schema_version": 1, "name": "empty", "tasks": []}"#);
        let error = validate_definition(&empty, Path::new("empty.json")).unwrap_err();
        assert!(error.to_string().contains("between 1 and"));
    }

    #[test]
    fn expanded_boundary_dependencies_remain_bounded() {
        let directory = tempfile::tempdir().unwrap();
        let workflows = directory.path().join(WORKFLOW_DIRECTORY);
        fs::create_dir(&workflows).unwrap();
        let parent_ids = (0..32)
            .map(|index| format!("parent{index}"))
            .collect::<Vec<_>>();
        let child_tasks = (0..32)
            .map(|index| json!({"id": format!("child{index}"), "program": "/bin/true"}))
            .collect::<Vec<_>>();
        let mut root_tasks = parent_ids
            .iter()
            .map(|id| json!({"id": id, "program": "/bin/true"}))
            .collect::<Vec<_>>();
        root_tasks.push(json!({
            "id": "child-instance",
            "workflow": "child",
            "depends_on": parent_ids
        }));
        let child = json!({"schema_version": 1, "name": "child", "tasks": child_tasks});
        let root = json!({"schema_version": 2, "name": "root", "tasks": root_tasks});
        write_workflow(
            &workflows,
            "child.json",
            &serde_json::to_string(&child).unwrap(),
        );
        write_workflow(
            &workflows,
            "root.json",
            &serde_json::to_string(&root).unwrap(),
        );
        let catalog = load_all(directory.path()).unwrap();
        let error = WorkflowResolver::new(&catalog)
            .resolve("root", &BTreeMap::new(), directory.path())
            .unwrap_err();
        assert!(error.to_string().contains("exceeds 512 dependencies"));
    }
}
