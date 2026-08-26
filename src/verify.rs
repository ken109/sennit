use crate::packages::{Kind, Manager, Package, Packages};
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

/// 宣言したものが、このマシンで実際に解決できるかを確かめる。
///
/// check が「宣言と設定」を突き合わせるのに対し、こちらは「宣言と現実」を
/// 突き合わせる。formula 名の間違い(gpg は別名で、正式には gnupg)や、
/// インストールが黙って失敗した状態を捕まえる。
///
/// 検証できるのはコマンドとフォントだけ。GUI アプリやライブラリは
/// 「PATH に無い」ことが異常を意味しないので、数だけ報告して飛ばす。
/// verify の結果を機械可読で出す。マシン間の比較に使う。
#[derive(serde::Serialize, serde::Deserialize)]
pub struct Report {
    pub os: String,
    pub profiles: Vec<String>,
    pub present: Vec<String>,
    pub missing: Vec<String>,
    pub unverifiable: Vec<String>,
}

/// 2 つの報告を比べる。どちらのマシンにあって、どちらに無いか。
///
/// verify は 1 台しか見ない。複数マシンを使っていると、片方だけ古い、
/// 片方だけ手で入れた、という差は誰も教えてくれない。
pub fn compare(a: &Path, b: &Path) -> Result<()> {
    let ra: Report = serde_json::from_str(&std::fs::read_to_string(a)?)
        .with_context(|| format!("failed to parse {}", a.display()))?;
    let rb: Report = serde_json::from_str(&std::fs::read_to_string(b)?)
        .with_context(|| format!("failed to parse {}", b.display()))?;

    let sa: std::collections::BTreeSet<_> = ra.present.iter().collect();
    let sb: std::collections::BTreeSet<_> = rb.present.iter().collect();

    let only_a: Vec<_> = sa.difference(&sb).collect();
    let only_b: Vec<_> = sb.difference(&sa).collect();

    println!("{} ({}) vs {} ({})", a.display(), ra.os, b.display(), rb.os);
    println!("  {} present on both", sa.intersection(&sb).count());

    if only_a.is_empty() && only_b.is_empty() {
        println!("\x1b[32mok\x1b[0m  the two machines agree");
        return Ok(());
    }
    if !only_a.is_empty() {
        println!("\nonly on {} ({}):", a.display(), ra.os);
        for n in only_a {
            println!("  \x1b[33m<\x1b[0m {n}");
        }
    }
    if !only_b.is_empty() {
        println!("\nonly on {} ({}):", b.display(), rb.os);
        for n in only_b {
            println!("  \x1b[33m>\x1b[0m {n}");
        }
    }
    Ok(())
}

pub fn verify(root: &Path, export: Option<PathBuf>) -> Result<()> {
    let packages = Packages::load(&root.join("packages.toml"))?;

    let mut missing = Vec::new();
    let mut present = Vec::new();
    let mut unverifiable = Vec::new();
    let mut checked = 0usize;
    let mut skipped = 0usize;

    for (name, pkg) in packages.applicable() {
        match verifiable(&name, pkg) {
            None => {
                skipped += 1;
                unverifiable.push(name.clone());
            }
            Some(candidates) => {
                checked += 1;
                // provides のどれか 1 つでも解決すればよい。
                // neovim は nvim として入るので、パッケージ名では見つからない。
                if candidates.iter().any(|(kind, n)| resolves(*kind, n)) {
                    present.push(name.clone());
                } else {
                    let shown = candidates
                        .iter()
                        .map(|(_, n)| n.as_str())
                        .collect::<Vec<_>>()
                        .join(" / ");
                    missing.push((name.clone(), shown));
                }
            }
        }
    }

    missing.sort();

    if let Some(out) = export {
        let report = Report {
            os: crate::packages::current_os().to_string(),
            profiles: crate::packages::current_profiles(),
            present: present.clone(),
            missing: missing.iter().map(|(p, _)| p.clone()).collect(),
            unverifiable: unverifiable.clone(),
        };
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        std::fs::write(&out, serde_json::to_string_pretty(&report)?)
            .with_context(|| format!("failed to write {}", out.display()))?;
        println!("wrote {}", out.display());
    }

    println!("verified {checked} package(s) against this machine ({skipped} not verifiable)");
    for (pkg, names) in &missing {
        println!("  \x1b[31mmissing\x1b[0m  {pkg}  (looked for: {names})");
    }

    if missing.is_empty() {
        println!("\x1b[32mok\x1b[0m  everything verifiable is present");
        return Ok(());
    }
    bail!(
        "{} declared package(s) not found on this machine",
        missing.len()
    )
}

