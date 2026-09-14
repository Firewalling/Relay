use chrono::{DateTime, Utc};
use clap::{ArgGroup, Args, CommandFactory, Parser, Subcommand, ValueEnum};
use clap_complete::{Shell, generate};
use std::io;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum OutputFormat {
    #[default]
    Human,
    Json,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum ProjectScope {
    #[default]
    Auto,
    Current,
    Repository,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GlobalOptions {
    pub output: OutputFormat,
    pub scope: ProjectScope,
}

#[derive(Debug, Parser)]
#[command(name = "relay", version, about = "Local-first terminal work handoffs")]
pub struct Cli {
    /// Emit one machine-readable JSON document.
    #[arg(long, global = true)]
    pub json: bool,

    /// Select how Relay finds the project root.
    #[arg(long, global = true, value_enum, default_value_t)]
    pub scope: ProjectScope,

    #[command(subcommand)]
    pub command: Command,
}
impl Cli {
    pub fn global_options(&self) -> GlobalOptions {
        GlobalOptions {
            output: if self.json {
                OutputFormat::Json
            } else {
                OutputFormat::Human
            },
            scope: self.scope,
        }
    }
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run a persisted task plan through the built-in Rust coordinator and Go one-child supervisor.
    Run(RunArgs),
    /// Run one absolute program through the Rust launcher and Go one-child supervisor.
    Supervise(SuperviseArgs),
    /// Run, inspect, and validate project-local reusable workflows.
    Workflow(WorkflowArgs),
    /// Prints the negotiated, versioned Rust/Go process contract and launches nothing.
    Protocol,
    /// Start a session with a goal.
    Start(StartArgs),
    /// Show the current session.
    Status,
    /// Record the current work state.
    Checkpoint(CheckpointArgs),
    /// Create a handoff for another agent.
    Handoff(HandoffArgs),
    /// Finish the active session.
    Finish,
    /// List recorded sessions.
    List,
    /// Show one recorded session.
    Show(SessionSelectorArgs),
    /// Show append-only event history.
    History(HistoryArgs),
    /// Search stored event types and payload strings for a case-sensitive plain substring.
    Search(SearchArgs),
    /// Continue a completed session as a new session.
    Resume(SessionSelectorArgs),
    /// Diagnose Relay state and optionally perform conservative repairs.
    Doctor(DoctorArgs),
    /// Record a categorized note.
    Note(NoteArgs),
    /// Generate bounded context for another agent.
    Context(ContextArgs),
    /// Generate a shell completion script.
    Completions(CompletionsArgs),
}

impl Command {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Run(_) => "run",
            Self::Protocol => "protocol",
            Self::Supervise(_) => "supervise",
            Self::Workflow(_) => "workflow",
            Self::Start(_) => "start",
            Self::Status => "status",
            Self::Checkpoint(_) => "checkpoint",
            Self::Handoff(_) => "handoff",
            Self::Finish => "finish",
            Self::List => "list",
            Self::Show(_) => "show",
            Self::History(_) => "history",
            Self::Search(_) => "search",
            Self::Resume(_) => "resume",
            Self::Doctor(_) => "doctor",
            Self::Note(_) => "note",
            Self::Context(_) => "context",
            Self::Completions(_) => "completions",
        }
    }
}

#[derive(Debug, Args)]
pub struct WorkflowArgs {
    #[command(subcommand)]
    pub command: WorkflowCommand,
}

#[derive(Debug, Subcommand)]
pub enum WorkflowCommand {
    /// List project-local reusable workflows.
    List,
    /// Show one reusable workflow without executing it.
    Show(WorkflowNameArgs),
    /// Parse and validate one reusable workflow.
    Validate(WorkflowValidateArgs),
    /// Explain the resolved workflow without executing it.
    Explain(WorkflowValidateArgs),
    /// Run one reusable workflow through the existing coordinator.
    Run(WorkflowRunArgs),
}

#[derive(Debug, Args)]
pub struct WorkflowNameArgs {
    /// Include the fully expanded execution plan.
    #[arg(long)]
    pub resolved: bool,
    /// Workflow name.
    pub name: String,
}

