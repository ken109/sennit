use crate::packages::{Installable, Manager, Packages};
use anyhow::{bail, Context, Result};
use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;

/// packages.toml の宣言をもとに、未導入のものだけを入れる。
///
/// 冪等性は「入っているかを先に問い合わせ、差分だけ install する」ことで
/// 得る。マネージャの install が冪等かどうかに依存しない。
pub fn sync(root: &Path, dry_run: bool) -> Result<()> {
    let packages = Packages::load(&root.join("packages.toml"))?;
    let wanted = packages.installable();

    let mut planned = 0usize;
    for manager in [Manager::Brew, Manager::BrewCask, Manager::Mise] {
        let names: Vec<&Installable> = wanted.iter().filter(|i| i.manager == manager).collect();
        if names.is_empty() {
            continue;
        }

        let installed = list_installed(manager)?;
        let missing: Vec<&str> = names
            .iter()
            .map(|i| i.name.as_str())
            .filter(|n| !installed.contains(*n))
            .collect();

        println!(
            "{}: {} declared, {} missing",
            label(manager),
            names.len(),
            missing.len()
        );
        for m in &missing {
            println!("  \x1b[33m+\x1b[0m {m}");
        }
        planned += missing.len();

        if missing.is_empty() || dry_run {
            continue;
        }
        install(manager, &missing)?;
    }

    if dry_run {
        println!("\n{planned} package(s) would be installed (--dry-run)");
    } else if planned == 0 {
        println!("\nall declared packages are already installed");
    } else {
        println!("\n{planned} package(s) installed");
    }
    Ok(())
}

fn label(m: Manager) -> &'static str {
    match m {
        Manager::Brew => "brew",
        Manager::BrewCask => "brew --cask",
        Manager::Mise => "mise",
        Manager::ZedExtension => "zed",
        Manager::None => "-",
    }
}

/// 導入済みの一覧を取る。マネージャが無い環境では空集合を返さず落とす。
/// 黙って「全部未導入」として大量に install を走らせるより安全。
fn list_installed(manager: Manager) -> Result<BTreeSet<String>> {
    let (bin, args): (&str, &[&str]) = match manager {
        Manager::Brew => ("brew", &["list", "--formula", "-1"]),
        Manager::BrewCask => ("brew", &["list", "--cask", "-1"]),
        Manager::Mise => ("mise", &["ls", "--installed", "--no-header"]),
        _ => return Ok(BTreeSet::new()),
    };

    let out = Command::new(bin)
        .args(args)
        .output()
        .with_context(|| format!("failed to run `{bin}`; is it installed?"))?;
    if !out.status.success() {
        bail!(
            "`{bin} {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }

    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.split_whitespace().next())
        .map(|s| s.to_string())
        .collect())
}

fn install(manager: Manager, names: &[&str]) -> Result<()> {
    let (bin, base): (&str, &[&str]) = match manager {
        Manager::Brew => ("brew", &["install"]),
        Manager::BrewCask => ("brew", &["install", "--cask"]),
        Manager::Mise => ("mise", &["use", "-g"]),
        _ => return Ok(()),
    };

    let status = Command::new(bin)
        .args(base)
        .args(names)
        .status()
        .with_context(|| format!("failed to run `{bin}`"))?;
    if !status.success() {
        bail!("`{bin} {}` failed", base.join(" "));
    }
    Ok(())
}
