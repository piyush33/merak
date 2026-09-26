use anyhow::Result;
use clap::{Parser, Subcommand};
use merak_cli::{model, source};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "merak", version, about = "Semantic transitions for code changes")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Print the Program Behaviour Model of a source tree as JSON.
    Model {
        /// Directory to analyze.
        #[arg(default_value = ".")]
        dir: PathBuf,
    },
    /// Derive the semantic transition between two versions.
    ///
    /// Either `merak diff <base>..<head>` (git revisions of --repo), or
    /// `merak diff --before <dir> --after <dir>`.
    Diff {
        /// Revision range `base..head` (head defaults to the working tree when omitted: `base..`).
        range: Option<String>,
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        /// Analyze only this subdirectory of the repository (git mode).
        #[arg(long, default_value = "")]
        root: String,
        #[arg(long)]
        before: Option<PathBuf>,
        #[arg(long)]
        after: Option<PathBuf>,
        /// Compare a snapshot (from `merak snapshot`) with the working tree of --repo.
        #[arg(long)]
        since: Option<PathBuf>,
        /// Emit JSON instead of Markdown.
        #[arg(long)]
        json: bool,
    },
    /// Save the analyzable sources of a directory, to diff against later with `diff --since`.
    Snapshot {
        #[arg(default_value = ".")]
        dir: PathBuf,
        #[arg(long)]
        out: PathBuf,
    },
    /// Run the MCP server (stdio) for coding agents.
    Mcp,
    /// Claude Code hook: `prompt` (UserPromptSubmit) snapshots, `stop` (Stop) reports this turn's transition.
    Hook {
        event: String,
        /// Directory for per-session snapshots (the plugin passes `${CLAUDE_PLUGIN_DATA}`).
        #[arg(long)]
        data: PathBuf,
    },
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Model { dir } => {
            let m = model(&source::from_dir(&dir)?)?;
            println!("{}", serde_json::to_string_pretty(&m)?);
        }
        Cmd::Snapshot { dir, out } => {
            std::fs::write(&out, serde_json::to_vec(&source::from_dir(&dir)?)?)?;
        }
        Cmd::Mcp => merak_cli::mcp::serve()?,
        Cmd::Hook { event, data } => std::process::exit(merak_cli::hook::run(&event, &data)),
        Cmd::Diff { range, repo, root, before, after, since, json } => {
            let t = std::time::Instant::now();
            let (a, b, label) = match (range, before, after, since) {
                (_, _, _, Some(snap)) => (source::from_snapshot(&snap)?, source::from_dir(&repo.join(&root))?, "since snapshot".to_string()),
                (_, Some(x), Some(y), None) => (source::from_dir(&x)?, source::from_dir(&y)?, format!("{} → {}", x.display(), y.display())),
                (Some(r), None, None, None) => {
                    let (base, head) = r.split_once("..").unwrap_or((r.as_str(), ""));
                    let (a, b) = merak_cli::load_range(&repo, base, head, &root)?;
                    (a, b, r.clone())
                }
                _ => anyhow::bail!("give a revision range `base..head`, both --before and --after, or --since <snapshot>"),
            };
            merak_cli::timing("load", t);
            let t = merak_cli::diff(&a, &b)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&t)?);
            } else {
                print!("{}", merak_transition::render::markdown(&t, Some(&label)));
            }
        }
    }
    Ok(())
}
