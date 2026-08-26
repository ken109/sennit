use crate::detect;
use crate::packages::{Kind, Package, Packages};
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// シェル履歴を読んで、宣言したコマンドが実際に使われているかを見る。
///
/// check は設定ファイル、verify はマシンの状態を見る。どちらも
/// 「シェルから手で叩くだけのコマンド」(rg, fd, gdu) には届かない。
/// 履歴だけがその層の記録を持っている。
///
/// ただし履歴はマシンごとに違い、期限もある。CI のゲートにはできないので、
/// 判定ではなく棚卸しの材料として出す。常に成功で終わる。
pub fn audit(root: &Path, history: Option<PathBuf>) -> Result<()> {
    let packages = Packages::load(&root.join("packages.toml"))?;
    let path = history.map(Ok).unwrap_or_else(default_history)?;
    // zsh は履歴にメタ文字でエンコードしたバイトを書くことがあり、
    // ファイル全体が正しい UTF-8 とは限らない。壊れた並びは読み飛ばす。
    let bytes = std::fs::read(&path)
        .with_context(|| format!("failed to read history: {}", path.display()))?;
    let text = String::from_utf8_lossy(&bytes);
    let counts = count_commands(&text);

    // 設定から参照されているものは、打鍵されなくても動いている。
    // starship や delta のような自動実行のツールを「未使用」と言わないため、
    // check の検出結果と突き合わせる。
    let referenced: std::collections::HashSet<String> = detect::scan(root)?
        .into_iter()
        .filter(|r| r.kind == Kind::Command)
        .map(|r| r.name)
        .collect();

    // 設定フォーマットの検出器が読まないファイル(script/, lefthook.yml,
    // ワークフロー等)での言及も見る。check の undeclared 判定に使うと
    // 誤検出だらけになるが、「この宣言は使われているか」を見る向きでは有効。
    let mentioned = mentions(root)?;

    let mut used: Vec<(String, usize)> = Vec::new();
    let mut unused: Vec<String> = Vec::new();
    let mut config_only: Vec<String> = Vec::new();
    let mut skipped = 0usize;

    for (name, pkg) in packages.applicable() {
        match command_names(&name, pkg) {
            None => skipped += 1,
            Some(names) => {
                let n: usize = names.iter().filter_map(|c| counts.get(c)).sum();
                if n > 0 {
                    used.push((name, n));
                } else if names.iter().any(|c| referenced.contains(c))
                    || names.iter().any(|c| mentioned.contains(c))
                    || mentioned.contains(&name)
                {
                    config_only.push(name);
                } else {
                    unused.push(name);
                }
            }
        }
    }

    used.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    unused.sort();
    config_only.sort();

    println!(
        "{} command(s) in {} history entries ({} packages not commands)",
        used.len() + unused.len(),
        counts.values().sum::<usize>(),
        skipped
    );

    println!(
        "  {} typed, {} driven by config, {} neither",
        used.len(),
        config_only.len(),
        unused.len()
    );

    if !unused.is_empty() {
        println!("\nneither typed nor referenced by any config:");
        for name in &unused {
            println!("  \x1b[33m·\x1b[0m {name}");
        }
    }

    println!("\nmost used:");
    for (name, n) in used.iter().take(10) {
        println!("  {n:>5}  {name}");
    }

    println!(
        "\nhistory is per machine and gets trimmed; treat this as a prompt to look, \
         not as proof that something is unused"
    );
    Ok(())
}

/// リポジトリ内のテキストファイルに現れる語をすべて集める。
/// packages.toml 自身は宣言そのものなので除く。
fn mentions(root: &Path) -> Result<std::collections::HashSet<String>> {
    let mut out = std::collections::HashSet::new();
    collect(root, &mut out, 0)?;
    Ok(out)
}

fn collect(dir: &Path, out: &mut std::collections::HashSet<String>, depth: usize) -> Result<()> {
    if depth > 6 {
        return Ok(());
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Ok(());
    };
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if name == ".git" || name == "target" || name == "packages.toml" {
            continue;
        }
        if path.is_dir() {
            collect(&path, out, depth + 1)?;
            continue;
        }
        // 大きなファイルは読まない(フォントやバイナリ)
        if entry
            .metadata()
            .map(|m| m.len() > 512 * 1024)
            .unwrap_or(true)
        {
            continue;
        }
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        let text = String::from_utf8_lossy(&bytes);
        for word in text.split(|c: char| !(c.is_alphanumeric() || c == '-' || c == '_')) {
            if word.len() > 1 {
                out.insert(word.to_string());
            }
        }
    }
    Ok(())
}

