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
            Self::Start(_) => "start",
            Self::Status => "status",
            Self::Checkpoint(_) => "checkpoint",
            Self::Handoff(_) => "handoff",
            Self::Finish => "finish",
            Self::List => "list",
            Self::Show(_) => "show",
            Self::History(_) => "history",
            Self::Resume(_) => "resume",
            Self::Doctor(_) => "doctor",
            Self::Note(_) => "note",
            Self::Context(_) => "context",
            Self::Completions(_) => "completions",
        }
    }
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
    /// Maximum output characters; items are never split.
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
    /// Agent receiving the handoff.
    #[arg(value_parser = parse_agent)]
    pub agent: String,

    /// Work completed during the session.
    #[arg(long = "completed-work")]
    pub completed_work: Vec<String>,

    /// An important decision made during the session.
    #[arg(long)]
    pub decision: Vec<String>,

    /// A failure or blocked item.
    #[arg(long)]
    pub failure: Vec<String>,

    /// Suggested next action for the receiving agent.
    #[arg(long = "next-action")]
    pub next_action: Option<String>,
}

#[derive(Debug, Args)]
pub struct SessionSelectorArgs {
    /// Exact session UUID or an unambiguous UUID prefix.
    pub session: String,
}

#[derive(Debug, Args)]
pub struct HistoryArgs {
    /// Restrict history to a session UUID or unambiguous prefix.
    #[arg(long)]
    pub session: Option<String>,

    /// Restrict history to an event type.
    #[arg(long = "type")]
    pub event_type: Option<String>,

    /// Maximum number of matching events to display.
    #[arg(long, default_value_t = 100)]
    pub limit: usize,
}

fn parse_goal(value: &str) -> Result<String, String> {
    parse_nonblank(value, "goal")
}

fn parse_agent(value: &str) -> Result<String, String> {
    parse_nonblank(value, "target agent")
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
        assert_eq!(args.agent, "omp");
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
    fn completion_scripts_are_nonempty_and_include_current_commands() {
        for shell in [Shell::Bash, Shell::Zsh, Shell::Fish] {
            let mut command = Cli::command();
            let mut script = Vec::new();
            generate(shell, &mut command, "relay", &mut script);
            let script = String::from_utf8(script).unwrap();

            assert!(!script.is_empty());
            for command in ["start", "doctor", "context", "completions"] {
                assert!(script.contains(command), "{shell:?} omitted {command}");
            }
        }
    }
}
