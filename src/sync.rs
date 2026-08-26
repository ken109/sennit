use crate::packages::{Installable, Manager, Packages};
use anyhow::{bail, Context, Result};
use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;

/// packages.toml の宣言をもとに、未導入のものだけを入れる。
///
/// 冪等性は「入っているかを先に問い合わせ、差分だけ install する」ことで
/// 得る。マネージャの install が冪等かどうかに依存しない。
/// マネージャを実行する順序。土台になるものから先に入れる。
const ORDER: [Manager; 5] = [
    Manager::Apt,
    Manager::Yay,
    Manager::Brew,
    Manager::BrewCask,
    Manager::Mise,
];

pub fn sync(root: &Path, dry_run: bool) -> Result<()> {
    let packages = Packages::load(&root.join("packages.toml"))?;
    let wanted = packages.installable();

    let mut planned = 0usize;
    for manager in ORDER {
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
        Manager::Apt => "apt",
        Manager::Yay => "yay",
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
        // dpkg-query は導入済みのみを ok として出す
        Manager::Apt => (
            "dpkg-query",
            &["-W", "-f=${binary:Package} ${db:Status-Status}\n"],
        ),
        Manager::Yay => ("pacman", &["-Qq"]),
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

    let text = String::from_utf8_lossy(&out.stdout);
    Ok(match manager {
        // dpkg は削除済みのパッケージも列挙するので installed だけ拾う。
        // アーキ修飾(pkg:amd64)も落とす。
        Manager::Apt => text
            .lines()
            .filter(|l| l.ends_with(" installed"))
            .filter_map(|l| l.split_whitespace().next())
            .map(|n| n.split(':').next().unwrap_or(n).to_string())
            .collect(),
        _ => text
            .lines()
            .filter_map(|l| l.split_whitespace().next())
            .map(|s| s.to_string())
            .collect(),
    })
}

fn install(manager: Manager, names: &[&str]) -> Result<()> {
    let (bin, base): (&str, &[&str]) = match manager {
        Manager::Brew => ("brew", &["install"]),
        Manager::BrewCask => ("brew", &["install", "--cask"]),
        Manager::Mise => ("mise", &["use", "-g"]),
        Manager::Apt => ("sudo", &["apt-get", "install", "-y"]),
        Manager::Yay => ("yay", &["-S", "--noconfirm"]),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn pos(m: Manager) -> usize {
        ORDER.iter().position(|x| *x == m).unwrap()
    }

    /// ディストリのパッケージが土台で、その上に Homebrew が乗る。
    /// font-cica の cask は展開に unzip を要求し、それを入れるのは apt なので
    /// この順序が崩れると Linux の初回インストールが落ちる。
    #[test]
    fn distro_packages_are_installed_before_homebrew() {
        assert!(pos(Manager::Apt) < pos(Manager::Brew));
        assert!(pos(Manager::Apt) < pos(Manager::BrewCask));
        assert!(pos(Manager::Yay) < pos(Manager::Brew));
    }

    /// mise は brew で入るので brew より後。
    #[test]
    fn mise_runs_after_the_manager_that_installs_it() {
        assert!(pos(Manager::Brew) < pos(Manager::Mise));
    }

    /// cask は formula より後(unzip などの依存が formula 側にもあるため)。
    #[test]
    fn casks_run_after_formulae() {
        assert!(pos(Manager::Brew) < pos(Manager::BrewCask));
    }
}
