use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::Path;

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
    /// この OS でだけ走らせる。空なら全 OS。packages.toml の `os` と同じ綴り
    #[serde(default)]
    pub os: Vec<String>,
    /// `run` を解釈するシェル。既定は `sh`(Windows では Git for Windows の bash)
    #[serde(default)]
    pub shell: Option<String>,
    /// フックが作るはずの状態が、このマシンで成り立っているかを訊くコマンド。
    /// 0 で終われば成り立っている。`run` と同じシェル・同じ `cwd` で走る。
    ///
    /// 監視対象の指紋だけでは「前に走った」までしか分からない。launchd や
    /// systemd への登録のように、走らせた結果がリポジトリの外(OS の側)に
    /// あるものは、誰かが外したり、登録が黙って失敗したりしても指紋は
    /// 変わらず、apply は二度と走らせず、誰も気づかない。実際、定期実行の
    /// 登録が 1 本も無いまま気づかれなかった。成り立っているかを訊く手段が
    /// あれば、apply は崩れたときに走らせ直し、verify は崩れていると言える。
    #[serde(default)]
    pub check: Option<String>,
}

/// `shell` に書けるもの。
pub const SHELLS: [&str; 3] = ["sh", "powershell", "pwsh"];

impl Hook {
    /// 宣言の検査。読み込みで落とす。
    pub fn validate(&self, name: &str) -> Result<()> {
        crate::manifest::validate_os(&format!("hook `{name}`"), &self.os)?;
        if let Some(sh) = &self.shell {
            if !SHELLS.contains(&sh.as_str()) {
                bail!(
                    "hook `{name}` has an unknown shell `{sh}`; expected one of: {}",
                    SHELLS.join(", ")
                );
            }
        }
        // 空の check はどのシェルでも 0 で終わる。いつも「成り立っている」と
        // 答える検査は、無いより悪い
        if self.check.as_deref().is_some_and(|c| c.trim().is_empty()) {
            bail!("hook `{name}` has an empty `check`; remove it or give it a command");
        }
        Ok(())
    }

    /// `script` を、このフックのシェルとディレクトリで走らせるコマンド。
    fn command(&self, root: &Path, name: &str, script: &str) -> Result<std::process::Command> {
        let dir = match &self.cwd {
            Some(c) => root.join(c),
            None => root.to_path_buf(),
        };
        let mut c = crate::platform::shell(self.shell.as_deref().unwrap_or("sh"), script)
            .with_context(|| format!("hook `{name}`"))?;
        c.current_dir(dir);
        Ok(c)
    }