fn default_history() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME is not set")?;
    for candidate in [".zsh_history", ".bash_history"] {
        let p = PathBuf::from(&home).join(candidate);
        if p.is_file() {
            return Ok(p);
        }
    }
    anyhow::bail!("no shell history found; pass --history <path>")
}

/// 履歴からコマンド名を数える。
///
/// zsh の拡張履歴は `: <時刻>:<経過>;<コマンド>` の形。パイプや `&&` で
/// 繋がった各セグメントの先頭語を数える。sudo や env のような前置詞は
/// 読み飛ばして、その次を本体とみなす。
fn count_commands(text: &str) -> HashMap<String, usize> {
    const PREFIXES: [&str; 6] = ["sudo", "env", "command", "time", "nohup", "exec"];
    let mut counts = HashMap::new();

    for line in text.lines() {
        let cmd = match line.strip_prefix(':') {
            Some(rest) => match rest.split_once(';') {
                Some((_, c)) => c,
                None => continue,
            },
            None => line,
        };

        for segment in cmd.split(['|', ';', '&']) {
            let mut words = segment.split_whitespace();
            let mut head = match words.next() {
                Some(w) => w,
                None => continue,
            };
            // 前置詞(sudo, env ...)と、その後ろに続く KEY=value を読み飛ばして
            // 本体のコマンドまで進む。`env FOO=1 rg y` の本体は rg。
            let mut skipped_prefix = false;
            loop {
                if PREFIXES.contains(&head) {
                    skipped_prefix = true;
                } else if skipped_prefix && head.contains('=') {
                    // 前置詞の後ろの代入だけ読み飛ばす
                } else {
                    break;
                }
                head = match words.next() {
                    Some(w) => w,
                    None => break,
                };
            }
            // 代入や置換で始まるものはコマンドではない
            if head.contains('=') || head.starts_with('$') || head.starts_with('(') {
                continue;
            }
            let head = head.rsplit('/').next().unwrap_or(head);
            if head.is_empty() {
                continue;
            }
            *counts.entry(head.to_string()).or_insert(0) += 1;
        }
    }
    counts
}

/// 履歴で探すべきコマンド名。コマンドを持たないものは None。
fn command_names(name: &str, pkg: &Package) -> Option<Vec<String>> {
    if pkg.kind_of() != Kind::Command {
        return None;
    }
    if pkg.manager_of() == crate::packages::Manager::BrewCask {
        return None;
    }
    let mut out = pkg.provides.clone();
    out.push(name.to_string());
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_the_head_of_each_segment() {
        let c = count_commands("rg foo | bat\ngit status\n");
        assert_eq!(c.get("rg"), Some(&1));
        assert_eq!(c.get("bat"), Some(&1));
        assert_eq!(c.get("git"), Some(&1));
    }

    /// zsh の拡張履歴は `: 時刻:経過;コマンド` の形。
    #[test]
    fn understands_zsh_extended_history() {
        let c = count_commands(": 1700000000:0;rg pattern\n");
        assert_eq!(c.get("rg"), Some(&1));
        assert_eq!(c.get(":"), None);
    }

    /// sudo や env は前置詞であって本体ではない。
    #[test]
    fn skips_prefix_commands() {
        let c = count_commands("sudo apt-get install x\nenv FOO=1 rg y\n");
        assert_eq!(c.get("apt-get"), Some(&1));
        assert_eq!(c.get("rg"), Some(&1));
        assert_eq!(c.get("sudo"), None);
    }

    #[test]
    fn strips_leading_paths() {
        let c = count_commands("/opt/homebrew/bin/sennit check\n");
        assert_eq!(c.get("sennit"), Some(&1));
    }

    /// 変数代入で始まるものはコマンドではない。
    #[test]
    fn ignores_assignments() {
        let c = count_commands("FOO=bar\n");
        assert!(c.is_empty());
    }
}
