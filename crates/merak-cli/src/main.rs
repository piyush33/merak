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
        #[arg(long)]
        before: Option<PathBuf>,
        #[arg(long)]
        after: Option<PathBuf>,
        /// Emit JSON instead of Markdown.
        #[arg(long)]
        json: bool,
    },
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Model { dir } => {
            let m = model(&source::from_dir(&dir)?)?;
            println!("{}", serde_json::to_string_pretty(&m)?);
        }
        Cmd::Diff { range, repo, before, after, json } => {
            let (a, b, label) = match (range, before, after) {
                (_, Some(x), Some(y)) => (source::from_dir(&x)?, source::from_dir(&y)?, format!("{} → {}", x.display(), y.display())),
                (Some(r), None, None) => {
                    let (base, head) = r.split_once("..").unwrap_or((r.as_str(), ""));
                    let a = source::from_git(&repo, base, "")?;
                    let b = if head.is_empty() { source::from_dir(&repo)? } else { source::from_git(&repo, head, "")? };
                    (a, b, r.clone())
                }
                _ => anyhow::bail!("give a revision range `base..head`, or both --before and --after"),
            };
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