    /// `check` を走らせた終了状態。宣言が無ければ None。
    fn run_check(&self, root: &Path, name: &str) -> Result<Option<std::process::ExitStatus>> {
        let Some(check) = &self.check else {
            return Ok(None);
        };
        let status = self
            .command(root, name, check)?
            .status()
            .with_context(|| format!("hook `{name}`: failed to run its check"))?;
        Ok(Some(status))
    }
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

/// run_all の結果。
///
/// 指紋だけでは「何かした」が分からない。監視対象の無いフックは毎回走るが
/// 指紋は動かないので、実際にコマンドを走らせた回に apply が「最新です」と
/// 出てしまっていた。走った本数も返す。
#[derive(Debug)]
pub struct Ran {
    pub fingerprints: BTreeMap<String, String>,
    pub count: usize,
}

/// 走らせる必要のあるフックだけを走らせ、新しい指紋を返す。
///
/// 走らせるのは、監視対象が変わったとき、または `check` が崩れていると
/// 答えたとき。`when-changed` も `check` も無いフックは毎回走る。
/// `check` のあるフックは、走らせたあとにもう一度訊き、まだ崩れていれば
/// 失敗にする。登録が黙って失敗したまま成功を返さないため。
///
/// `check` は --dry-run でも走らせる。状態を訊くだけのコマンドとして宣言する
/// もので、訊かなければ preview が本番と食い違う。
pub fn run_all(
    root: &Path,
    hooks: &BTreeMap<String, Hook>,
    previous: &BTreeMap<String, String>,
    dry_run: bool,
) -> Result<Ran> {
    let mut next = previous.clone();
    let mut count = 0usize;

    for (name, hook) in hooks {
        // ほかの OS 向けのフックは走らせず、指紋も前回のまま残す
        if !crate::manifest::on_this_os(&hook.os) {
            continue;
        }
        let fp = fingerprint(root, &hook.when_changed);
        let watched = !hook.when_changed.is_empty();
        let changed = watched && previous.get(name) != Some(&fp);
        // 監視対象も check も無いものは毎回走らせる
        let always = !watched && hook.check.is_none();
        let note = if changed || always {
            ""
        } else {
            match hook.run_check(root, name)? {
                Some(st) if !st.success() => "  (its check failed)",
                // 変わっておらず、check も成り立っている(または無い)
                _ => continue,
            }
        };

        println!("  {:>8}  {name}{note}", "hook");
        count += 1;
        if dry_run {
            continue;
        }

        let status = hook
            .command(root, name, &hook.run)?
            .status()
            .with_context(|| format!("hook `{name}`: failed to run"))?;
        if !status.success() {
            bail!(
                "hook `{name}` failed {}",
                describe_failure(status, &hook.run)
            );
        }
        if let Some(st) = hook.run_check(root, name)? {
            if !st.success() {
                bail!(
                    "hook `{name}` ran, but its check still fails {}",
                    describe_failure(st, hook.check.as_deref().unwrap_or_default())
                );
            }
        }
        next.insert(name.clone(), fp);
    }
    Ok(Ran {
        fingerprints: next,
        count,
    })
}

/// このマシン向けのフックのうち、`check` が崩れていると答えたものを出す。
/// 戻り値はその数。
///
/// verify から呼ぶ。check を走らせられなかったもの(シェルが無いなど)も
/// 崩れているに数える。確かめられなかったものを「問題なし」とは言わない。
pub fn verify(root: &Path, hooks: &BTreeMap<String, Hook>) -> usize {
    let mut bad = 0usize;
    for (name, hook) in hooks {
        if !crate::manifest::on_this_os(&hook.os) {
            continue;
        }
        match hook.run_check(root, name) {
            Ok(None) => {}
            Ok(Some(st)) if st.success() => {}
            Ok(Some(st)) => {
                bad += 1;
                let how = match st.code() {
                    Some(code) => format!("exit status {code}"),
                    None => st.to_string(),
                };
                println!(
                    "  \x1b[31mhook\x1b[0m     {name}  (its check failed with {how}; `sennit apply` runs the hook)"
                );
            }
            Err(e) => {
                bad += 1;
                println!("  \x1b[31mhook\x1b[0m     {name}  (its check could not run: {e:#})");
            }
        }
    }
    bad
}

/// 失敗したフックの、終了コードと何を走らせたか。
///
/// 終了コードが無いと、スクリプトのどこで落ちたのか当たりが付かない。
/// 複数行のスクリプト(PowerShell のフックに多い)を丸ごとエラーに載せると
/// 読めなくなるので、1 行のときだけ載せる。
fn describe_failure(status: std::process::ExitStatus, run: &str) -> String {
    let how = match status.code() {
        Some(code) => format!("with exit status {code}"),
        // Unix でシグナルで止まった
        None => format!("({status})"),
    };
    let run = run.trim();
    if run.lines().count() == 1 {
        format!("{how}: {run}")
    } else {
        format!("{how} (its `run` has {} lines)", run.lines().count())
    }
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
                os: vec![],
                shell: None,
                check: None,
            },
        );

        // 前回の記録に同じ名前があっても、監視対象が無いなら毎回走る
        let mut seen = BTreeMap::new();
        seen.insert("always".to_string(), fingerprint(&dir, &[]));

        assert_eq!(run_all(&dir, &hooks, &seen, false).unwrap().count, 1);
        assert_eq!(run_all(&dir, &hooks, &seen, false).unwrap().count, 1);
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
                os: vec![],
                shell: None,
                check: None,
            },
        );

        let seen = run_all(&dir, &hooks, &BTreeMap::new(), false).unwrap();
        assert_eq!(seen.count, 1);
        assert!(dir.join("ran.txt").exists());
        // 2 回目は走らない
        let again = run_all(&dir, &hooks, &seen.fingerprints, false).unwrap();
        assert_eq!(again.count, 0);
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
                os: vec![],
                shell: None,
                check: None,
            },
        );
        let e = run_all(&dir, &hooks, &BTreeMap::new(), false).unwrap_err();
        assert!(format!("{e:#}").contains("boom"), "{e:#}");
        // どれだけで終わったか、何を走らせたか
        assert!(format!("{e:#}").contains("exit status 3: exit 3"), "{e:#}");
    }

    /// 複数行のスクリプトはエラーに丸ごと載せない。
    #[test]
    fn a_failing_multi_line_hook_names_its_exit_status_only() {
        let dir = std::env::temp_dir().join("sennit-hook-fail-lines");
        std::fs::create_dir_all(&dir).unwrap();
        let mut hooks = BTreeMap::new();
        hooks.insert(
            "boom".to_string(),
            Hook {
                when_changed: vec![],
                run: "true\nexit 4\n".to_string(),
                cwd: None,
                os: vec![],
                shell: None,
                check: None,
            },
        );
        let e = format!(
            "{:#}",
            run_all(&dir, &hooks, &BTreeMap::new(), false).unwrap_err()
        );
        assert!(e.contains("exit status 4 (its `run` has 2 lines)"), "{e}");
    }

    /// ほかの OS 向けのフックは走らず、前回の指紋も消えない。
    #[test]
    fn a_hook_for_another_os_does_not_run() {
        let dir = std::env::temp_dir().join("sennit-hook-os");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let other = if cfg!(windows) { "linux" } else { "windows" };
        let mut hooks = BTreeMap::new();
        hooks.insert(
            "elsewhere".to_string(),
            Hook {
                when_changed: vec![],
                run: "exit 3".to_string(),
                cwd: None,
                os: vec![other.to_string()],
                shell: None,
                check: None,
            },
        );
        let mut seen = BTreeMap::new();
        seen.insert("elsewhere".to_string(), "kept".to_string());
        let ran = run_all(&dir, &hooks, &seen, false).unwrap();
        assert_eq!(ran.count, 0);
        assert_eq!(ran.fingerprints["elsewhere"], "kept");
    }

    #[test]
    fn an_unknown_shell_or_os_is_rejected() {
        let h = |os: &[&str], shell: Option<&str>| Hook {
            when_changed: vec![],
            run: "true".into(),
            cwd: None,
            os: os.iter().map(|s| s.to_string()).collect(),
            shell: shell.map(str::to_string),
            check: None,
        };
        assert!(h(&["windows"], Some("pwsh")).validate("x").is_ok());
        let e = h(&[], Some("cmd")).validate("x").unwrap_err();
        assert!(format!("{e:#}").contains("unknown shell"), "{e:#}");
        let e = h(&["win"], None).validate("x").unwrap_err();
        assert!(format!("{e:#}").contains("unknown os"), "{e:#}");
    }

    fn checked(when_changed: &[&str], run: &str, check: &str) -> BTreeMap<String, Hook> {
        let mut hooks = BTreeMap::new();
        hooks.insert(
            "agents".to_string(),
            Hook {
                when_changed: when_changed.iter().map(|s| s.to_string()).collect(),
                run: run.to_string(),
                cwd: None,
                os: vec![],
                shell: None,
                check: Some(check.to_string()),
            },
        );
        hooks
    }

    fn ran(dir: &Path) -> usize {
        std::fs::read_to_string(dir.join("ran.txt"))
            .map(|s| s.lines().count())
            .unwrap_or(0)
    }

    /// 監視対象が変わっていなくても、check が崩れていれば走らせ直す。
    /// 登録を誰かが外した、登録が黙って失敗した、を apply が直せる。
    #[test]
    fn a_watched_hook_runs_again_when_its_check_fails() {
        let dir = std::env::temp_dir().join("sennit-hook-check-watched");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("watched"), "one").unwrap();
        let hooks = checked(
            &["watched"],
            "touch registered && echo ran >> ran.txt",
            "test -f registered",
        );

        let first = run_all(&dir, &hooks, &BTreeMap::new(), false).unwrap();
        assert_eq!((first.count, ran(&dir)), (1, 1));
        // 変わっておらず、成り立っている: 走らない
        let again = run_all(&dir, &hooks, &first.fingerprints, false).unwrap();
        assert_eq!((again.count, ran(&dir)), (0, 1));
        // 外から崩された: 指紋は同じでも走る
        std::fs::remove_file(dir.join("registered")).unwrap();
        let healed = run_all(&dir, &hooks, &first.fingerprints, false).unwrap();
        assert_eq!((healed.count, ran(&dir)), (1, 2));
        assert!(dir.join("registered").exists());
    }

    /// check があり監視対象の無いフックは、崩れているときだけ走る。
    #[test]
    fn a_hook_with_only_a_check_runs_when_the_check_fails() {
        let dir = std::env::temp_dir().join("sennit-hook-check-only");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let hooks = checked(
            &[],
            "touch registered && echo ran >> ran.txt",
            "test -f registered",
        );
        assert_eq!(
            run_all(&dir, &hooks, &BTreeMap::new(), false)
                .unwrap()
                .count,
            1
        );
        assert_eq!(
            run_all(&dir, &hooks, &BTreeMap::new(), false)
                .unwrap()
                .count,
            0
        );
        assert_eq!(ran(&dir), 1);
    }

    /// --dry-run は check を訊くが、フックは走らせない。
    #[test]
    fn dry_run_asks_the_check_but_does_not_run_the_hook() {
        let dir = std::env::temp_dir().join("sennit-hook-check-dry");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let hooks = checked(&[], "echo ran >> ran.txt", "test -f registered");
        assert_eq!(
            run_all(&dir, &hooks, &BTreeMap::new(), true).unwrap().count,
            1
        );
        assert_eq!(ran(&dir), 0);
        std::fs::write(dir.join("registered"), "").unwrap();
        assert_eq!(
            run_all(&dir, &hooks, &BTreeMap::new(), true).unwrap().count,
            0
        );
    }

    /// 走らせても check が成り立たないなら失敗にする。`|| true` で包んだ登録が
    /// 黙って失敗したまま、apply が成功を返さないため。指紋も記録しない。
    #[test]
    fn a_hook_whose_check_still_fails_after_running_is_an_error() {
        let dir = std::env::temp_dir().join("sennit-hook-check-still");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let hooks = checked(&[], "true", "exit 5");
        let e = format!(
            "{:#}",
            run_all(&dir, &hooks, &BTreeMap::new(), false).unwrap_err()
        );
        assert!(
            e.contains("ran, but its check still fails with exit status 5"),
            "{e}"
        );
    }

    /// verify は崩れている check を数える。走らせられなかったものも数える。
    #[test]
    fn verify_counts_failing_checks() {
        let dir = std::env::temp_dir().join("sennit-hook-check-verify");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut hooks = checked(&[], "true", "test -f registered");
        assert_eq!(verify(&dir, &hooks), 1);
        std::fs::write(dir.join("registered"), "").unwrap();
        assert_eq!(verify(&dir, &hooks), 0);
        // check の無いフックは訊かない
        hooks.get_mut("agents").unwrap().check = None;
        assert_eq!(verify(&dir, &hooks), 0);
    }

    #[test]
    fn an_empty_check_is_rejected() {
        let hooks = checked(&[], "true", "  ");
        let e = hooks["agents"].validate("agents").unwrap_err();
        assert!(format!("{e:#}").contains("empty `check`"), "{e:#}");
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
