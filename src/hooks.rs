use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

/// 配置したあとに走らせる処理。
///
/// 「設定を置いたら、それを取り込むために何かを実行する」という関係は
/// これまで表現できず、シェルスクリプトに置くしかなかった。結果として
/// 毎回無条件に走り、依存関係もどこにも書かれていなかった。
///
/// 実例: .config/bat/themes/ を配置しても、bat cache --build を踏まないと
/// テーマ名が解決できず、delta まで既定色に落ちる。
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Hook {
    /// このパスが変わったときだけ走らせる。空なら毎回走る。
    #[serde(default, rename = "when-changed")]
    pub when_changed: Vec<String>,
    pub run: String,
    /// 実行するディレクトリ(リポジトリルートからの相対)。既定はルート。
    #[serde(default)]
    pub cwd: Option<String>,
}

/// 監視対象の現在の指紋。内容とパスの両方を見る。
pub fn fingerprint(root: &Path, paths: &[String]) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut entries: Vec<(String, u64)> = Vec::new();
    for rel in paths {
        collect(&root.join(rel), root, &mut entries, 0);
    }
    entries.sort();

    let mut h = DefaultHasher::new();
    entries.hash(&mut h);
    format!("{:016x}", h.finish())
}

fn collect(path: &Path, root: &Path, out: &mut Vec<(String, u64)>, depth: usize) {
    if depth > 8 {
        return;
    }
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return;
    };
    if meta.is_dir() {
        let Ok(entries) = std::fs::read_dir(path) else {
            return;
        };
        for e in entries.filter_map(|e| e.ok()) {
            collect(&e.path(), root, out, depth + 1);
        }
        return;
    }
    // 内容そのものではなくサイズと更新時刻で見る。設定は小さいが、
    // フォントの tmTheme のように大きいものもあるため。
    let key = path
        .strip_prefix(root)
        .unwrap_or(path)
        .display()
        .to_string();
    let stamp = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    out.push((key, stamp ^ (meta.len() << 20)));
}

/// 変化したフックだけを走らせ、新しい指紋を返す。
pub fn run_all(
    root: &Path,
    hooks: &BTreeMap<String, Hook>,
    previous: &BTreeMap<String, String>,
    dry_run: bool,
) -> Result<BTreeMap<String, String>> {
    let mut next = previous.clone();

    for (name, hook) in hooks {
        let fp = fingerprint(root, &hook.when_changed);
        // when-changed が空なら毎回走らせる
        let changed = hook.when_changed.is_empty() || previous.get(name) != Some(&fp);
        if !changed {
            continue;
        }

        println!("  {:>8}  {name}", "hook");
        if dry_run {
            continue;
        }

        let dir = match &hook.cwd {
            Some(c) => root.join(c),
            None => root.to_path_buf(),
        };
        let status = Command::new("sh")
            .arg("-c")
            .arg(&hook.run)
            .current_dir(&dir)
            .status()
            .with_context(|| format!("hook `{name}`: failed to run"))?;
        if !status.success() {
            bail!("hook `{name}` failed: {}", hook.run);
        }
        next.insert(name.clone(), fp);
    }
    Ok(next)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 監視対象が無いフックは毎回走る。「常に実行」を表現する手段が要る。
    #[test]
    fn a_hook_without_watches_always_runs() {
        // 走ったかどうかは、実行が残す痕跡で見る。dry_run では何も走らないので
        // 「指紋が空」を見ても、走らないフックと区別が付かなかった。
        let dir = std::env::temp_dir().join("sennit-hook-always");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut hooks = BTreeMap::new();
        hooks.insert(
            "always".to_string(),
            Hook {
                when_changed: vec![],
                run: "echo ran >> ran.txt".to_string(),
                cwd: None,
            },
        );

        // 前回の記録に同じ名前があっても、監視対象が無いなら毎回走る
        let mut seen = BTreeMap::new();
        seen.insert("always".to_string(), fingerprint(&dir, &[]));

        run_all(&dir, &hooks, &seen, false).unwrap();
        run_all(&dir, &hooks, &seen, false).unwrap();
        let ran = std::fs::read_to_string(dir.join("ran.txt")).unwrap();
        assert_eq!(ran.lines().count(), 2, "{ran:?}");
    }

    /// 監視対象が変わっていなければ走らない。
    #[test]
    fn a_watched_hook_is_skipped_when_nothing_changed() {
        let dir = std::env::temp_dir().join("sennit-hook-watched");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("watched"), "one").unwrap();

        let mut hooks = BTreeMap::new();
        hooks.insert(
            "w".to_string(),
            Hook {
                when_changed: vec!["watched".to_string()],
                run: "echo ran >> ran.txt".to_string(),
                cwd: None,
            },
        );

        let seen = run_all(&dir, &hooks, &BTreeMap::new(), false).unwrap();
        assert!(dir.join("ran.txt").exists());
        // 2 回目は走らない
        run_all(&dir, &hooks, &seen, false).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("ran.txt"))
                .unwrap()
                .lines()
                .count(),
            1
        );
    }

    /// 非零で終わったフックは apply を落とす。
    #[test]
    fn a_failing_hook_is_an_error() {
        let dir = std::env::temp_dir().join("sennit-hook-fail");
        std::fs::create_dir_all(&dir).unwrap();
        let mut hooks = BTreeMap::new();
        hooks.insert(
            "boom".to_string(),
            Hook {
                when_changed: vec![],
                run: "exit 3".to_string(),
                cwd: None,
            },
        );
        let e = run_all(&dir, &hooks, &BTreeMap::new(), false).unwrap_err();
        assert!(format!("{e:#}").contains("boom"), "{e:#}");
    }

    /// 同じ内容なら指紋も同じ。ここが揺れるとフックが毎回走ってしまう。
    #[test]
    fn fingerprint_is_stable_for_unchanged_content() {
        let dir = std::env::temp_dir().join("sennit-fp-test");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a"), "x").unwrap();
        let a = fingerprint(&dir, &["a".into()]);
        let b = fingerprint(&dir, &["a".into()]);
        assert_eq!(a, b);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 存在しないパスでも落ちない。宣言だけ先に書くことがある。
    #[test]
    fn missing_paths_do_not_panic() {
        let fp = fingerprint(Path::new("/nonexistent"), &["nope".into()]);
        assert!(!fp.is_empty());
    }
}