#[derive(Debug, Args)]
pub struct WorkflowValidateArgs {
    /// Optional NAME=VALUE input overrides used during validation.
    #[arg(long = "input", value_name = "NAME=VALUE", allow_hyphen_values = true)]
    pub inputs: Vec<String>,
    /// Workflow name.
    pub name: String,
}

#[derive(Debug, Clone, Args)]
#[command(group(
    ArgGroup::new("state")
        .args(["state_path", "resume"])
        .multiple(false)
))]
pub struct WorkflowRunArgs {
    /// Resolve, validate, explain, and exit without execution or writes.
    #[arg(long, conflicts_with = "resume")]
    pub dry_run: bool,
    /// Absolute path to the relay-supervisor executable.
    #[arg(long)]
    pub supervisor: Option<std::path::PathBuf>,
    /// Maximum number of DAG tasks to run simultaneously.
    #[arg(long, default_value_t = 1, value_parser = parse_jobs)]
    pub jobs: usize,
    /// Absolute aggregate run state path for a new run.
    #[arg(long)]
    pub state_path: Option<std::path::PathBuf>,
    /// Absolute aggregate run state path to resume.
    #[arg(long)]
    pub resume: Option<std::path::PathBuf>,
    /// Optional NAME=VALUE workflow input override.
    #[arg(long = "input", value_name = "NAME=VALUE", allow_hyphen_values = true)]
    pub inputs: Vec<String>,
    /// Workflow name.
    pub name: String,
}

#[derive(Debug, Args)]
pub struct CompletionsArgs {
    /// Shell whose completion script should be generated.
    #[arg(value_enum)]
    pub shell: CompletionShell,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum CompletionShell {
    Bash,
    Zsh,
    Fish,
}

impl From<CompletionShell> for Shell {
    fn from(shell: CompletionShell) -> Self {
        match shell {
            CompletionShell::Bash => Self::Bash,
            CompletionShell::Zsh => Self::Zsh,
            CompletionShell::Fish => Self::Fish,
        }
    }
}

pub fn generate_completions(shell: CompletionShell) {
    let mut command = Cli::command();
    generate(Shell::from(shell), &mut command, "relay", &mut io::stdout());
}

#[derive(Debug, Args)]
#[command(subcommand_negates_reqs = true, args_conflicts_with_subcommands = true)]
#[command(group(
    ArgGroup::new("state")
        .args(["state_path", "resume"])
        .multiple(false)
))]
pub struct RunArgs {
    #[command(subcommand)]
    pub command: Option<RunCommand>,
    /// Absolute path to the relay-supervisor executable.
    #[arg(long)]
    pub supervisor: Option<std::path::PathBuf>,
    /// Maximum number of DAG tasks to run simultaneously.
    #[arg(long, default_value_t = 1, value_parser = parse_jobs)]
    pub jobs: usize,
    /// Absolute aggregate run state path for a new run.
    #[arg(long)]
    pub state_path: Option<std::path::PathBuf>,
    /// Absolute aggregate run state path to resume.
    #[arg(long)]
    pub resume: Option<std::path::PathBuf>,
    /// Task plan JSON file.
    #[arg(required = true)]
    pub plan: Option<std::path::PathBuf>,
}

#[derive(Debug, Subcommand)]
pub enum RunCommand {
    /// Explain a persisted run without execution or reconciliation.
    Explain { state_or_run: std::path::PathBuf },
}

#[derive(Debug, Args)]
pub struct SuperviseArgs {
    /// Absolute path to the relay-supervisor executable.
    #[arg(long)]
    pub supervisor: Option<std::path::PathBuf>,
    /// Stable identifier for this supervised run.
    #[arg(long)]
    pub run_id: Option<String>,
    /// Supervisor-owned run record path.
    #[arg(long)]
    pub state_path: Option<std::path::PathBuf>,
    /// Directory in which the program runs; defaults to the current directory.
    #[arg(long)]
    pub working_directory: Option<std::path::PathBuf>,
    /// Child timeout in milliseconds; zero disables the timeout.
    #[arg(long, default_value_t = 0)]
    pub timeout_ms: i64,
    /// Grace period before the supervisor escalates termination.
    #[arg(long, default_value_t = 2_000)]
    pub termination_grace_ms: i64,
    /// Maximum retained child stdout bytes.
    #[arg(long, default_value_t = 1 << 20)]
    pub stdout_limit_bytes: i64,
    /// Maximum retained child stderr bytes.
    #[arg(long, default_value_t = 1 << 20)]
    pub stderr_limit_bytes: i64,
    /// Maximum retained supervisor observation-log bytes.
    #[arg(long, default_value_t = 64 << 10)]
    pub log_limit_bytes: i64,
    /// Zero-based child argument index to redact from persisted output.
    #[arg(long = "redact-argument-index")]
    pub redact_argument_indexes: Vec<usize>,
    #[arg(skip)]
    pub relay_environment: Vec<(String, String)>,
    /// Absolute program path and literal arguments, following `--`.
    #[arg(last = true, required = true, num_args = 1.., allow_hyphen_values = true)]
    pub program_and_args: Vec<String>,
}
#[derive(Debug, Args)]
pub struct DoctorArgs {
    /// Remove only provably complete stale operation markers.
    #[arg(long)]
    pub repair: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "snake_case")]
