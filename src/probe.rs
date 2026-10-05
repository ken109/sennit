use anyhow::{bail, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::io::Write;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// 秘密がまだ効くかを確かめる方法。
///
/// 「在る」と「効く」は別で、失効したトークンはファイルにもマシンにも
/// 存在したまま使えなくなる。静的な検査では見えない。
///
/// 何をもって「効く」とするかは秘密ごとに違う(どの API を叩くか)ので、
/// sennit は知らない。プロバイダと同じく、コマンドを宣言してもらう。
/// 終了コードが 0 なら有効。
///
///     [probes.github]
///     secret  = "op://Cloud/GitHub PAT/token"
///     command = "curl -fsS --max-time 10 -o /dev/null -H @- https://api.github.com/user"
///     input   = "Authorization: Bearer {}"
///     invalid-exit = [22]
///
/// 秘密の値はコマンドの引数にも環境変数にも渡さず、標準入力にだけ流す。
/// 引数は他のプロセスから `ps` で見えるため。
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Probe {
    /// 秘密の取り出し先。`[providers]` で解決できる `scheme://...` の形。
    pub secret: String,
    /// 検査コマンド。シェルを経由せず、引用符だけを解釈して分割する。
    pub command: String,
    /// 標準入力に流す内容。`{}` が秘密の値に置き換わる。
    #[serde(default = "default_input")]
    pub input: String,
    /// 「秘密が拒否された」ことを表す終了コード。
    ///
    /// 空なら 0 以外はすべて拒否と読む。書いておくと、それ以外の失敗
    /// (curl の接続失敗など)を「失効した」と言わずに「確かめられなかった」
    /// と区別できる。ネットワークが無いだけで失効と報告するのは誤りなので。
    #[serde(default, rename = "invalid-exit")]
    pub invalid_exit: Vec<i32>,
    /// 秒。これを過ぎたらコマンドを止めて「確かめられなかった」とする。
    #[serde(default = "default_timeout")]
    pub timeout: u64,
}

fn default_input() -> String {
    "{}".into()
}

fn default_timeout() -> u64 {
    30
}

impl Probe {
    /// 宣言そのものの検査。値を引く前に、綴りの誤りで落とす。
    pub fn validate(&self, name: &str) -> Result<()> {
        if crate::render::split_reference(&self.secret).is_none() {
            bail!(
                "probe `{name}`: secret `{}` is not a `scheme://...` reference",
                self.secret
            );
        }
        if crate::render::shell_words(&self.command).is_empty() {
            bail!("probe `{name}`: command is empty");
        }
        if self.timeout == 0 {
            bail!("probe `{name}`: timeout must be at least one second");
        }
        if self.invalid_exit.contains(&0) {
            bail!("probe `{name}`: invalid-exit may not contain 0, which means the secret is accepted");
        }
        Ok(())
    }
}

/// コマンドを走らせた結果。値は含めない。
#[derive(Debug, PartialEq, Eq)]
pub enum Ran {
    Exited(i32),
    /// 起動できなかった、時間切れ、シグナルで落ちた、など。
    Failed(String),
}

/// 1 件の判定。
#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    Accepted,
    /// 秘密が拒否された。値は終了コード。
    Rejected(i32),
    /// 確かめられなかった。理由には秘密の値を入れない。
    Unchecked(String),
}

/// 終了コードをどう読むか。
pub fn judge(probe: &Probe, ran: Ran) -> Verdict {
    match ran {
        Ran::Failed(why) => Verdict::Unchecked(why),
        Ran::Exited(0) => Verdict::Accepted,
        Ran::Exited(code) => {
            if probe.invalid_exit.is_empty() || probe.invalid_exit.contains(&code) {
                Verdict::Rejected(code)
            } else {
                Verdict::Unchecked(format!(
                    "the check exited {code}, which is not one of invalid-exit"
                ))
            }
        }
    }
}

/// 宣言された検査を名前順に走らせる。
///
/// 値の取得と検査コマンドの実行は呼び出し側から渡す。テストは外部サービスにも
/// 1Password にも触れずに、判定の分岐だけを確かめられる。
pub fn probe_all(
    probes: &BTreeMap<String, Probe>,
    fetch: &mut dyn FnMut(&str, &str) -> Result<String>,
    run: &dyn Fn(&Probe, &str) -> Ran,
) -> Vec<(String, Verdict)> {
    let mut out = Vec::new();
    for (name, probe) in probes {
        let verdict = match crate::render::split_reference(&probe.secret) {
            None => Verdict::Unchecked("secret is not a `scheme://...` reference".into()),
            Some((scheme, rest)) => match fetch(scheme, rest) {
                // 取れないのは、ロックされた 1Password や無いコマンドなど。
                // 秘密が効かないのとは別の話なので混ぜない。
                Err(e) => Verdict::Unchecked(format!("could not read the secret: {e:#}")),
                Ok(secret) => judge(probe, run(probe, &secret)),
            },
        };
        out.push((name.clone(), verdict));
    }
    out
}

