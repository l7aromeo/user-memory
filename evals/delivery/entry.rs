//! The delivery benchmark's way into the matcher (`src/memory/delivery.rs`),
//! which no shipped command calls yet. `just delivery-bench` builds it as
//! `user-memory-bench`; releases build only `--bin user-memory`, so it never
//! ships.
//!
//! It compiles the matcher and the modules it uses from their own files,
//! through a `#[path]` mirror of `src/memory`, so they keep their
//! `crate::memory::…` paths. A mirrored file that starts using a module
//! outside the mirror breaks this binary's build, which `just check` builds
//! (`--all-targets`); see docs/dev/development.md.
//!
//! - `index`: builds a store's delivery index for a scope into a file.
//! - `match`: runs tasks' actions through a window, one action at a time,
//!   and prints each action's matched passages as JSON lines.
//! - `lookup`: loads the index file and matches one action, as one hook
//!   call would; with `--db`, also cuts the passing passages' text from the
//!   store.

#![deny(unsafe_code)]

#[allow(
    dead_code,
    reason = "the mirrored modules are the shipped binary's; this binary uses part of them"
)]
#[path = "../../src/memory"]
mod memory {
    pub mod delivery;
    pub mod words;
    pub mod index {
        pub mod passages;
        pub mod stems;
        pub mod weights;
    }
}

use clap::{Parser, Subcommand};
use memory::delivery::{self, Action, DeliveryIndex, Kind, Matched, Tuning, Window};
use rusqlite::{Connection, OpenFlags};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    fs,
    io::{self, BufWriter, Write},
    path::PathBuf,
    process::ExitCode,
};

#[derive(Parser)]
#[command(about = "The delivery benchmark's entry into the matcher (development only)")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Build the delivery index of a store's global memories and PROJECTS'.
    Index {
        #[arg(long)]
        db: PathBuf,
        #[arg(long = "project")]
        projects: Vec<String>,
        #[arg(long)]
        out: PathBuf,
    },
    /// Match every action of the tasks in TASKS (a JSON array of
    /// `{"id", "actions": [{"kind", "text", "failed"}]}`), each in its own
    /// window, printing one JSON line per action.
    Match {
        #[arg(long)]
        index: PathBuf,
        #[arg(long)]
        tasks: PathBuf,
        /// The strongest matches printed per action.
        #[arg(long, default_value_t = 40)]
        top: usize,
        #[command(flatten)]
        tuning: TuningArgs,
    },
    /// Load the index and match ACTION (`{"kind", "text", "failed"}`), as
    /// one hook call would.
    Lookup {
        #[arg(long)]
        index: PathBuf,
        #[arg(long)]
        action: String,
        /// Also cut the passing passages' text from this store.
        #[arg(long)]
        db: Option<PathBuf>,
    },
}

/// Settings to measure in place of the matcher's constants.
#[derive(clap::Args)]
struct TuningArgs {
    /// Actions the window keeps (WINDOW_ACTIONS).
    #[arg(long)]
    window: Option<usize>,
    /// Actions after which a term's weight halves (WINDOW_HALF_LIFE).
    #[arg(long)]
    half_life: Option<f64>,
    /// Share of a term only other passages hold (OTHER_PASSAGE_CREDIT).
    #[arg(long)]
    credit: Option<f64>,
}

impl TuningArgs {
    fn tuning(&self) -> Tuning {
        let base = Tuning::default();
        Tuning {
            window_actions: self.window.unwrap_or(base.window_actions),
            half_life: self.half_life.unwrap_or(base.half_life),
            credit: self.credit.unwrap_or(base.credit),
        }
    }
}

/// An action as the benchmark writes it. Unknown fields, such as a tool's
/// result, are ignored: nothing but the kind, the text and the outcome
/// joins the window.
#[derive(Deserialize)]
struct ActionInput {
    kind: String,
    text: String,
    #[serde(default)]
    failed: bool,
}

#[derive(Deserialize)]
struct Task {
    id: String,
    actions: Vec<ActionInput>,
}