pub enum NoteCategory {
    Progress,
    Decision,
    Blocker,
    Observation,
    NextAction,
}

impl NoteCategory {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Progress => "progress",
            Self::Decision => "decision",
            Self::Blocker => "blocker",
            Self::Observation => "observation",
            Self::NextAction => "next_action",
        }
    }
}

#[derive(Debug, Args)]
#[command(group(ArgGroup::new("input").args(["text", "stdin", "editor"]).multiple(false)))]
pub struct NoteArgs {
    /// Note category.
    #[arg(value_enum)]
    pub category: NoteCategory,
    /// Note text.
    pub text: Option<String>,
    /// Read the note from standard input.
    #[arg(long)]
    pub stdin: bool,
    /// Open an editor to compose the note.
    #[arg(long)]
    pub editor: bool,
}

#[derive(Debug, Args)]
pub struct ContextArgs {
    /// Select a session UUID or unambiguous prefix.
    #[arg(long)]
    pub session: Option<String>,
    /// Select a structured handoff UUID, prefix, or latest alias.
    #[arg(long)]
    pub handoff: Option<String>,
    /// Include only these note categories.
    #[arg(long = "category", value_enum)]
    pub categories: Vec<NoteCategory>,
    /// Include only these event kinds.
    #[arg(long = "kind")]
    pub kinds: Vec<String>,
    /// Maximum recent matching notes to include.
    #[arg(long, default_value_t = 20)]
    pub notes: usize,
    /// Maximum recent matching events to include.
    #[arg(long, default_value_t = 20)]
    pub events: usize,
    /// Maximum recent checkpoints to include.
    #[arg(long, default_value_t = 5)]
    pub checkpoints: usize,
    /// Maximum human Markdown characters; items are never split.
    #[arg(long = "max-chars", default_value_t = 12000)]
    pub max_chars: usize,
}

#[derive(Debug, Args)]
pub struct StartArgs {
    /// The outcome this session is working toward.
    #[arg(value_parser = parse_goal)]
    pub goal: String,
}

#[derive(Debug, Args)]
pub struct CheckpointArgs {
    /// Optional description of the checkpoint.
    pub message: Option<String>,
}

#[derive(Debug, Args)]
pub struct HandoffArgs {
    /// Existing target-agent form, or explicit handoff action.
    #[arg(value_parser = parse_handoff_subject)]
    pub agent: Option<String>,
    /// Inspect one handoff by UUID or unambiguous prefix.
    #[arg(long = "show", value_name = "HANDOFF_ID", conflicts_with_all = ["latest", "list"])]
    pub show: Option<String>,
    /// Inspect the latest handoff.
    #[arg(long, conflicts_with_all = ["show", "list"])]
    pub latest: bool,
    /// List all handoffs.
    #[arg(long, conflicts_with_all = ["show", "latest"])]
    pub list: bool,
    /// Agent receiving the handoff in explicit create form.
    #[arg(long, alias = "target", value_parser = parse_agent)]
    pub to: Option<String>,
    /// Agent creating the handoff, when explicitly known.
    #[arg(long = "source-agent", alias = "from", value_parser = parse_agent)]
    pub source_agent: Option<String>,
    /// Optional handoff goal override.
    #[arg(long, value_parser = parse_goal)]
    pub goal: Option<String>,
    /// Work completed during the session.
    #[arg(long = "completed-work", alias = "completed")]
    pub completed_work: Vec<String>,
    /// An important decision made during the session.
    #[arg(long)]
    pub decision: Vec<String>,
    /// A failure or blocked item.
    #[arg(long = "failure", alias = "blocker")]
    pub failure: Vec<String>,
    /// Suggested next action for the receiving agent.
    #[arg(long = "next-action")]
    pub next_action: Option<String>,
    /// Additional suggested next actions.
    #[arg(long = "next-actions")]
    pub next_actions: Vec<String>,
    /// Project-local aggregate run state to reference.
    #[arg(long = "run")]
    pub runs: Vec<std::path::PathBuf>,
    /// Explicit checkpoint UUID, prefix, or latest selector.
    #[arg(long = "checkpoint")]
    pub checkpoints: Vec<String>,
    /// Explicit note event UUID or prefix.
    #[arg(long = "note")]
    pub notes: Vec<String>,
    /// Previous handoff UUID to supersede.
    #[arg(long)]
    pub supersedes: Option<String>,
}