/// 実際にコマンドを走らせる。出力は捨てる。
///
/// 検査コマンドが秘密を標準出力や標準エラーに返す(curl -v など)ことは
/// ありうるので、どちらも読まない。見せるのは終了コードだけ。
pub fn run_command(probe: &Probe, secret: &str) -> Ran {
    let mut parts = crate::render::shell_words(&probe.command);
    if parts.is_empty() {
        return Ran::Failed("command is empty".into());
    }
    let bin = parts.remove(0);

    let mut child = match Command::new(&bin)
        .args(&parts)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(_) => return Ran::Failed(format!("failed to run `{bin}`; is it installed?")),
    };

    if let Some(mut stdin) = child.stdin.take() {
        // 読まずに終わるコマンドへの書き込みは失敗する。結果は終了コードで見る
        let _ = stdin.write_all(probe.input.replace("{}", secret).as_bytes());
    }

    let deadline = Instant::now() + Duration::from_secs(probe.timeout);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                return match status.code() {
                    Some(code) => Ran::Exited(code),
                    None => Ran::Failed("the check was terminated by a signal".into()),
                }
            }
            Ok(None) => {}
            Err(e) => return Ran::Failed(format!("could not wait for the check: {e}")),
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Ran::Failed(format!("timed out after {}s", probe.timeout));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// 検査を走らせて結果を出す。拒否されたか確かめられなかったものがあれば失敗。
///
/// 確かめられなかったものを「問題なし」として通さない。通すと、1Password が
/// ロックされたままの CI が、何も検査せずに緑になる。
pub fn run(manifest: &crate::manifest::Manifest) -> Result<()> {
    if manifest.probes.is_empty() {
        println!("\x1b[33mnote\x1b[0m  no [probes] declared; no secret was checked");
        return Ok(());
    }

    let mut cache =
        crate::render::SecretCache::with(crate::render::effective_providers(&manifest.providers));
    let results = probe_all(
        &manifest.probes,
        &mut |scheme, rest| cache.read(scheme, rest),
        &run_command,
    );
    report(&results)
}

