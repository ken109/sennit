use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::path::Path;
use std::process::Command;

/// リポジトリに置いた暗号化ファイルの復号方法。
///
/// プロバイダ(`{{ op://... }}`)は外部サービスに値を問い合わせるが、
/// こちらはリポジトリの中にある暗号文を鍵で開く。違いは大きい。
///
/// - 外部サービスもサインインも要らない。鍵ファイルさえあれば無人で動く
/// - よって CI でも初回セットアップでも成立する
///
/// 1Password が「人間のロック解除を必要とする」という理由で初回に使えない
/// のに対し、これはその制約が無い。両方あると穴が埋まる。
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Encryption {
    /// `{}` が暗号文ファイルのパスに置き換わる。標準出力が平文。
    pub command: String,
    /// この鍵が無ければ復号を試みずに保留する。
    /// 鍵の場所を宣言しておくと、失敗ではなく「まだ設定していない」と扱える。
    #[serde(default)]
    pub identity: Option<String>,
}

impl Encryption {
    /// 復号できる状態か。鍵の宣言が無ければ、やってみるまで分からないので真。
    pub fn ready(&self) -> bool {
        match &self.identity {
            None => true,
            Some(path) => Path::new(&expand_home(path)).exists(),
        }
    }

    pub fn identity_path(&self) -> Option<String> {
        self.identity.as_ref().map(|p| expand_home(p))
    }

    pub fn decrypt(&self, file: &Path) -> Result<Vec<u8>> {
        let mut parts = crate::render::shell_words(&self.command);
        if parts.is_empty() {
            bail!("encryption command is empty");
        }
        let file = file.display().to_string();
        for p in parts.iter_mut() {
            *p = expand_home(&p.replace("{}", &file));
        }
        let bin = parts.remove(0);

        let out = Command::new(&bin)
            .args(&parts)
            .output()
            .with_context(|| format!("failed to run `{bin}`; is it installed?"))?;
        if !out.status.success() {
            bail!(
                "decrypting {file} failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(out.stdout)
    }
}

/// 先頭の ~ を展開する。鍵はリポジトリの外に置くので必要になる。
fn expand_home(p: &str) -> String {
    match p.strip_prefix("~/") {
        Some(rest) => match std::env::var("HOME") {
            Ok(home) => format!("{home}/{rest}"),
            Err(_) => p.to_string(),
        },
        None => p.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enc(identity: Option<&str>) -> Encryption {
        Encryption {
            command: "age -d -i key {}".into(),
            identity: identity.map(|s| s.to_string()),
        }
    }

    /// 鍵の宣言が無ければ、やってみるまで分からないので実行を試みる。
    #[test]
    fn without_a_declared_identity_it_tries() {
        assert!(enc(None).ready());
    }

    /// 宣言した鍵が無い環境では復号を試みない。
    /// 「鍵をまだ置いていない」は壊れているのとは違う。
    #[test]
    fn a_missing_identity_defers_instead_of_failing() {
        assert!(!enc(Some("/nonexistent/key.txt")).ready());
    }

    #[test]
    fn an_existing_identity_is_ready() {
        let p = std::env::temp_dir().join("sennit-id-test");
        std::fs::write(&p, "x").unwrap();
        assert!(enc(Some(p.to_str().unwrap())).ready());
        std::fs::remove_file(&p).ok();
    }

    /// 鍵はリポジトリの外に置くので ~ を展開できる必要がある。
    #[test]
    fn tilde_is_expanded() {
        let home = std::env::var("HOME").unwrap();
        assert_eq!(expand_home("~/x"), format!("{home}/x"));
        assert_eq!(expand_home("/abs/x"), "/abs/x");
    }
}
