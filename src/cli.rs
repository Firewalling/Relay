use clap::{Args, Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "relay", version, about = "Local-first terminal work handoffs")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
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
}