fn report(results: &[(String, Verdict)]) -> Result<()> {
    let accepted = results
        .iter()
        .filter(|(_, v)| *v == Verdict::Accepted)
        .count();
    println!("probed {} secret(s): {accepted} accepted", results.len());

    let mut rejected = 0usize;
    let mut unchecked = 0usize;
    for (name, verdict) in results {
        match verdict {
            Verdict::Accepted => {}
            Verdict::Rejected(code) => {
                rejected += 1;
                println!("  \x1b[31minvalid\x1b[0m    {name}  (the check exited {code})");
            }
            Verdict::Unchecked(why) => {
                unchecked += 1;
                println!("  \x1b[33munchecked\x1b[0m  {name}  ({why})");
            }
        }
    }

    if rejected == 0 && unchecked == 0 {
        println!("\x1b[32mok\x1b[0m  every probed secret is accepted");
        return Ok(());
    }
    match (rejected, unchecked) {
        (r, 0) => bail!("{r} secret(s) rejected"),
        (0, u) => bail!("{u} secret(s) could not be checked"),
        (r, u) => bail!("{r} secret(s) rejected, {u} could not be checked"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn probe(src: &str) -> Probe {
        toml::from_str(src).unwrap()
    }

    fn basic() -> Probe {
        probe("secret = \"op://V/I/f\"\ncommand = \"check\"\n")
    }

    /// 0 は有効。0 以外は、種類を限らない既定では拒否。
    #[test]
    fn zero_is_accepted_and_anything_else_is_rejected_by_default() {
        assert_eq!(judge(&basic(), Ran::Exited(0)), Verdict::Accepted);
        assert_eq!(judge(&basic(), Ran::Exited(1)), Verdict::Rejected(1));
        assert_eq!(judge(&basic(), Ran::Exited(22)), Verdict::Rejected(22));
    }

    /// invalid-exit を書くと、それ以外の失敗は「失効」と言わない。
    /// ネットワークが無いだけで失効と報告するのは誤り。
    #[test]
    fn only_the_declared_codes_mean_rejected() {
        let p = probe("secret = \"op://V/I/f\"\ncommand = \"c\"\ninvalid-exit = [22]\n");
        assert_eq!(judge(&p, Ran::Exited(22)), Verdict::Rejected(22));
        assert!(matches!(judge(&p, Ran::Exited(7)), Verdict::Unchecked(_)));
        assert_eq!(judge(&p, Ran::Exited(0)), Verdict::Accepted);
    }

    #[test]
    fn a_command_that_did_not_run_is_unchecked() {
        let v = judge(&basic(), Ran::Failed("timed out".into()));
        assert_eq!(v, Verdict::Unchecked("timed out".into()));
    }

    /// 値を取れないのは、効かないのとは別。コマンドは走らせない。
    #[test]
    fn a_secret_that_cannot_be_read_is_unchecked_and_nothing_is_run() {
        let mut probes = BTreeMap::new();
        probes.insert("a".to_string(), basic());
        let results = probe_all(
            &probes,
            &mut |_, _| anyhow::bail!("vault is locked"),
            &|_, _| panic!("the check must not run without a secret"),
        );
        assert_eq!(results.len(), 1);
        match &results[0].1 {
            Verdict::Unchecked(why) => assert!(why.contains("vault is locked"), "{why}"),
            other => panic!("{other:?}"),
        }
    }

    /// 取り出し先にはスキームと残りを分けて渡し、検査には取れた値を渡す。
    #[test]
    fn the_fetched_value_reaches_the_check() {
        let mut probes = BTreeMap::new();
        probes.insert("a".to_string(), basic());
        let mut asked = Vec::new();
        let results = probe_all(
            &probes,
            &mut |scheme, rest| {
                asked.push(format!("{scheme}|{rest}"));
                Ok("the-value".into())
            },
            &|_, secret| {
                if secret == "the-value" {
                    Ran::Exited(0)
                } else {
                    Ran::Exited(1)
                }
            },
        );
        assert_eq!(asked, vec!["op|V/I/f".to_string()]);
        assert_eq!(results[0].1, Verdict::Accepted);
    }

    #[test]
    fn a_secret_that_is_not_a_reference_is_rejected_when_validating() {
        let p = probe("secret = \"just-a-token\"\ncommand = \"c\"\n");
        assert!(p.validate("a").is_err());
    }

    #[test]
    fn an_empty_command_and_a_zero_timeout_are_rejected() {
        assert!(probe("secret = \"op://a/b\"\ncommand = \"\"\n")
            .validate("a")
            .is_err());
        assert!(
            probe("secret = \"op://a/b\"\ncommand = \"c\"\ntimeout = 0\n")
                .validate("a")
                .is_err()
        );
    }

    /// 0 は「効く」を表す。拒否の印にすると、有効な秘密が失効と報告される。
    #[test]
    fn zero_may_not_be_an_invalid_exit() {
        let p = probe("secret = \"op://a/b\"\ncommand = \"c\"\ninvalid-exit = [0]\n");
        assert!(p.validate("a").is_err());
    }

    /// 綴りの誤り(invalid_exit)で、検査が黙って「全部拒否」に変わらない。
    #[test]
    fn a_misspelled_key_is_rejected() {
        let r: Result<Probe, _> =
            toml::from_str("secret = \"op://a/b\"\ncommand = \"c\"\ninvalid_exit = [22]\n");
        assert!(r.is_err());
    }

    /// 値は標準入力に流れ、{} が置き換わる。
    #[test]
    fn the_secret_is_sent_on_stdin() {
        let p = probe(
            "secret = \"op://a/b\"\ncommand = \"grep -qx 'Bearer tok-123'\"\ninput = \"Bearer {}\"\n",
        );
        assert_eq!(run_command(&p, "tok-123"), Ran::Exited(0));
        assert_eq!(run_command(&p, "tok-999"), Ran::Exited(1));
    }

    #[test]
    fn a_missing_binary_is_unchecked_not_rejected() {
        let p = probe("secret = \"op://a/b\"\ncommand = \"sennit-no-such-binary\"\n");
        assert!(matches!(run_command(&p, "x"), Ran::Failed(_)));
    }

    #[test]
    fn a_hung_check_is_stopped() {
        let p = probe("secret = \"op://a/b\"\ncommand = \"sleep 30\"\ntimeout = 1\n");
        let started = Instant::now();
        assert!(matches!(run_command(&p, "x"), Ran::Failed(_)));
        assert!(started.elapsed() < Duration::from_secs(10));
    }
}
