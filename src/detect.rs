use crate::packages::Kind;
use anyhow::Result;
use std::path::Path;

/// 設定ファイルが要求している外部依存。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Requirement {
    pub kind: Kind,
    pub name: String,
    /// どのファイルのどこから読み取ったか
    pub source: String,
}

/// リポジトリ全体を走査して要求を集める。
///
/// 検出器は設定フォーマットごとに書く。汎用的な文字列検索にすると
/// 誤検出だらけになるため、意味の分かっている箇所だけを見る。
pub fn scan(root: &Path) -> Result<Vec<Requirement>> {
    let mut reqs = Vec::new();
    git_config(root, &mut reqs)?;
    alacritty(root, &mut reqs)?;
    zed(root, &mut reqs)?;
    zsh(root, &mut reqs)?;
    config_dirs(root, &mut reqs)?;
    reqs.sort_by(|a, b| (a.kind.label(), &a.name).cmp(&(b.kind.label(), &b.name)));
    reqs.dedup_by(|a, b| a.kind == b.kind && a.name == b.name);
    Ok(reqs)
}

fn read(root: &Path, rel: &str) -> Option<String> {
    std::fs::read_to_string(root.join(rel)).ok()
}

/// 先頭の `!`(git のシェル実行記法)と引数を落として、実行されるコマンド名を取る。
fn command_name(value: &str) -> Option<String> {
    let v = value.trim().trim_start_matches('!').trim();
    let first = v.split_whitespace().next()?;
    // 絶対パス指定でも実体名を取る
    let name = first.rsplit('/').next()?;
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}

/// git config: pager / diffFilter / credential helper が外部コマンドを呼ぶ。
fn git_config(root: &Path, out: &mut Vec<Requirement>) -> Result<()> {
    let Some(text) = read(root, ".config/git/config") else {
        return Ok(());
    };
    for line in text.lines() {
        let line = line.trim();
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        if !matches!(key, "pager" | "diffFilter" | "helper") {
            continue;
        }
        if let Some(name) = command_name(value) {
            out.push(Requirement {
                kind: Kind::Command,
                name,
                source: format!(".config/git/config ({key})"),
            });
        }
    }
    Ok(())
}

/// alacritty: フォントファミリ
fn alacritty(root: &Path, out: &mut Vec<Requirement>) -> Result<()> {
    let Some(text) = read(root, ".config/alacritty/alacritty.toml") else {
        return Ok(());
    };
    for cap in find_all(&text, "family = \"") {
        out.push(Requirement {
            kind: Kind::Font,
            name: cap,
            source: ".config/alacritty/alacritty.toml (font.family)".into(),
        });
    }
    Ok(())
}

/// zed: フォントファミリと、auto_install_extensions が要求する拡張
fn zed(root: &Path, out: &mut Vec<Requirement>) -> Result<()> {
    let Some(text) = read(root, ".config/zed/settings.json") else {
        return Ok(());
    };
    for key in ["\"buffer_font_family\": \"", "\"font_family\": \""] {
        for cap in find_all(&text, key) {
            out.push(Requirement {
                kind: Kind::Font,
                name: cap,
                source: ".config/zed/settings.json (font_family)".into(),
            });
        }
    }
    // auto_install_extensions のブロック内のキーを拾う
    if let Some(start) = text.find("\"auto_install_extensions\"") {
        if let Some(open) = text[start..].find('{') {
            let rest = &text[start + open..];
            if let Some(close) = rest.find('}') {
                for cap in find_all(&rest[..close], "\"") {
                    if !cap.is_empty() {
                        out.push(Requirement {
                            kind: Kind::Extension,
                            name: cap,
                            source: ".config/zed/settings.json (auto_install_extensions)".into(),
                        });
                    }
                }
            }
        }
    }
    Ok(())
}

/// zsh: eval "$(cmd ...)" と ${+commands[cmd]} が実在を前提にしているコマンド
fn zsh(root: &Path, out: &mut Vec<Requirement>) -> Result<()> {
    let mut files: Vec<std::path::PathBuf> = vec![root.join(".zshenv")];
    if let Ok(dir) = std::fs::read_dir(root.join(".config/zsh/rc")) {
        files.extend(dir.filter_map(|e| e.ok()).map(|e| e.path()));
    }
    files.push(root.join(".config/zsh/.zshrc"));

    for path in files {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let rel = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .display()
            .to_string();

        for cap in find_all(&text, "eval \"$(") {
            if let Some(name) = command_name(&cap) {
                out.push(Requirement {
                    kind: Kind::Command,
                    name,
                    source: format!("{rel} (eval)"),
                });
            }
        }
        for cap in find_all(&text, "${+commands[") {
            let name = cap.trim_end_matches(']').to_string();
            if !name.is_empty() {
                out.push(Requirement {
                    kind: Kind::Command,
                    name,
                    source: format!("{rel} (commands[])"),
                });
            }
        }
    }
    Ok(())
}

/// .config/<name> の存在そのものが、そのツールへの依存を表す。
///
/// direnv のように「設定ディレクトリはあるが、どの設定ファイルからも
/// 名前が参照されない」依存はこれでしか捕まらない。実際 direnvrc を配置
/// しているのに direnv を入れていない期間があった。
/// 対象外にしたいものは packages.toml の ignore.commands に書く。
fn config_dirs(root: &Path, out: &mut Vec<Requirement>) -> Result<()> {
    let Ok(dir) = std::fs::read_dir(root.join(".config")) else {
        return Ok(());
    };
    for entry in dir.filter_map(|e| e.ok()) {
        let name = entry.file_name().to_string_lossy().to_string();
        // starship.toml のような単一ファイル形式も拾う
        let name = name.strip_suffix(".toml").unwrap_or(&name).to_string();
        if name.starts_with('.') {
            continue;
        }
        out.push(Requirement {
            kind: Kind::Command,
            name,
            source: ".config/ (設定ディレクトリの存在)".into(),
        });
    }
    Ok(())
}

/// prefix に続く、閉じ記号までの文字列をすべて拾う小さなヘルパ。
/// prefix の末尾文字を終端記号とみなす(`"` なら `"`、`(` なら `)`、`[` なら `]`)。
fn find_all(text: &str, prefix: &str) -> Vec<String> {
    let close = match prefix.chars().last() {
        Some('"') => '"',
        Some('(') => ')',
        Some('[') => ']',
        _ => return Vec::new(),
    };
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(i) = rest.find(prefix) {
        let after = &rest[i + prefix.len()..];
        if let Some(j) = after.find(close) {
            out.push(after[..j].to_string());
            rest = &after[j..];
        } else {
            break;
        }
    }
    out
}
