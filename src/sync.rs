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
        Manager::None | Manager::Unknown => "-",
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
    match manager {
        Manager::Brew => run("brew", &["install"], names),
        Manager::BrewCask => run("brew", &["install", "--cask"], names),
        Manager::Mise => run("mise", &["use", "-g"], names),
        // yay は root で実行すると自分で拒否するので昇格しない
        Manager::Yay => run("yay", &["-S", "--noconfirm"], names),
        Manager::Apt => {
            let esc = Escalation::detect()?;
            // パッケージ一覧が古いと "Unable to locate package" で落ちる
            esc.run("apt-get", &["update"], &[])?;
            esc.run("apt-get", &["install", "-y"], names)
        }
        _ => Ok(()),
    }
}

fn run(bin: &str, base: &[&str], names: &[&str]) -> Result<()> {
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

/// root 権限の取り方。apt のようにシステムを触るマネージャで使う。
///
/// 素朴に sudo を呼ぶと、パスワードが必要でかつ端末が無い環境
/// (CI、スクリプト経由、コンテナのビルド中)で止まるか、分かりにくい形で
/// 失敗する。呼ぶ前に判定して、無理なら理由を添えて落とす。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Escalation {
    /// 既に root。sudo を挟まない
    None,
    Sudo,
}

impl Escalation {
    fn detect() -> Result<Self> {
        if is_root() {
            return Ok(Escalation::None);
        }
        if !which("sudo") {
            bail!("apt requires root, but this is not root and sudo is not installed");
        }
        // パスワード不要ならそのまま使える
        if sudo_is_passwordless() {
            return Ok(Escalation::Sudo);
        }
        // 端末があれば sudo が自分で聞けるので任せる
        if std::io::IsTerminal::is_terminal(&std::io::stdin()) {
            return Ok(Escalation::Sudo);
        }
        bail!(
            "apt requires root: sudo needs a password and there is no terminal to ask on.\n\
             Run sennit from a terminal, run it as root, or allow passwordless sudo for apt-get."
        )
    }

    /// 実行時に前置するコマンド。root なら何も挟まない。
    fn prefix(self) -> Option<&'static str> {
        match self {
            Escalation::None => None,
            Escalation::Sudo => Some("sudo"),
        }
    }

    fn run(self, bin: &str, base: &[&str], names: &[&str]) -> Result<()> {
        match self.prefix() {
            None => run(bin, base, names),
            Some(prefix) => {
                let mut args: Vec<&str> = vec![bin];
                args.extend_from_slice(base);
                run(prefix, &args, names)
            }
        }
    }
}

fn is_root() -> bool {
    // getuid を呼ばずに済ませる。root のときだけ存在が保証される値ではないが、
    // id -u は POSIX で必ず使える。
    Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "0")
        .unwrap_or(false)
}

fn sudo_is_passwordless() -> bool {
    // 失敗時に sudo が "a password is required" を出すが、ここでは判定に
    // 使うだけなので利用者には見せない
    Command::new("sudo")
        .args(["-n", "true"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn which(bin: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|d| d.join(bin).is_file()))
        .unwrap_or(false)
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

    /// root なら sudo を挟まない。コンテナのビルド中など root で走る場面がある。
    #[test]
    fn root_does_not_use_sudo() {
        assert_eq!(Escalation::None.prefix(), None);
    }

    #[test]
    fn non_root_uses_sudo() {
        assert_eq!(Escalation::Sudo.prefix(), Some("sudo"));
    }
}
