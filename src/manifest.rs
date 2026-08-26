use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::path::Path;

/// sennit.toml の表現。
///
/// 知らないキーは受け付けない。`when_changed` を `when-changed` の代わりに
/// 書くと、監視付きのフックが毎回走るフックに黙って変わる。綴りの誤りが
/// 無言の挙動変化になるくらいなら、読み込みで落とす方がよい。
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub link: Link,
    /// 出力パス -> テンプレートパス
    #[serde(default)]
    pub render: std::collections::BTreeMap<String, String>,
    /// 配置後に走らせる処理
    #[serde(default)]
    pub hooks: std::collections::BTreeMap<String, crate::hooks::Hook>,
    /// テンプレートに渡すデータファイル。既定は theme.toml。
    #[serde(default)]
    pub data: Vec<String>,
    /// 秘密の取り出し方。scheme -> コマンド。宣言が無ければ op のみ。
    #[serde(default)]
    pub providers: crate::render::Providers,
    /// 出力パス -> リポジトリ内の暗号文ファイル
    #[serde(default)]
    pub encrypted: std::collections::BTreeMap<String, String>,
    /// 暗号文の開き方
    #[serde(default)]
    pub encryption: Option<crate::encrypted::Encryption>,
    /// パス -> 8進のモード。
    ///
    /// symlink 方式なのでリポジトリ側の権限がそのまま見える。宣言しておくと
    /// apply が揃え、verify が検査する。
    #[serde(default)]
    pub modes: std::collections::BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Link {
    #[serde(default)]
    pub common: Vec<String>,
    #[serde(default)]
    pub darwin: Vec<String>,
    #[serde(default)]
    pub linux: Vec<String>,
    #[serde(default)]
    pub ignore: Vec<String>,
}