/// このパッケージについて「存在すればこれが見つかるはず」という名前の一覧。
/// 検証しようがないものは None。
fn verifiable(name: &str, pkg: &Package) -> Option<Vec<(Kind, String)>> {
    let kind = pkg.kind_of();
    match kind {
        // 拡張はエディタが入れるので PATH には出ない
        Kind::Extension => return None,
        Kind::Font => {
            // フォントはファミリ名でしか探せない。宣言が無ければ諦める
            if pkg.provides.is_empty() {
                return None;
            }
            return Some(
                pkg.provides
                    .iter()
                    .map(|p| (Kind::Font, p.clone()))
                    .collect(),
            );
        }
        Kind::Command => {}
        // ライブラリは実行ファイルを持たない
        Kind::Library => return None,
    }

    // フォント以外の cask は GUI アプリ。PATH に出ないのが普通
    if pkg.manager_of() == Manager::BrewCask {
        return None;
    }
    // mise が入れるものは shim 経由で、mise を有効化したシェルでしか
    // PATH に現れない。存在しないことが異常を意味しないので判定しない。
    if pkg.manager_of() == Manager::Mise {
        return None;
    }

    let mut out: Vec<(Kind, String)> = pkg
        .provides
        .iter()
        .map(|p| (Kind::Command, p.clone()))
        .collect();
    if out.is_empty() {
        out.push((Kind::Command, name.to_string()));
    }
    Some(out)
}

fn resolves(kind: Kind, name: &str) -> bool {
    match kind {
        Kind::Font => font_installed(name),
        _ => on_path(name),
    }
}

fn on_path(name: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| {
            std::env::split_paths(&paths).any(|dir| {
                let p = dir.join(name);
                p.is_file() && is_executable(&p)
            })
        })
        .unwrap_or(false)
}

#[cfg(unix)]
fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p)
        .map(|m| m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(_p: &Path) -> bool {
    true
}

/// フォントのファイル名はファミリ名と一致しない
/// (Hack Nerd Font Mono -> HackNerdFontMono-Regular.ttf)。
/// 空白を除いた前方一致で探す。
fn font_installed(family: &str) -> bool {
    let needle = family.replace(' ', "").to_lowercase();
    font_dirs().iter().any(|dir| {
        std::fs::read_dir(dir)
            .map(|entries| {
                entries.filter_map(|e| e.ok()).any(|e| {
                    e.file_name()
                        .to_string_lossy()
                        .replace(' ', "")
                        .to_lowercase()
                        .starts_with(&needle)
                })
            })
            .unwrap_or(false)
    })
}

fn font_dirs() -> Vec<PathBuf> {
    let home = std::env::var("HOME").unwrap_or_default();
    if cfg!(target_os = "macos") {
        vec![
            PathBuf::from(&home).join("Library/Fonts"),
            PathBuf::from("/Library/Fonts"),
            PathBuf::from("/System/Library/Fonts"),
        ]
    } else {
        vec![
            PathBuf::from(&home).join(".local/share/fonts"),
            PathBuf::from(&home).join(".fonts"),
            PathBuf::from("/usr/share/fonts"),
            PathBuf::from("/usr/local/share/fonts"),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pkg(src: &str) -> Package {
        toml::from_str(src).unwrap()
    }

    /// provides があればそちらを探す。neovim は nvim としてしか PATH に出ない。
    #[test]
    fn looks_for_provided_names_not_the_package_name() {
        let p = pkg("provides = [\"nvim\"]\n");
        let got = verifiable("neovim", &p).unwrap();
        assert_eq!(got, vec![(Kind::Command, "nvim".to_string())]);
    }

    #[test]
    fn falls_back_to_the_package_name() {
        let p = pkg("");
        let got = verifiable("bat", &p).unwrap();
        assert_eq!(got, vec![(Kind::Command, "bat".to_string())]);
    }

    /// GUI アプリは PATH に出ないので検証対象にしない。
    /// ここを外すと 1password や slack が毎回 missing になる。
    #[test]
    fn gui_casks_are_not_verifiable() {
        let p = pkg("manager = \"brew-cask\"\n");
        assert!(verifiable("slack", &p).is_none());
    }

    /// フォントの cask は検証できる。ファミリ名の宣言がある場合に限る。
    #[test]
    fn font_casks_are_verified_by_family_name() {
        let p =
            pkg("manager = \"brew-cask\"\nkind = \"font\"\nprovides = [\"Hack Nerd Font Mono\"]\n");
        let got = verifiable("font-hack-nerd-font", &p).unwrap();
        assert_eq!(got, vec![(Kind::Font, "Hack Nerd Font Mono".to_string())]);
    }

    /// mise の shim は mise を有効化したシェルにしか出ない。
    #[test]
    fn mise_managed_tools_are_not_verifiable() {
        let p = pkg("manager = \"mise\"\n");
        assert!(verifiable("go", &p).is_none());
    }

    /// ライブラリは実行ファイルを持たない。
    #[test]
    fn libraries_are_not_verifiable() {
        let p = pkg("kind = \"library\"\n");
        assert!(verifiable("libyaml", &p).is_none());
    }
}
