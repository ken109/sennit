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
const ORDER: [Manager; 6] = [
    Manager::Apt,
    Manager::Yay,
    // Windows の土台。mise も winget で入る
    Manager::Winget,
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
        // winget には「一覧を 1 回で取る」安定した手段が無い(表は列が切り詰められる)。
        // 1 件ずつ訊き、upgrade の宣言も扱うので別に回す
        if manager == Manager::Winget {
            planned += winget::sync(&names, dry_run)?;
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
        Manager::Winget => "winget",
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
    // 実際に走らせるコマンドで確かめる。`sudo -n true` を試すと、
    // `%sudo ALL=(ALL) NOPASSWD: /usr/bin/apt-get` のように対象を絞った
    // 設定では失敗し、「apt-get を passwordless にしてください」という
    // 既にやってあることを案内して止まる。
    //
    // 失敗時に sudo が "a password is required" を出すが、ここでは判定に
    // 使うだけなので利用者には見せない
    Command::new("sudo")
        .args(["-n", "apt-get", "--version"])
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

/// winget を回す部分。Windows 以外でも型検査とテストが通るよう cfg で分けない。
pub mod winget {
    use super::Installable;
    use anyhow::{bail, Context, Result};
    use std::process::Command;

    /// `winget list` が「入っていない」と答えるときの終了コード
    /// (APPINSTALLER_CLI_ERROR_NO_APPLICATIONS_FOUND)
    pub const NO_APPLICATIONS_FOUND: u32 = 0x8A15_0014;
    // 以下は winget の returnCodes.md の値
    /// upgrade: 新しい版が無い(UPDATE_NOT_APPLICABLE)
    pub const UPDATE_NOT_APPLICABLE: u32 = 0x8A15_002B;
    /// upgrade: 入っている版より新しくない(UPGRADE_VERSION_NOT_NEWER)
    pub const UPGRADE_VERSION_NOT_NEWER: u32 = 0x8A15_004F;
    /// upgrade: 入っている版が分からない(UPGRADE_VERSION_UNKNOWN)
    pub const UPGRADE_VERSION_UNKNOWN: u32 = 0x8A15_0050;
    /// upgrade: pin が掛かっている(PACKAGE_IS_PINNED)
    pub const PACKAGE_IS_PINNED: u32 = 0x8A15_0068;
    /// install: 既にどれかの版が入っている(PACKAGE_ALREADY_INSTALLED)
    pub const PACKAGE_ALREADY_INSTALLED: u32 = 0x8A15_0061;
    /// install: 別の版が既に入っている(INSTALL_ALREADY_INSTALLED)
    pub const INSTALL_ALREADY_INSTALLED: u32 = 0x8A15_010D;
    /// 起動中のため入れられない(INSTALL_PACKAGE_IN_USE)
    pub const INSTALL_PACKAGE_IN_USE: u32 = 0x8A15_0101;
    /// ファイルが使用中(INSTALL_FILE_IN_USE)
    pub const INSTALL_FILE_IN_USE: u32 = 0x8A15_0103;
    /// 再起動で完了する(INSTALL_REBOOT_REQUIRED_TO_FINISH)
    pub const INSTALL_REBOOT_REQUIRED_TO_FINISH: u32 = 0x8A15_0109;

    /// install / upgrade の終了コードの読み方。
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Outcome {
        Done,
        /// 済んだが、再起動で完了する
        Reboot,
        /// 起動中で入れ替えられなかった。閉じてから入れ直せば済む
        InUse,
        Failed,
    }

    /// `upgrade` が真なら upgrade の、偽なら install の終了コード。
    pub fn outcome(code: u32, upgrade: bool) -> Outcome {
        match code {
            0 => Outcome::Done,
            INSTALL_REBOOT_REQUIRED_TO_FINISH => Outcome::Reboot,
            INSTALL_PACKAGE_IN_USE | INSTALL_FILE_IN_USE => Outcome::InUse,
            // 新しい版が無い・比べられない・pin で止めてある: どれも直すものが無い
            UPDATE_NOT_APPLICABLE
            | UPGRADE_VERSION_NOT_NEWER
            | UPGRADE_VERSION_UNKNOWN
            | PACKAGE_IS_PINNED
                if upgrade =>
            {
                Outcome::Done
            }
            // 入れようとしたら既に入っていた: 望んだ状態にはなっている
            PACKAGE_ALREADY_INSTALLED | INSTALL_ALREADY_INSTALLED if !upgrade => Outcome::Done,
            _ => Outcome::Failed,
        }
    }

    /// どのパッケージにも付ける引数。ID の完全一致で、winget のソースだけを見て、
    /// 端末に何も訊かない。
    const COMMON: [&str; 5] = [
        "--exact",
        "--source",
        "winget",
        "--accept-source-agreements",
        "--disable-interactivity",
    ];

    /// 宣言を、このマシンの様子で振り分けたもの。
    #[derive(Debug, Default, PartialEq, Eq)]
    pub struct Sorted<'a> {
        /// 入っていない。入れる
        pub missing: Vec<&'a Installable>,
        /// 入っていて、毎回上げる宣言がある
        pub upgrades: Vec<&'a Installable>,
        /// winget の一覧に無いが、同じコマンドが PATH にある(見つけた場所)
        pub elsewhere: Vec<(&'a Installable, String)>,
    }

    /// 振り分け。`installed` と `on_path` を差し替えられるよう、外から渡す。
    pub fn sort<'a>(
        wanted: &[&'a Installable],
        installed: impl Fn(&str) -> Result<bool>,
        on_path: impl Fn(&[String]) -> Option<String>,
    ) -> Result<Sorted<'a>> {
        let mut out = Sorted::default();
        for i in wanted {
            if installed(&i.name)? {
                if i.upgrade {
                    out.upgrades.push(i);
                }
                continue;
            }
            // winget の一覧に無くても、同じコマンドが PATH にあれば別の手段で
            // 入っている。入れると同じ道具が 2 つ並び、どちらが動くか分からなくなる
            if let Some(found) = on_path(&i.commands) {
                out.elsewhere.push((i, found));
                continue;
            }
            out.missing.push(i);
        }
        Ok(out)
    }

    /// 1 件ずつ見て、入っていないものを入れ、upgrade の宣言があるものは上げる。
    /// 戻り値は入れる(--dry-run なら入れるはずの)件数。
    pub fn sync(wanted: &[&Installable], dry_run: bool) -> Result<usize> {
        let Sorted {
            missing,
            upgrades,
            elsewhere,
        } = sort(wanted, installed, on_path)?;

        println!(
            "winget: {} declared, {} missing",
            wanted.len(),
            missing.len()
        );
        for (i, found) in &elsewhere {
            println!("  \x1b[36m=\x1b[0m {}  (already on PATH: {found})", i.name);
        }
        for i in &missing {
            println!("  \x1b[33m+\x1b[0m {}", i.name);
        }
        for i in &upgrades {
            println!("  \x1b[33m^\x1b[0m {}  (upgrade)", i.name);
        }
        if dry_run {
            return Ok(missing.len());
        }

        let mut failed: Vec<String> = Vec::new();
        let mut reboot = false;
        let jobs = missing
            .iter()
            .map(|i| (i, false))
            .chain(upgrades.iter().map(|i| (i, true)));
        for (i, upgrade) in jobs {
            let verb = if upgrade { "upgrade" } else { "install" };
            let code = run(&[verb, "--id", &i.name, "--accept-package-agreements"])?;
            match outcome(code, upgrade) {
                Outcome::Done => {}
                Outcome::Reboot => reboot = true,
                // PowerShell 7 の中から Microsoft.PowerShell を上げると、自分が
                // 使っているので入れ替えられない。sync の失敗にはしない
                Outcome::InUse => eprintln!(
                    "\x1b[33mwarning\x1b[0m: {} is in use, so the {verb} was skipped; \
                     close it (for PowerShell, run sync from another shell) and run sync again",
                    i.name
                ),
                Outcome::Failed => {
                    failed.push(format!("{} ({verb} exited {})", i.name, describe(code)))
                }
            }
        }
        if reboot {
            eprintln!("\x1b[33mnote\x1b[0m: restart Windows to finish installing");
        }
        if !failed.is_empty() {
            bail!("winget failed for: {}", failed.join(", "));
        }
        Ok(missing.len())
    }

    /// 終了コードを 16 進でも見せる。winget の失敗は 0x8A15.... で引く
    pub fn describe(code: u32) -> String {
        format!("{} / 0x{code:08X}", code as i32)
    }

    /// その ID が入っているか。winget が無い・訊けないときは落とす。
    /// 黙って「入っていない」と読むと、全部を入れ直しにかかる。
    pub fn installed(id: &str) -> Result<bool> {
        let out = Command::new("winget")
            .arg("list")
            .args(["--id", id])
            .args(COMMON)
            .output()
            .context(
                "failed to run `winget`; install App Installer from the Microsoft Store \
                 (winget is part of it)",
            )?;
        let code = exit_code(&out.status);
        match code {
            0 => Ok(true),
            NO_APPLICATIONS_FOUND => Ok(false),
            _ => bail!(
                "`winget list --id {id}` failed with {}: {}",
                describe(code),
                String::from_utf8_lossy(&out.stdout).trim()
            ),
        }
    }

    fn run(args: &[&str]) -> Result<u32> {
        let status = Command::new("winget")
            .args(args)
            .args(COMMON)
            .status()
            .context("failed to run `winget`")?;
        Ok(exit_code(&status))
    }

    /// Windows の終了コードは 32 ビットの符号なし値。i32 で返ってくるので戻す
    fn exit_code(status: &std::process::ExitStatus) -> u32 {
        status.code().map(|c| c as u32).unwrap_or(u32::MAX)
    }

    fn on_path(commands: &[String]) -> Option<String> {
        let paths = std::env::var_os("PATH")?;
        for dir in std::env::split_paths(&paths) {
            for c in commands.iter().filter(|c| !c.is_empty()) {
                if let Some(p) = crate::platform::find_in_dir(&dir, c) {
                    return Some(p.display().to_string());
                }
            }
        }
        None
    }
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

    /// mise は brew で入るので brew より後。Windows では winget が入れる。
    #[test]
    fn mise_runs_after_the_manager_that_installs_it() {
        assert!(pos(Manager::Brew) < pos(Manager::Mise));
        assert!(pos(Manager::Winget) < pos(Manager::Mise));
    }

    /// 「新しい版が無い」は失敗ではない。毎回 upgrade する宣言が、最新の
    /// マシンで毎回 sync を落とさないように。
    #[test]
    fn winget_reporting_no_newer_version_is_not_a_failure() {
        use winget::{outcome, Outcome};
        assert_eq!(outcome(0, true), Outcome::Done);
        for code in [0x8A15_002B, 0x8A15_004F, 0x8A15_0050, 0x8A15_0068] {
            assert_eq!(outcome(code, true), Outcome::Done, "{code:X}");
            // install でこれが出たら、それは失敗
            assert_eq!(outcome(code, false), Outcome::Failed, "{code:X}");
        }
        // PowerShell の $LASTEXITCODE で見える値と同じもの
        assert_eq!(0x8A15_002Bu32 as i32, -1978335189);
        assert_eq!(outcome(1, true), Outcome::Failed);
        assert_eq!(
            outcome(winget::NO_APPLICATIONS_FOUND, true),
            Outcome::Failed
        );
        assert_eq!(winget::describe(0x8A15_002B), "-1978335189 / 0x8A15002B");
    }

    /// 既に入っていた install は成功。再起動待ちは成功、使用中は飛ばす。
    #[test]
    fn winget_install_codes_are_read_by_what_they_mean() {
        use winget::{outcome, Outcome};
        assert_eq!(outcome(0x8A15_0061, false), Outcome::Done);
        assert_eq!(outcome(0x8A15_010D, false), Outcome::Done);
        for upgrade in [true, false] {
            assert_eq!(outcome(0x8A15_0109, upgrade), Outcome::Reboot);
            assert_eq!(outcome(0x8A15_0101, upgrade), Outcome::InUse);
            assert_eq!(outcome(0x8A15_0103, upgrade), Outcome::InUse);
        }
    }

    /// 入っている・PATH にある・入っていないの振り分け。
    #[test]
    fn winget_declarations_are_sorted_by_what_is_on_the_machine() {
        let pkg = |name: &str, upgrade: bool, cmd: &str| Installable {
            name: name.to_string(),
            manager: Manager::Winget,
            upgrade,
            commands: vec![cmd.to_string()],
        };
        let a = pkg("A.Installed", false, "a");
        let b = pkg("B.Upgraded", true, "b");
        let c = pkg("C.Elsewhere", true, "c");
        let d = pkg("D.Missing", false, "d");
        let wanted = vec![&a, &b, &c, &d];
        let got = winget::sort(
            &wanted,
            |id| Ok(id.starts_with('A') || id.starts_with('B')),
            |cmds| (cmds[0] == "c").then(|| r"C:\scoop\shims\c.exe".to_string()),
        )
        .unwrap();
        assert_eq!(got.missing, vec![&d]);
        assert_eq!(got.upgrades, vec![&b]);
        // PATH にあるものは upgrade の宣言があっても winget では触らない
        assert_eq!(
            got.elsewhere,
            vec![(&c, r"C:\scoop\shims\c.exe".to_string())]
        );

        // winget に訊けなければ落とす。黙って「入っていない」と読まない
        let e = winget::sort(&wanted, |_| anyhow::bail!("no winget"), |_| None).unwrap_err();
        assert!(e.to_string().contains("no winget"));
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