impl Manifest {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read manifest: {}", path.display()))?;
        let m: Manifest = toml::from_str(&text)
            .with_context(|| format!("failed to parse manifest: {}", path.display()))?;
        m.validate()
            .with_context(|| format!("invalid manifest: {}", path.display()))?;
        Ok(m)
    }

    /// 宣言そのものの検査。配置を始める前に落とす。
    fn validate(&self) -> Result<()> {
        let mut paths: Vec<(&str, &str)> = Vec::new();
        for p in self
            .link
            .common
            .iter()
            .chain(&self.link.darwin)
            .chain(&self.link.linux)
        {
            paths.push(("link", p));
        }
        for (out, src) in &self.render {
            paths.push(("render output", out));
            paths.push(("render template", src));
        }
        for (out, src) in &self.encrypted {
            paths.push(("encrypted output", out));
            paths.push(("encrypted source", src));
        }
        for p in self.modes.keys() {
            paths.push(("modes", p));
        }
        for p in &self.data {
            paths.push(("data", p));
        }
        for h in self.hooks.values() {
            if let Some(cwd) = &h.cwd {
                paths.push(("hook cwd", cwd));
            }
            for w in &h.when_changed {
                paths.push(("hook when-changed", w));
            }
        }

        // 相対パスは $HOME とリポジトリの両方の基準になる。絶対パスを混ぜると
        // join がそちらを採ってリポジトリの外を指し、`..` は $HOME の外へ出る。
        // どちらも「利用者の設定を置く」の範囲を越えるので宣言の時点で断る。
        for (what, p) in paths {
            let path = Path::new(p);
            if path.is_absolute() {
                bail!("{what} `{p}` is an absolute path; declarations are relative to the repository root");
            }
            if path
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
            {
                bail!("{what} `{p}` contains `..`; declarations may not leave the repository");
            }
            if p.is_empty() {
                bail!("{what} has an empty path");
            }
        }

        // 綴りを誤った 8 進は、宣言したはずの制限が黙って無くなる。
        // verify も同じ理由で読み飛ばすので、誰も気づけない。
        for (path, mode) in &self.modes {
            let m = u32::from_str_radix(mode, 8)
                .with_context(|| format!("mode `{mode}` for `{path}` is not octal"))?;
            // 3 桁ちょうど。`"60"` は 0o060 と読まれ、所有者から権限を
            // 落としたモードが意図せず掛かる。`"0600"` のような 4 桁も、
            // setuid ビットを含む書き方と紛れるので断る。
            if mode.len() != 3 {
                bail!("mode `{mode}` for `{path}` must be exactly three octal digits");
            }
            // 許可ビットだけ。setuid / setgid は dotfiles の用途に無く、
            // 受け付けると verify が見る 0o777 と食い違って収束しなくなる。
            if m > 0o777 {
                bail!("mode `{mode}` for `{path}` is out of range; use three octal digits");
            }
        }
        Ok(())
    }

    /// 現在の OS で配置対象になるトップレベルのパス。
    pub fn targets(&self) -> Vec<&str> {
        let mut out: Vec<&str> = self.link.common.iter().map(String::as_str).collect();
        let os_specific = if cfg!(target_os = "macos") {
            &self.link.darwin
        } else {
            &self.link.linux
        };
        out.extend(os_specific.iter().map(String::as_str));
        out
    }

    /// [render] の入力として宣言されているか。
    pub fn is_template(&self, rel: &Path) -> bool {
        self.render.values().any(|t| Path::new(t) == rel)
    }

    /// [encrypted] の入力として宣言されているか。暗号文も配置しない。
    pub fn is_ciphertext(&self, rel: &Path) -> bool {
        self.encrypted.values().any(|t| Path::new(t) == rel)
    }

    /// このパスに宣言されたモード。書いたパスそのものにだけ効く。
    ///
    /// 以前は前方一致で、ディレクトリの宣言が下のファイルにも降りていた。
    /// `.ssh = "700"` が known_hosts を実行可能にする一方、ディレクトリ
    /// 自体は誰も触らず、verify は永久に「755 のままだ」と言い続けていた。
    /// verify は昔から書いたパスだけを見ていたので、そちらに揃える。
    pub fn mode_for(&self, rel: &Path) -> Option<u32> {
        self.modes
            .iter()
            .find(|(pat, _)| Path::new(pat.as_str()) == rel)
            // 8 進として読めることは load の検査で保証済み
            .and_then(|(_, m)| u32::from_str_radix(m, 8).ok())
    }

    /// ignore のパターンは 2 種類だけ扱う。
    /// - `*.ext` : 拡張子一致
    /// - それ以外: パスの前方一致(同一パスも含む)
    pub fn is_ignored(&self, rel: &Path) -> bool {
        self.link.ignore.iter().any(|pat| {
            if let Some(ext) = pat.strip_prefix("*.") {
                rel.extension().is_some_and(|e| e == ext)
            } else {
                rel.starts_with(pat)
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load(toml: &str) -> Result<Manifest> {
        let p = std::env::temp_dir().join(format!(
            "sennit-manifest-{}.toml",
            toml.len() as u64 * 31 + toml.bytes().map(u64::from).sum::<u64>()
        ));
        std::fs::write(&p, toml).unwrap();
        Manifest::load(&p)
    }

    #[test]
    fn an_absolute_declaration_is_rejected() {
        // join が絶対パスを採るので、リポジトリの外を指してしまう
        let e = load("[link]\ncommon = [\"/etc/passwd\"]\n").unwrap_err();
        assert!(format!("{e:#}").contains("absolute"), "{e:#}");
    }

    #[test]
    fn leaving_the_repository_is_rejected() {
        let e = load("[link]\ncommon = [\"../secret\"]\n").unwrap_err();
        assert!(format!("{e:#}").contains(".."), "{e:#}");
    }

    #[test]
    fn a_template_path_that_escapes_is_rejected() {
        let e = load("[link]\ncommon = []\n\n[render]\n\"a.conf\" = \"../a.tmpl\"\n").unwrap_err();
        assert!(format!("{e:#}").contains(".."), "{e:#}");
    }

    #[test]
    fn a_mode_that_is_not_octal_is_rejected() {
        // 黙って読み飛ばすと、宣言した制限が誰にも適用されないまま ok になる
        let e = load("[link]\ncommon = []\n\n[modes]\n\".npmrc\" = \"0o600\"\n").unwrap_err();
        assert!(format!("{e:#}").contains("octal"), "{e:#}");
    }

    #[test]
    fn a_misspelled_key_is_rejected() {
        // when_changed と書くと、監視付きのフックが毎回走るフックに変わる
        let e = load("[link]\ncommon = []\n\n[hooks.h]\nwhen_changed = [\"a\"]\nrun = \"true\"\n")
            .unwrap_err();
        assert!(format!("{e:#}").contains("when_changed"), "{e:#}");
    }

    #[test]
    fn a_valid_manifest_loads() {
        let m = load(
            "[link]\ncommon = [\".config\"]\nignore = [\"*.tmpl\"]\n\n[modes]\n\".npmrc\" = \"600\"\n",
        )
        .unwrap();
        assert_eq!(m.mode_for(Path::new(".npmrc")), Some(0o600));
    }

    fn manifest(ignore: &[&str]) -> Manifest {
        Manifest {
            link: Link {
                common: vec![],
                darwin: vec![],
                linux: vec![],
                ignore: ignore.iter().map(|s| s.to_string()).collect(),
            },
            render: Default::default(),
            hooks: Default::default(),
            providers: Default::default(),
            encrypted: Default::default(),
            encryption: None,
            data: Default::default(),
            modes: Default::default(),
        }
    }

    #[test]
    fn ignores_by_extension_glob() {
        let m = manifest(&["*.tmpl"]);
        assert!(m.is_ignored(Path::new(".config/starship.toml.tmpl")));
        assert!(!m.is_ignored(Path::new(".config/starship.toml")));
    }

    /// 前方一致は要素単位。文字列としての前方一致にすると、名前の頭が
    /// 同じだけの別のディレクトリまで巻き込む。
    #[test]
    fn a_prefix_matches_whole_components_not_characters() {
        let m = manifest(&["conf/nvim"]);
        assert!(m.is_ignored(Path::new("conf/nvim/init.lua")));
        assert!(m.is_ignored(Path::new("conf/nvim")));
        // 名前の頭が同じだけの別物は対象外
        assert!(!m.is_ignored(Path::new("conf/nvim-extra/x.lua")));
        assert!(!m.is_ignored(Path::new("conf/nvimrc")));
    }

    /// 根からの前方一致。途中の階層に同名があっても当たらない。
    #[test]
    fn a_prefix_is_rooted_at_the_repository() {
        let m = manifest(&["README.md"]);
        assert!(m.is_ignored(Path::new("README.md")));
        assert!(!m.is_ignored(Path::new("conf/README.md")));
    }

    #[test]
    fn ignores_by_path_prefix() {
        let m = manifest(&[".config/secret"]);
        assert!(m.is_ignored(Path::new(".config/secret/token")));
        // 前方一致はディレクトリ自身にも効く
        assert!(m.is_ignored(Path::new(".config/secret")));
        assert!(!m.is_ignored(Path::new(".config/public/token")));
    }

    fn with_modes(pairs: &[(&str, &str)]) -> Manifest {
        let mut m = manifest(&[]);
        for (k, v) in pairs {
            m.modes.insert(k.to_string(), v.to_string());
        }
        m
    }

    /// [render] の入力は、ignore を書かなくても配置対象から外れる。
    /// 書き忘れると未展開の {{ }} を含むファイルが $HOME に置かれるため。
    #[test]
    fn declared_templates_are_never_placed() {
        let mut m = manifest(&[]);
        m.render.insert("a.conf".into(), "a.conf.tmpl".into());
        assert!(m.is_template(Path::new("a.conf.tmpl")));
        assert!(!m.is_template(Path::new("a.conf")));
    }

    /// 暗号文も同様。平文だけが配置される。
    #[test]
    fn declared_ciphertexts_are_never_placed() {
        let mut m = manifest(&[]);
        m.encrypted.insert("c.conf".into(), "c.conf.age".into());
        assert!(m.is_ciphertext(Path::new("c.conf.age")));
        assert!(!m.is_ciphertext(Path::new("c.conf")));
    }

    #[test]
    fn mode_is_read_as_octal() {
        let m = with_modes(&[(".npmrc", "600")]);
        assert_eq!(m.mode_for(Path::new(".npmrc")), Some(0o600));
    }

    /// 前方一致で最も長い宣言を採る。ディレクトリ全体に指定しつつ、
    /// 書いたパスにだけ効く。ディレクトリの宣言は下へ降りない。
    ///
    /// 降ろしていた頃は `.ssh = "700"` が known_hosts を実行可能にし、
    /// ディレクトリ自体は誰も触らないので verify が永久に落ちていた。
    #[test]
    fn a_declaration_applies_to_the_path_it_names() {
        let m = with_modes(&[(".ssh", "700"), (".ssh/config", "600")]);
        assert_eq!(m.mode_for(Path::new(".ssh")), Some(0o700));
        assert_eq!(m.mode_for(Path::new(".ssh/config")), Some(0o600));
        assert_eq!(m.mode_for(Path::new(".ssh/known_hosts")), None);
    }

    /// 3 桁ちょうどでなければ断る。
    ///
    /// `"60"` は 0o060 と読まれ、所有者が読めないファイルが黙って出来る。
    /// 4 桁は verify が見る 0o777 と食い違って収束しない。
    #[test]
    fn a_mode_must_be_three_octal_digits() {
        for bad in ["2755", "0600", "60", "6"] {
            let toml = format!("[link]\ncommon = []\n\n[modes]\n\"bin/tool\" = \"{bad}\"\n");
            let e = load(&toml).unwrap_err();
            assert!(
                format!("{e:#}").contains("three octal digits"),
                "{bad}: {e:#}"
            );
        }
        assert!(load("[link]\ncommon = []\n\n[modes]\n\"bin/tool\" = \"755\"\n").is_ok());
    }

    #[test]
    fn a_data_file_that_escapes_is_rejected() {
        let e = load("data = [\"../outside.toml\"]\n\n[link]\ncommon = []\n").unwrap_err();
        assert!(format!("{e:#}").contains(".."), "{e:#}");
    }

    #[test]
    fn a_hook_cwd_that_escapes_is_rejected() {
        let e = load("[link]\ncommon = []\n\n[hooks.h]\nrun = \"true\"\ncwd = \"../elsewhere\"\n")
            .unwrap_err();
        assert!(format!("{e:#}").contains(".."), "{e:#}");
    }

    /// 宣言があればそれが勝つ。生成物を書き込み可能にしたい場合の逃げ道。
    #[test]
    fn a_declared_mode_overrides_the_read_only_default() {
        let m = with_modes(&[(".npmrc", "600")]);
        assert_eq!(m.mode_for(Path::new(".npmrc")), Some(0o600));
    }

    #[test]
    fn no_declaration_means_no_opinion() {
        let m = manifest(&[]);
        assert_eq!(m.mode_for(Path::new(".npmrc")), None);
    }

    #[test]
    fn empty_ignore_matches_nothing() {
        let m = manifest(&[]);
        assert!(!m.is_ignored(Path::new(".config/anything")));
    }
}
