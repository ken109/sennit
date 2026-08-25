mod manifest;
mod plan;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use manifest::Manifest;
use plan::{Plan, State};
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(
    name = "sennit",
    version,
    about = "Dotfiles manager that keeps symlink semantics"
)]
struct Cli {
    /// リポジトリルート(既定: sennit.toml を上方向に探索)
    #[arg(long, global = true)]
    root: Option<PathBuf>,

    /// 配置先(既定: $HOME)
    #[arg(long, global = true)]
    home: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// symlink を配置する
    Apply {
        /// 実際には変更せず、何をするかだけ表示する
        #[arg(long)]
        dry_run: bool,
    },
    /// 適用したときに何が変わるかを表示する
    Diff,
    /// 配置状況を一覧する
    List {
        /// 差分のあるものだけ表示する
        #[arg(long)]
        changed: bool,
    },
}

fn main() {
    if let Err(e) = run() {
        eprintln!("\x1b[31merror\x1b[0m: {e:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let root = match &cli.root {
        Some(p) => p.clone(),
        None => find_root().context("could not locate sennit.toml")?,
    };
    let home = match &cli.home {
        Some(p) => p.clone(),
        None => PathBuf::from(std::env::var("HOME").context("HOME is not set")?),
    };

    let manifest = Manifest::load(&root.join("sennit.toml"))?;
    let plan = Plan::build(&root, &home, &manifest)?;

    match cli.command {
        Command::Apply { dry_run } => apply(&plan, dry_run),
        Command::Diff => {
            print_diff(&plan);
            Ok(())
        }
        Command::List { changed } => {
            print_list(&plan, changed);
            Ok(())
        }
    }
}

/// カレントディレクトリから上方向に sennit.toml を探す。
fn find_root() -> Result<PathBuf> {
    let mut dir = std::env::current_dir()?;
    loop {
        if dir.join("sennit.toml").is_file() {
            return Ok(dir);
        }
        if !dir.pop() {
            bail!("sennit.toml not found in any parent directory");
        }
    }
}

fn apply(plan: &Plan, dry_run: bool) -> Result<()> {
    let changes: Vec<_> = plan.changes().collect();
    if changes.is_empty() {
        println!("already up to date ({} links)", plan.entries.len());
        return Ok(());
    }

    for e in &changes {
        let verb = match &e.state {
            State::Missing => "link",
            State::Wrong { .. } => "relink",
            State::Occupied => "replace",
            State::Linked => unreachable!(),
        };
        println!("  {:>8}  {}", verb, e.rel.display());

        if dry_run {
            continue;
        }

        if let Some(parent) = e.dest.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        remove_dest(&e.dest, &e.state)?;
        std::os::unix::fs::symlink(&e.src, &e.dest)
            .with_context(|| format!("failed to link {}", e.dest.display()))?;
    }

    if dry_run {
        println!("\n{} change(s), nothing written (--dry-run)", changes.len());
    } else {
        println!("\n{} link(s) updated", changes.len());
    }
    Ok(())
}

/// 既存の dest を退ける。symlink でない実体は消す前に種別を確かめる。
fn remove_dest(dest: &Path, state: &State) -> Result<()> {
    match state {
        State::Missing => Ok(()),
        State::Wrong { .. } => std::fs::remove_file(dest)
            .with_context(|| format!("failed to remove symlink {}", dest.display())),
        State::Occupied => {
            let meta = std::fs::symlink_metadata(dest)?;
            if meta.is_dir() {
                std::fs::remove_dir_all(dest)
            } else {
                std::fs::remove_file(dest)
            }
            .with_context(|| format!("failed to remove {}", dest.display()))
        }
        State::Linked => Ok(()),
    }
}

fn print_diff(plan: &Plan) {
    let changes: Vec<_> = plan.changes().collect();
    if changes.is_empty() {
        println!("no changes ({} links already in place)", plan.entries.len());
        return;
    }
    for e in &changes {
        match &e.state {
            State::Missing => println!("\x1b[32m+\x1b[0m {}", e.rel.display()),
            State::Wrong { current } => println!(
                "\x1b[33m~\x1b[0m {}\n    now -> {}\n    new -> {}",
                e.rel.display(),
                current.display(),
                e.src.display()
            ),
            State::Occupied => println!(
                "\x1b[31m!\x1b[0m {}  (not a symlink; would be replaced)",
                e.rel.display()
            ),
            State::Linked => {}
        }
    }
    println!("\n{} change(s)", changes.len());
}

fn print_list(plan: &Plan, changed_only: bool) {
    let width = plan
        .entries
        .iter()
        .map(|e| e.rel.as_os_str().len())
        .max()
        .unwrap_or(6)
        .max(6);

    for e in &plan.entries {
        if changed_only && !e.state.needs_change() {
            continue;
        }
        let mark = match e.state {
            State::Linked => "\x1b[32mok\x1b[0m",
            State::Missing => "\x1b[33m--\x1b[0m",
            State::Wrong { .. } => "\x1b[33m~~\x1b[0m",
            State::Occupied => "\x1b[31m!!\x1b[0m",
        };
        println!(
            "{}  {:<width$}  -> {}",
            mark,
            e.rel.display(),
            e.dest.display(),
            width = width
        );
    }

    let n = plan.entries.len();
    let c = plan.changes().count();
    println!("\n{n} link(s), {c} need change");
}