#[derive(Debug, Args)]
pub struct SessionSelectorArgs {
    /// Exact session UUID, an unambiguous UUID prefix, or the reserved `latest` alias.
    pub session: String,
}

#[derive(Debug, Args)]
pub struct HistoryArgs {
    /// Restrict history to a session UUID, unambiguous prefix, or the reserved `latest` alias.
    #[arg(long)]
    pub session: Option<String>,

    /// Restrict history to an event type.
    #[arg(long = "type")]
    pub event_type: Option<String>,

    /// Display matching events newest first.
    #[arg(long)]
    pub reverse: bool,

    /// Include events at or after this inclusive RFC3339 timestamp.
    #[arg(long, value_parser = parse_rfc3339_utc)]
    pub since: Option<DateTime<Utc>>,

    /// Include events at or before this inclusive RFC3339 timestamp.
    #[arg(long, value_parser = parse_rfc3339_utc)]
    pub until: Option<DateTime<Utc>>,

    /// Maximum number of matching events to display.
    #[arg(long, default_value_t = 100)]
    pub limit: usize,
}

#[derive(Debug, Args)]
pub struct SearchArgs {
    /// Required case-sensitive plain substring matched against event type and payload strings.
    #[arg(value_parser = parse_search_query)]
    pub query: String,

    /// Filter before matching by session UUID, unambiguous prefix, or case-insensitive `latest` alias.
    #[arg(long)]
    pub session: Option<String>,

    /// Filter before matching to exactly this event type.
    #[arg(long = "type")]
    pub event_type: Option<String>,

    /// Filter before matching to notes with this category.
    #[arg(long, value_enum)]
    pub category: Option<NoteCategory>,

    /// Order matching events newest first before applying the limit.
    #[arg(long)]
    pub reverse: bool,

    /// Filter before matching to events at or after this inclusive RFC3339 timestamp.
    #[arg(long, value_parser = parse_rfc3339_utc)]
    pub since: Option<DateTime<Utc>>,

    /// Filter before matching to events at or before this inclusive RFC3339 timestamp.
    #[arg(long, value_parser = parse_rfc3339_utc)]
    pub until: Option<DateTime<Utc>>,

    /// Maximum number of matching events after ordering.
    #[arg(long, default_value_t = 100)]
    pub limit: usize,
}

fn parse_jobs(value: &str) -> Result<usize, String> {
    let jobs = value
        .parse::<usize>()
        .map_err(|_| "jobs must be a positive integer".to_owned())?;
    if jobs > 0 {
        Ok(jobs)
    } else {
        Err("jobs must be a positive integer".to_owned())
    }
}

fn parse_rfc3339_utc(value: &str) -> Result<DateTime<Utc>, String> {
    DateTime::parse_from_rfc3339(value)
        .map(|timestamp| timestamp.with_timezone(&Utc))
        .map_err(|_| {
            "invalid RFC3339 timestamp; expected a value such as 2025-01-02T03:04:05Z or 2025-01-02T03:04:05+02:00"
                .to_owned()
        })
}

fn parse_handoff_subject(value: &str) -> Result<String, String> {
    parse_nonblank(value, "handoff action or target agent")
}