fn action(input: &ActionInput) -> Result<Action, String> {
    let kind = match input.kind.as_str() {
        "prompt" => Kind::Prompt,
        "task" => Kind::Task,
        "plan" => Kind::Plan,
        "command" => Kind::Command,
        "file" => Kind::File,
        "message" => Kind::Message,
        "search" => Kind::Search,
        other => return Err(format!("unknown action kind {other:?}")),
    };
    Ok(Action {
        kind,
        text: input.text.clone(),
        failed: input.failed,
    })
}

fn matched(index: &DeliveryIndex, m: &Matched) -> Value {
    json!({
        "id": index.id(m.memory),
        "slot": m.slot,
        "start": m.start,
        "length": m.length,
        "score": (m.score * 1e4).round() / 1e4,
        "holders": (m.holders * 1e4).round() / 1e4,
        "root": (m.root * 1e4).round() / 1e4,
        "mixed": (m.mixed * 1e4).round() / 1e4,
        "mixed_log": (m.mixed_log * 1e4).round() / 1e4,
        "share": (m.share * 1e4).round() / 1e4,
        "fewest": m.fewest,
        "fired": m.fired,
        "failed": m.failed,
        "anchored": m.anchored,
        "command": m.command,
        "push": m.push(),
        "failure": m.failure(),
        "hold": m.hold(),
    })
}

fn run(cli: Cli) -> Result<(), String> {
    match cli.command {
        Command::Index { db, projects, out } => {
            let sources = delivery::sources(&db, &projects).map_err(|e| e.to_string())?;
            let bytes = DeliveryIndex::write(&sources).map_err(|e| e.to_string())?;
            fs::write(&out, &bytes).map_err(|e| format!("{}: {e}", out.display()))?;
            println!(
                "{}",
                json!({"memories": sources.len(), "bytes": bytes.len()})
            );
        }
        Command::Match {
            index,
            tasks,
            top,
            tuning,
        } => {
            let index = DeliveryIndex::load(&index).map_err(|e| e.to_string())?;
            let text =
                fs::read_to_string(&tasks).map_err(|e| format!("{}: {e}", tasks.display()))?;
            let tasks: Vec<Task> = serde_json::from_str(&text).map_err(|e| e.to_string())?;
            let mut out = BufWriter::new(io::stdout().lock());
            for task in &tasks {
                let mut window = Window::with(tuning.tuning());
                for (i, input) in task.actions.iter().enumerate() {
                    window.push(&action(input)?);
                    let found = index.rank(&window).map_err(|e| e.to_string())?;
                    let shown: Vec<Value> =
                        found.iter().take(top).map(|m| matched(&index, m)).collect();
                    writeln!(
                        out,
                        "{}",
                        json!({"task": task.id, "action": i, "matched": shown})
                    )
                    .map_err(|e| e.to_string())?;
                }
            }
            out.flush().map_err(|e| e.to_string())?;
        }
        Command::Lookup {
            index,
            action: input,
            db,
        } => {
            let index = DeliveryIndex::load(&index).map_err(|e| e.to_string())?;
            let input: ActionInput = serde_json::from_str(&input).map_err(|e| e.to_string())?;
            let mut window = Window::default();
            window.push(&action(&input)?);
            let found = index.rank(&window).map_err(|e| e.to_string())?;
            let passing: Vec<&Matched> = found
                .iter()
                .filter(|m| m.push() || m.failure() || m.hold())
                .collect();
            let conn = db
                .map(|db| Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY))
                .transpose()
                .map_err(|e| e.to_string())?;
            let mut shown = Vec::new();
            for m in passing {
                let mut value = matched(&index, m);
                if let Some(conn) = &conn {
                    value["text"] = json!(
                        delivery::text(conn, index.id(m.memory), m).map_err(|e| e.to_string())?
                    );
                }
                shown.push(value);
            }
            println!("{}", json!({"matched": found.len(), "passing": shown}));
        }
    }
    Ok(())
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("user-memory-bench: {e}");
            ExitCode::FAILURE
        }
    }
}
