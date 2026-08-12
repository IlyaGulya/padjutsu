use clap::Parser;
use clap::Subcommand;

#[derive(Debug, Subcommand, PartialEq)]
pub(crate) enum ControlCommand {
    /// Rumble the controller
    Rumble {
        /// The controller ID to rumble
        #[clap(short, long)]
        id: Option<u32>,
        /// The duration of the rumble in milliseconds
        #[clap(short, long)]
        ms: u32,
    },
}

#[allow(clippy::enum_variant_names)]
#[derive(Debug, Subcommand, PartialEq)]
pub(crate) enum Command {
    /// Run the daemon in the foreground.
    Run {
        /// The profile to run
        #[clap(short, long)]
        workspace: Option<String>,
    },
    /// Start daemon in the background.
    Start {
        /// The directory containing the profile
        #[clap(short, long)]
        workspace: Option<String>,
    },
    /// Stop the daemon.
    Stop,
    /// Show the status of the daemon.
    Status,
    /// Observe the daemon's events.
    Observe,
    /// Read or mark the persistent production metrics flight recorder.
    Metrics {
        /// Show this many minutes ending now (ignored with --at).
        #[arg(long, default_value_t = 15)]
        since_minutes: u64,
        /// Center the window on a local time or RFC3339 timestamp.
        #[arg(long)]
        at: Option<String>,
        /// Minutes before and after --at.
        #[arg(long, default_value_t = 2)]
        window_minutes: u64,
        /// Show only snapshots with a recognized incident signature.
        #[arg(long)]
        incidents_only: bool,
        /// Persist a human incident marker instead of reading metrics.
        #[arg(long)]
        mark: Option<String>,
    },
    /// Send a command to the daemon.
    Command {
        /// The workspace to send the command to
        #[clap(short, long)]
        workspace: Option<String>,
        /// The command to send
        #[clap(subcommand)]
        command: ControlCommand,
    },
}

/// Highly effective conversion of a gamepad into a macropad for applications.
#[derive(Parser)]
#[command(version, about, long_about = None)]
pub(crate) struct Cli {
    /// Turn debugging information on
    #[arg(short, long)]
    pub verbose: bool,

    /// Disable colored output
    #[arg(long)]
    pub no_color: bool,

    /// The command to run
    #[clap(subcommand)]
    pub command: Command,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_metrics_time_window() {
        let cli = Cli::try_parse_from([
            "padjutsud",
            "metrics",
            "--at",
            "2026-08-12 16:30:00",
            "--window-minutes",
            "3",
            "--incidents-only",
        ])
        .expect("parse metrics query");

        assert_eq!(
            cli.command,
            Command::Metrics {
                since_minutes: 15,
                at: Some("2026-08-12 16:30:00".into()),
                window_minutes: 3,
                incidents_only: true,
                mark: None,
            }
        );
    }

    #[test]
    fn parses_incident_marker() {
        let cli = Cli::try_parse_from([
            "padjutsud",
            "metrics",
            "--mark",
            "mouse stuck after load spike",
        ])
        .expect("parse marker");

        assert!(matches!(
            cli.command,
            Command::Metrics {
                mark: Some(ref marker),
                ..
            } if marker == "mouse stuck after load spike"
        ));
    }
}