fn parse_goal(value: &str) -> Result<String, String> {
    parse_nonblank(value, "goal")
}

fn parse_agent(value: &str) -> Result<String, String> {
    parse_nonblank(value, "target agent")
}

fn parse_search_query(value: &str) -> Result<String, String> {
    parse_nonblank(value, "search query")
}

fn parse_nonblank(value: &str, name: &str) -> Result<String, String> {
    if value.trim().is_empty() {
        Err(format!("{name} must not be blank or whitespace-only"))
    } else {
        Ok(value.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn human_and_auto_are_the_global_defaults() {
        let cli = Cli::try_parse_from(["relay", "status"]).unwrap();

        assert_eq!(
            cli.global_options(),
            GlobalOptions {
                output: OutputFormat::Human,
                scope: ProjectScope::Auto,
            }
        );
    }

    #[test]
    fn parses_global_options_on_either_side_of_command() {
        let before =
            Cli::try_parse_from(["relay", "--json", "--scope", "repository", "status"]).unwrap();
        let after =
            Cli::try_parse_from(["relay", "status", "--scope", "current", "--json"]).unwrap();

        assert_eq!(before.global_options().output, OutputFormat::Json);
        assert_eq!(before.scope, ProjectScope::Repository);
        assert_eq!(after.global_options().output, OutputFormat::Json);
        assert_eq!(after.scope, ProjectScope::Current);
    }

    #[test]
    fn run_jobs_default_is_one_and_positive_values_parse() {
        let default = Cli::try_parse_from(["relay", "run", "plan.json"]).unwrap();
        let explicit = Cli::try_parse_from(["relay", "run", "--jobs", "65", "plan.json"]).unwrap();
        let zero = Cli::try_parse_from(["relay", "run", "--jobs", "0", "plan.json"]);

        assert!(matches!(
            default.command,
            Command::Run(RunArgs { jobs: 1, .. })
        ));
        assert!(matches!(
            explicit.command,
            Command::Run(RunArgs { jobs: 65, .. })
        ));
        assert!(zero.is_err());
    }

    #[test]
    fn run_inspection_cannot_be_combined_with_execution_arguments() {
        let cli = Cli::try_parse_from(["relay", "run", "explain", "state.json"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Run(RunArgs {
                command: Some(RunCommand::Explain { .. }),
                plan: None,
                ..
            })
        ));
        assert!(Cli::try_parse_from(["relay", "run"]).is_err());
        assert!(Cli::try_parse_from(["relay", "run", "explain"]).is_err());
        assert!(
            Cli::try_parse_from(["relay", "run", "--jobs", "2", "explain", "state.json"]).is_err()
        );
        assert!(
            Cli::try_parse_from(["relay", "run", "plan.json", "explain", "state.json"]).is_err()
        );
    }

    #[test]
    fn protocol_is_argument_free_and_named_protocol() {
        let cli = Cli::try_parse_from(["relay", "protocol"]).unwrap();

        assert!(matches!(&cli.command, Command::Protocol));
        assert_eq!(cli.command.name(), "protocol");
    }

    #[test]
    fn protocol_accepts_global_options_on_either_side() {
        let before =
            Cli::try_parse_from(["relay", "--json", "--scope", "repository", "protocol"]).unwrap();
        let after =
            Cli::try_parse_from(["relay", "protocol", "--json", "--scope", "repository"]).unwrap();

        for cli in [before, after] {
            assert!(matches!(&cli.command, Command::Protocol));
            assert_eq!(cli.command.name(), "protocol");
            assert_eq!(
                cli.global_options(),
                GlobalOptions {
                    output: OutputFormat::Json,
                    scope: ProjectScope::Repository,
                }
            );
        }
    }

    #[test]
    fn protocol_rejects_unexpected_arguments() {
        assert!(Cli::try_parse_from(["relay", "protocol", "unexpected"]).is_err());
    }

    #[test]
    fn parses_handoff_notes() {
        let cli = Cli::try_parse_from([
            "relay",
            "handoff",
            "omp",
            "--completed-work",
            "added models",
            "--decision",
            "use jsonl",
            "--failure",
            "none",
            "--next-action",
            "add lifecycle",
        ])
        .unwrap();

        let Command::Handoff(args) = cli.command else {
            panic!("expected handoff command");
        };
        assert_eq!(args.agent, Some("omp".into()));
        assert_eq!(args.completed_work, ["added models"]);
        assert_eq!(args.decision, ["use jsonl"]);
        assert_eq!(args.failure, ["none"]);
        assert_eq!(args.next_action.as_deref(), Some("add lifecycle"));
    }

    #[test]
    fn rejects_blank_start_goal() {
        let error = Cli::try_parse_from(["relay", "start", " \t "]).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("goal must not be blank or whitespace-only")
        );
    }

    #[test]
    fn rejects_blank_handoff_agent() {
        let error = Cli::try_parse_from(["relay", "handoff", "\n "]).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("target agent must not be blank or whitespace-only")
        );
    }

    #[test]
    fn history_navigation_defaults_are_preserved() {
        let cli = Cli::try_parse_from(["relay", "history"]).unwrap();

        let Command::History(args) = cli.command else {
            panic!("expected history command");
        };
        assert!(!args.reverse);
        assert_eq!(args.since, None);
        assert_eq!(args.until, None);
        assert_eq!(args.limit, 100);
    }

    #[test]
    fn parses_history_navigation_and_normalizes_offsets_to_utc() {
        let cli = Cli::try_parse_from([
            "relay",
            "history",
            "--reverse",
            "--since",
            "2025-01-02T03:04:05+02:30",
            "--until",
            "2025-01-03T03:04:05-04:00",
        ])
        .unwrap();

        let Command::History(args) = cli.command else {
            panic!("expected history command");
        };
        assert!(args.reverse);
        assert_eq!(
            args.since.unwrap().to_rfc3339(),
            "2025-01-02T00:34:05+00:00"
        );
        assert_eq!(
            args.until.unwrap().to_rfc3339(),
            "2025-01-03T07:04:05+00:00"
        );
    }

    #[test]
    fn rejects_malformed_history_timestamp_during_argument_parsing() {
        let error =
            Cli::try_parse_from(["relay", "history", "--since", "January 2, 2025"]).unwrap_err();
        let message = error.to_string();

        assert!(message.contains("invalid RFC3339 timestamp"));
        assert!(message.contains("2025-01-02T03:04:05Z"));
    }

    #[test]
    fn search_defaults_are_preserved() {
        let cli = Cli::try_parse_from(["relay", "search", "needle"]).unwrap();

        let Command::Search(args) = cli.command else {
            panic!("expected search command");
        };
        assert_eq!(args.query, "needle");
        assert_eq!(args.session, None);
        assert_eq!(args.event_type, None);
        assert_eq!(args.category, None);
        assert!(!args.reverse);
        assert_eq!(args.since, None);
        assert_eq!(args.until, None);
        assert_eq!(args.limit, 100);
    }

    #[test]
    fn parses_every_search_field_and_normalizes_offsets_to_utc() {
        let cli = Cli::try_parse_from([
            "relay",
            "search",
            "payload needle",
            "--session",
            "latest",
            "--type",
            "note",
            "--category",
            "next_action",
            "--reverse",
            "--since",
            "2025-01-02T03:04:05+02:30",
            "--until",
            "2025-01-03T03:04:05-04:00",
            "--limit",
            "7",
        ])
        .unwrap();

        let Command::Search(args) = cli.command else {
            panic!("expected search command");
        };
        assert_eq!(args.query, "payload needle");
        assert_eq!(args.session.as_deref(), Some("latest"));
        assert_eq!(args.event_type.as_deref(), Some("note"));
        assert_eq!(args.category, Some(NoteCategory::NextAction));
        assert!(args.reverse);
        assert_eq!(
            args.since.unwrap().to_rfc3339(),
            "2025-01-02T00:34:05+00:00"
        );
        assert_eq!(
            args.until.unwrap().to_rfc3339(),
            "2025-01-03T07:04:05+00:00"
        );
        assert_eq!(args.limit, 7);
    }

    #[test]
    fn rejects_blank_search_query() {
        let error = Cli::try_parse_from(["relay", "search", " \t "]).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("search query must not be blank or whitespace-only")
        );
    }

    #[test]
    fn rejects_malformed_search_timestamp_during_argument_parsing() {
        let error =
            Cli::try_parse_from(["relay", "search", "needle", "--until", "tomorrow"]).unwrap_err();
        let message = error.to_string();

        assert!(message.contains("invalid RFC3339 timestamp"));
        assert!(message.contains("2025-01-02T03:04:05Z"));
    }

    #[test]
    fn completion_scripts_are_nonempty_and_include_current_commands() {
        for shell in [Shell::Bash, Shell::Zsh, Shell::Fish] {
            let mut command = Cli::command();
            let mut script = Vec::new();
            generate(shell, &mut command, "relay", &mut script);
            let script = String::from_utf8(script).unwrap();

            assert!(!script.is_empty());
            for command in [
                "start",
                "doctor",
                "context",
                "search",
                "protocol",
                "supervise",
                "completions",
            ] {
                assert!(script.contains(command), "{shell:?} omitted {command}");
            }
        }
    }

    #[test]
    fn supervise_preserves_literal_trailing_argv() {
        let cli = Cli::try_parse_from([
            "relay",
            "supervise",
            "--timeout-ms",
            "9",
            "--",
            "/usr/bin/printf",
            "--looks-like-an-option",
            "literal;$(not-a-shell)",
        ])
        .unwrap();
        let Command::Supervise(args) = cli.command else {
            panic!("expected supervise");
        };
        assert_eq!(
            args.program_and_args,
            [
                "/usr/bin/printf",
                "--looks-like-an-option",
                "literal;$(not-a-shell)"
            ]
        );
        assert_eq!(args.timeout_ms, 9);
    }

    #[test]
    fn supervise_requires_separator_for_program_argv() {
        assert!(Cli::try_parse_from(["relay", "supervise", "/usr/bin/printf"]).is_err());
    }
    #[test]
    fn parses_structured_handoff_actions_and_context_selector() {
        let cli = Cli::try_parse_from([
            "relay",
            "handoff",
            "create",
            "--to",
            "agent-b",
            "--from",
            "agent-a",
            "--next-actions",
            "review",
            "--run",
            ".relay/runs/run.json",
        ])
        .unwrap();
        let Command::Handoff(args) = cli.command else {
            panic!("expected handoff create");
        };
        assert_eq!(args.agent, Some("create".into()));
        assert_eq!(args.to.as_deref(), Some("agent-b"));
        assert_eq!(args.source_agent.as_deref(), Some("agent-a"));
        assert_eq!(args.next_actions, ["review"]);
        assert_eq!(
            args.runs,
            [std::path::PathBuf::from(".relay/runs/run.json")]
        );

        let Command::Handoff(args) = Cli::try_parse_from(["relay", "handoff", "--show", "1234"])
            .unwrap()
            .command
        else {
            panic!("expected handoff show");
        };
        assert_eq!(args.agent, None);
        assert_eq!(args.show.as_deref(), Some("1234"));
        assert!(!args.latest);
        assert!(!args.list);

        let Command::Handoff(args) = Cli::try_parse_from(["relay", "handoff", "--latest"])
            .unwrap()
            .command
        else {
            panic!("expected handoff latest");
        };
        assert!(args.latest);

        let Command::Handoff(args) = Cli::try_parse_from(["relay", "handoff", "--list"])
            .unwrap()
            .command
        else {
            panic!("expected handoff list");
        };
        assert!(args.list);
        let Command::Context(args) =
            Cli::try_parse_from(["relay", "context", "--handoff", "latest"])
                .unwrap()
                .command
        else {
            panic!("expected context");
        };
        assert_eq!(args.handoff.as_deref(), Some("latest"));
    }
    #[test]
    fn positional_handoff_subject_does_not_use_a_reserved_sentinel() {
        let Command::Handoff(args) = Cli::try_parse_from(["relay", "handoff", "__create__"])
            .unwrap()
            .command
        else {
            panic!("expected handoff command");
        };
        assert_eq!(args.agent.as_deref(), Some("__create__"));
    }
    #[test]
    fn rejects_extra_handoff_positionals_before_mutation() {
        assert!(Cli::try_parse_from(["relay", "handoff", "agent-a", "selector"]).is_err());
        assert!(
            Cli::try_parse_from(["relay", "handoff", "create", "--to", "agent-b", "selector"])
                .is_err()
        );
    }
}
