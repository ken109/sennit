use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};

/// sennit.toml の表現。
///
/// 知らないキーは受け付けない。`when_changed` を `when-changed` の代わりに
/// 書くと、監視付きのフックが毎回走るフックに黙って変わる。綴りの誤りが
/// 無言の挙動変化になるくらいなら、読み込みで落とす方がよい。
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub link: Link,
    /// 出力パス -> テンプレートパス(か、`{ from = "...", os = [...] }`)
    #[serde(default)]
    pub render: std::collections::BTreeMap<String, Render>,
    /// 配置後に走らせる処理
    #[serde(default)]
    pub hooks: std::collections::BTreeMap<String, crate::hooks::Hook>,
    /// テンプレートに渡すデータファイル。既定は theme.toml。
    #[serde(default)]
    pub data: Vec<String>,
    /// 秘密の取り出し方。scheme -> コマンド。宣言が無ければ op のみ。
    #[serde(default)]
    pub providers: crate::render::Providers,
    /// 秘密がまだ効くかの確かめ方。`verify --secrets` が走らせる。
    #[serde(default)]
    pub probes: std::collections::BTreeMap<String, crate::probe::Probe>,
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
    /// 出力パス -> 重ねる共有設定。出力は $HOME からの相対（link と同じ規則）。
    ///
    /// 出力は symlink にしない。マシン固有の設定が同居し、アプリ自身も
    /// 書き換えるファイル（Claude Code の settings.json など）のためのもの。
    #[serde(default)]
    pub merge: std::collections::BTreeMap<String, Merge>,
}

/// `[merge."<output>"]` の中身。
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Merge {
    /// リポジトリ内の JSON。出力へ重ねる側。
    pub from: String,
}

/// `[render]` の値。テンプレートのパスだけか、生成する OS を絞った表。
#[derive(Debug)]
pub enum Render {
    Template(String),
    Table(RenderTable),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RenderTable {
    /// テンプレートのパス
    pub from: String,
    /// 空なら全 OS。packages.toml の `os` と同じ綴り
    #[serde(default)]
    pub os: Vec<String>,
}

impl crate::de::ShortOrTable for Render {
    type Short = String;
    type Table = RenderTable;
    const EXPECTING: &'static str = "a template path, or a table with `from` and `os`";
    fn short(s: String) -> Self {
        Render::Template(s)
    }
    fn table(t: RenderTable) -> Self {
        Render::Table(t)
    }
}
crate::short_or_table!(Render);

impl Render {
    pub fn template(&self) -> &str {
        match self {
            Render::Template(t) => t,
            Render::Table(t) => &t.from,
        }
    }

    pub fn os(&self) -> &[String] {
        match self {
            Render::Template(_) => &[],
            Render::Table(t) => &t.os,
        }
    }

    /// この OS で生成するか。
    pub fn here(&self) -> bool {
        on_this_os(self.os())
    }
}

impl From<&str> for Render {
    fn from(t: &str) -> Self {
        Render::Template(t.to_string())
    }
}

/// `os` が空か、今の OS を含むか。
pub fn on_this_os(os: &[String]) -> bool {
    os.is_empty() || os.iter().any(|o| o == crate::packages::current_os())
}

/// `os` に書ける綴りか。
pub fn validate_os(what: &str, os: &[String]) -> Result<()> {
    for o in os {
        if !matches!(o.as_str(), "darwin" | "linux" | "windows") {
            bail!("{what} has an unknown os `{o}`; expected darwin, linux or windows");
        }
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Link {
    #[serde(default)]
    pub common: Vec<Target>,
    #[serde(default)]
    pub darwin: Vec<Target>,
    #[serde(default)]
    pub linux: Vec<Target>,
    #[serde(default)]
    pub windows: Vec<Target>,
    #[serde(default)]
    pub ignore: Ignore,
}

/// `[link]` の一覧の 1 項目。リポジトリ内のパスだけか、配置先を添えた表。
///
/// 配置先を書かなければ、リポジトリ内のパスがそのまま $HOME からの相対パスになる。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// リポジトリ内のパス
    pub path: String,
    /// 配置先。$HOME からの相対か、`{appdata}/herdr` のように既知のフォルダから。
    pub to: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TargetTable {
    pub path: String,
    pub to: String,
}

impl crate::de::ShortOrTable for Target {
    type Short = String;
    type Table = TargetTable;
    const EXPECTING: &'static str = "a path, or a table with `path` and `to`";
    fn short(s: String) -> Self {
        Target { path: s, to: None }
    }
    fn table(t: TargetTable) -> Self {
        Target {
            path: t.path,
            to: Some(t.to),
        }
    }
}
crate::short_or_table!(Target);

impl From<&str> for Target {
    fn from(s: &str) -> Self {
        Target {
            path: s.to_string(),
            to: None,
        }
    }
}

/// `to` を「既知のフォルダ」と「その下のパス」に分ける。
/// `{documents}/PowerShell` -> (Some("documents"), "PowerShell")、`a/b` -> (None, "a/b")
pub fn split_to(to: &str) -> (Option<&str>, &str) {
    if let Some(rest) = to.strip_prefix('{') {
        if let Some(end) = rest.find('}') {
            let name = &rest[..end];
            let after = &rest[end + 1..];
            return (Some(name), after.trim_start_matches('/'));
        }
    }
    (None, to)
}

/// 配置先。dest = base.join(under)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Placement {
    pub base: PathBuf,
    pub under: PathBuf,
}

/// `[link] ignore`。全 OS 共通の配列か、`[link.ignore]` の OS ごとの表。
#[derive(Debug)]
pub enum Ignore {
    List(Vec<String>),
    ByOs(IgnoreByOs),
}

impl Default for Ignore {
    fn default() -> Self {
        Ignore::List(Vec::new())
    }
}

/// `[link.ignore]`。キーは `[link]` の配置の一覧と同じ。
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IgnoreByOs {
    #[serde(default)]
    pub common: Vec<String>,
    #[serde(default)]
    pub darwin: Vec<String>,
    #[serde(default)]
    pub linux: Vec<String>,
    #[serde(default)]
    pub windows: Vec<String>,
}

impl crate::de::ShortOrTable for Ignore {
    type Short = Vec<String>;
    type Table = IgnoreByOs;
    const EXPECTING: &'static str =
        "an array of patterns, or a table of them keyed by common, darwin, linux and windows";
    fn short(s: Vec<String>) -> Self {
        Ignore::List(s)
    }
    fn table(t: IgnoreByOs) -> Self {
        Ignore::ByOs(t)
    }
}
crate::short_or_table!(Ignore);

impl Ignore {
    /// この OS で効くパターン。
    pub fn here(&self) -> Vec<&str> {
        match self {
            Ignore::List(v) => v.iter().map(String::as_str).collect(),
            Ignore::ByOs(b) => b
                .common
                .iter()
                .chain(for_this_os(&b.darwin, &b.linux, &b.windows))
                .map(String::as_str)
                .collect(),
        }
    }
}

/// OS ごとの 3 つの一覧から、今の OS のものを選ぶ。
fn for_this_os<'a, T>(darwin: &'a [T], linux: &'a [T], windows: &'a [T]) -> &'a [T] {
    if cfg!(target_os = "macos") {
        darwin
    } else if cfg!(windows) {
        windows
    } else {
        linux
    }
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
        for (list, items) in [
            ("common", &self.link.common),
            ("darwin", &self.link.darwin),
            ("linux", &self.link.linux),
            ("windows", &self.link.windows),
        ] {
            for t in items {
                paths.push(("link", &t.path));
                let Some(to) = &t.to else { continue };
                let (folder, rest) = split_to(to);
                if let Some(f) = folder {
                    if !crate::platform::KNOWN_FOLDERS.contains(&f) {
                        bail!(
                            "link `{}` is placed under an unknown folder `{{{f}}}`; expected one of {}",
                            t.path,
                            crate::platform::KNOWN_FOLDERS
                                .iter()
                                .map(|f| format!("{{{f}}}"))
                                .collect::<Vec<_>>()
                                .join(", ")
                        );
                    }
                    // %APPDATA% や Documents は Windows のもの。macOS で ~/AppData を作らない
                    if crate::platform::windows_only_folder(f) && list != "windows" {
                        bail!(
                            "link `{}` is placed under `{{{f}}}`, a Windows folder; \
                             declare it in the `windows` list",
                            t.path
                        );
                    }
                    if !rest.is_empty() {
                        paths.push(("link destination", rest));
                    }
                } else {
                    paths.push(("link destination", rest));
                }
            }
        }
        for (out, r) in &self.render {
            paths.push(("render output", out));
            paths.push(("render template", r.template()));
        }
        for (out, src) in &self.encrypted {
            paths.push(("encrypted output", out));
            paths.push(("encrypted source", src));
        }
        for p in self.modes.keys() {
            paths.push(("modes", p));
        }
        for (out, m) in &self.merge {
            paths.push(("merge output", out));
            paths.push(("merge source", &m.from));
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
            // On Windows `\foo` and `C:foo` are not absolute but still escape the
            // directory they are joined to
            if path.is_absolute()
                || path.has_root()
                || path
                    .components()
                    .any(|c| matches!(c, std::path::Component::Prefix(_)))
            {
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

        self.validate_merge()?;

        for (out, r) in &self.render {
            validate_os(&format!("render output `{out}`"), r.os())?;
        }

        for (name, probe) in &self.probes {
            probe.validate(name)?;
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

    /// [merge] の出力が、ほかの宣言と同じパスを取り合っていないか。
    ///
    /// 取り合うと、片方が張った symlink をもう片方が JSON として読んで書き直す。
    /// symlink を書き換えるとリポジトリの中身が変わるので、宣言の時点で断る。
    fn validate_merge(&self) -> Result<()> {
        let outs: Vec<&str> = self.merge.keys().map(String::as_str).collect();
        for (i, a) in outs.iter().enumerate() {
            for b in &outs[i + 1..] {
                let (pa, pb) = (Path::new(a), Path::new(b));
                if pa.starts_with(pb) || pb.starts_with(pa) {
                    bail!("merge outputs `{a}` and `{b}` overlap");
                }
            }
        }
        for out in &outs {
            let rel = Path::new(out);
            if self
                .link
                .common
                .iter()
                .chain(&self.link.darwin)
                .chain(&self.link.linux)
                .chain(&self.link.windows)
                .any(|t| {
                    Path::new(&t.path) == rel
                        || t.to.as_deref().is_some_and(|to| {
                            matches!(split_to(to), (None | Some("home"), r) if Path::new(r) == rel)
                        })
                })
            {
                bail!("merge output `{out}` is also declared under [link]; sennit would both link and rewrite it");
            }
            if self.render.contains_key(*out) || self.encrypted.contains_key(*out) {
                bail!("merge output `{out}` is also a [render] or [encrypted] output");
            }
        }
        for (out, m) in &self.merge {
            if Path::new(&m.from) == Path::new(out.as_str()) {
                bail!("merge output `{out}` is its own source");
            }
        }
        Ok(())
    }

    /// 現在の OS で配置対象になるトップレベルのパス。
    pub fn targets(&self) -> Vec<&str> {
        self.targets_here().map(|t| t.path.as_str()).collect()
    }

    /// 現在の OS で配置対象になる項目。common が先。
    pub fn targets_here(&self) -> impl Iterator<Item = &Target> {
        self.link.common.iter().chain(for_this_os(
            &self.link.darwin,
            &self.link.linux,
            &self.link.windows,
        ))
    }

    /// リポジトリ内のパス `rel` をどこへ置くか。
    ///
    /// `to` のある項目のうち、`rel` を含むいちばん深いものに従う。無ければ
    /// $HOME の下の同じパス。
    pub fn placement(&self, dirs: &crate::platform::Dirs, rel: &Path) -> Result<Placement> {
        let mapped = self
            .targets_here()
            .filter(|t| t.to.is_some() && rel.starts_with(&t.path))
            .max_by_key(|t| Path::new(&t.path).components().count());
        let Some(t) = mapped else {
            return Ok(Placement {
                base: dirs.home.clone(),
                under: rel.to_path_buf(),
            });
        };
        let (base, top) = Self::base_of(dirs, t)?;
        let below = rel.strip_prefix(&t.path).unwrap_or(Path::new(""));
        Ok(Placement {
            base,
            under: if below.as_os_str().is_empty() {
                top
            } else {
                top.join(below)
            },
        })
    }

    /// 項目の配置先の根と、そこからの相対パス(項目そのもの)。
    fn base_of(dirs: &crate::platform::Dirs, t: &Target) -> Result<(PathBuf, PathBuf)> {
        match t.to.as_deref() {
            None => Ok((dirs.home.clone(), PathBuf::from(&t.path))),
            Some(to) => {
                let (folder, rest) = split_to(to);
                let base = match folder {
                    Some(f) => dirs.known(f)?,
                    None => dirs.home.clone(),
                };
                Ok((base, PathBuf::from(rest)))
            }
        }
    }

    /// 現在の OS の各項目の (配置先の根, 根からの相対パス)。
    /// 祖先の symlink をどこまで張り替えるかの判断に使う。
    pub fn tops(&self, dirs: &crate::platform::Dirs) -> Result<Vec<(PathBuf, PathBuf)>> {
        self.targets_here()
            .map(|t| Self::base_of(dirs, t))
            .collect()
    }

    /// この OS で生成する [render] の 出力 -> テンプレート。
    pub fn renders_here(&self) -> Vec<(&str, &str)> {
        self.render
            .iter()
            .filter(|(_, r)| r.here())
            .map(|(out, r)| (out.as_str(), r.template()))
            .collect()
    }

    /// [render] の入力として宣言されているか。OS を問わない。
    /// 別の OS 向けのテンプレートも、ここで配置してよいものではない。
    pub fn is_template(&self, rel: &Path) -> bool {
        self.render.values().any(|r| Path::new(r.template()) == rel)
    }

    /// 別の OS でだけ生成する [render] の出力か。
    ///
    /// この OS では作らないので、リポジトリに残っていても(共有した作業ツリー、
    /// 以前の生成物)配置しない。古いままのファイルを張ることになる。
    pub fn is_foreign_output(&self, rel: &Path) -> bool {
        self.render
            .iter()
            .any(|(out, r)| !r.here() && Path::new(out) == rel)
    }

    /// [encrypted] の入力として宣言されているか。暗号文も配置しない。
    pub fn is_ciphertext(&self, rel: &Path) -> bool {
        self.encrypted.values().any(|t| Path::new(t) == rel)
    }

    /// [merge] の入力として宣言されているか。配置せず、$HOME にも出さない。
    pub fn is_merge_source(&self, rel: &Path) -> bool {
        self.merge.values().any(|m| Path::new(&m.from) == rel)
    }

    /// [merge] の出力として宣言されているか。
    pub fn is_merge_output(&self, rel: &Path) -> bool {
        self.merge.keys().any(|o| Path::new(o) == rel)
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

    /// ignore に当たるか。パターンの書き方は `glob` モジュールを参照。
    ///
    /// - スラッシュを含む（`.claude/worktrees`, `**/node_modules`）: 根から
    /// - スラッシュの無いワイルドカード（`*.test.ts`）: どの深さにも
    /// - スラッシュもワイルドカードも無い名前（`README.md`）: 根からの前方一致
    ///
    /// ディレクトリに当たれば、その下もすべて当たる。
    pub fn is_ignored(&self, rel: &Path) -> bool {
        self.link
            .ignore
            .here()
            .iter()
            .any(|pat| crate::glob::Pattern::new(pat).matches(rel))
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
                windows: vec![],
                ignore: Ignore::List(ignore.iter().map(|s| s.to_string()).collect()),
            },
            render: Default::default(),
            hooks: Default::default(),
            providers: Default::default(),
            probes: Default::default(),
            encrypted: Default::default(),
            encryption: None,
            data: Default::default(),
            modes: Default::default(),
            merge: Default::default(),
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
        assert!(!m.is_foreign_output(Path::new("a.conf")));
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

    #[test]
    fn ignore_takes_globs() {
        let m = manifest(&[
            "*.test.ts",
            "**/node_modules",
            ".claude/worktrees",
            "*.tar.gz",
        ]);
        assert!(m.is_ignored(Path::new(".claude/hooks/a.test.ts")));
        assert!(!m.is_ignored(Path::new(".claude/hooks/a.ts")));
        assert!(m.is_ignored(Path::new(".claude/hooks/node_modules/x/y.js")));
        assert!(m.is_ignored(Path::new(".claude/worktrees/a/b.md")));
        assert!(!m.is_ignored(Path::new(".claude/worktrees-old/b.md")));
        // used to match nothing: only the last extension was looked at
        assert!(m.is_ignored(Path::new("pkg/x.tar.gz")));
    }

    #[test]
    fn a_merge_source_is_never_placed_and_an_output_is_recognised() {
        let m = load(
            "[link]\ncommon = [\".claude\"]\n\n[merge.\".claude/settings.json\"]\nfrom = \".claude/settings.shared.json\"\n",
        )
        .unwrap();
        assert!(m.is_merge_source(Path::new(".claude/settings.shared.json")));
        assert!(!m.is_merge_source(Path::new(".claude/settings.json")));
        assert!(m.is_merge_output(Path::new(".claude/settings.json")));
    }

    #[test]
    fn a_merge_output_that_is_also_linked_is_rejected() {
        let e = load(
            "[link]\ncommon = [\".claude/settings.json\"]\n\n[merge.\".claude/settings.json\"]\nfrom = \"s.json\"\n",
        )
        .unwrap_err();
        assert!(format!("{e:#}").contains("[link]"), "{e:#}");
    }

    #[test]
    fn a_merge_output_that_is_also_rendered_is_rejected() {
        let e = load(
            "[link]\ncommon = []\n\n[render]\n\"a.json\" = \"a.json.tmpl\"\n\n[merge.\"a.json\"]\nfrom = \"s.json\"\n",
        )
        .unwrap_err();
        assert!(format!("{e:#}").contains("[render]"), "{e:#}");
    }

    #[test]
    fn overlapping_merge_outputs_are_rejected() {
        let e = load(
            "[link]\ncommon = []\n\n[merge.\"a\"]\nfrom = \"x.json\"\n\n[merge.\"a/b\"]\nfrom = \"y.json\"\n",
        )
        .unwrap_err();
        assert!(format!("{e:#}").contains("overlap"), "{e:#}");
    }

    #[test]
    fn a_merge_path_that_escapes_is_rejected() {
        let e =
            load("[link]\ncommon = []\n\n[merge.\"../x.json\"]\nfrom = \"s.json\"\n").unwrap_err();
        assert!(format!("{e:#}").contains(".."), "{e:#}");
        let e = load("[link]\ncommon = []\n\n[merge.\"x.json\"]\nfrom = \"/etc/s.json\"\n")
            .unwrap_err();
        assert!(format!("{e:#}").contains("absolute"), "{e:#}");
    }

    /// `[link.ignore]` は OS ごとに書ける。common は全 OS に効く。
    #[test]
    fn ignore_can_be_written_per_os() {
        let m = load(
            "[link]\ncommon = [\".config\"]\n\n[link.ignore]\ncommon = [\"*.tmpl\"]\ndarwin = [\".config/d\"]\nlinux = [\".config/l\"]\nwindows = [\".config/w\"]\n",
        )
        .unwrap();
        assert!(m.is_ignored(Path::new(".config/a.tmpl")));
        let here = if cfg!(target_os = "macos") {
            "d"
        } else if cfg!(windows) {
            "w"
        } else {
            "l"
        };
        for os in ["d", "l", "w"] {
            let p = format!(".config/{os}/x");
            assert_eq!(m.is_ignored(Path::new(&p)), os == here, "{p}");
        }
    }

    /// 配列の書き方はそのまま通る。
    #[test]
    fn the_ignore_array_still_applies_everywhere() {
        let m = load("[link]\ncommon = []\nignore = [\"*.tmpl\"]\n").unwrap();
        assert!(m.is_ignored(Path::new("a.tmpl")));
    }

    #[test]
    fn an_unknown_os_under_link_ignore_is_rejected() {
        let e = load("[link]\ncommon = []\n\n[link.ignore]\nmacos = [\"x\"]\n").unwrap_err();
        assert!(format!("{e:#}").contains("unknown field `macos`"), "{e:#}");
    }

    /// `os` を付けた [render] は、その OS でだけ生成し配置する。テンプレートは
    /// どの OS でも配置しない。
    #[test]
    fn a_render_can_be_limited_to_some_os() {
        let m = load(
            "[link]\ncommon = [\"c\"]\n\n[render]\n\"c/all.conf\" = \"c/all.conf.tmpl\"\n\"c/d.conf\" = { from = \"c/d.conf.tmpl\", os = [\"darwin\"] }\n\"c/w.conf\" = { from = \"c/w.conf.tmpl\", os = [\"windows\"] }\n",
        )
        .unwrap();
        let here: Vec<&str> = m.renders_here().into_iter().map(|(o, _)| o).collect();
        assert!(here.contains(&"c/all.conf"));
        assert_eq!(here.contains(&"c/d.conf"), cfg!(target_os = "macos"));
        assert_eq!(here.contains(&"c/w.conf"), cfg!(windows));
        assert_eq!(m.is_foreign_output(Path::new("c/w.conf")), !cfg!(windows));
        assert!(!m.is_foreign_output(Path::new("c/all.conf")));
        for t in ["c/all.conf.tmpl", "c/d.conf.tmpl", "c/w.conf.tmpl"] {
            assert!(m.is_template(Path::new(t)), "{t}");
        }
    }

    #[test]
    fn a_render_with_an_unknown_os_or_key_is_rejected() {
        let e = load(
            "[link]\ncommon = []\n\n[render]\n\"a\" = { from = \"a.tmpl\", os = [\"macos\"] }\n",
        )
        .unwrap_err();
        assert!(format!("{e:#}").contains("unknown os"), "{e:#}");
        let e = load("[link]\ncommon = []\n\n[render]\n\"a\" = { template = \"a.tmpl\" }\n")
            .unwrap_err();
        assert!(
            format!("{e:#}").contains("unknown field `template`"),
            "{e:#}"
        );
        let e = load("[link]\ncommon = []\n\n[render]\n\"a\" = { from = \"../a.tmpl\" }\n")
            .unwrap_err();
        assert!(format!("{e:#}").contains(".."), "{e:#}");
    }

    #[test]
    fn to_is_split_into_a_folder_and_a_path() {
        assert_eq!(
            split_to("{documents}/PowerShell"),
            (Some("documents"), "PowerShell")
        );
        assert_eq!(split_to("{appdata}"), (Some("appdata"), ""));
        assert_eq!(split_to("AppData/Roaming/x"), (None, "AppData/Roaming/x"));
    }

    /// `to` で、リポジトリ内のパスと別の場所へ置ける。いちばん深い項目に従う。
    #[test]
    fn a_target_can_be_placed_elsewhere() {
        let m = load(
            "[link]\ncommon = [\"conf\", { path = \"win/herdr\", to = \"AppData/Roaming/herdr\" }, { path = \"conf/deep\", to = \"{home}/elsewhere\" }]\n",
        )
        .unwrap();
        let dirs = crate::platform::Dirs::new(PathBuf::from("/h"), false);
        let at = |rel: &str| {
            let p = m.placement(&dirs, Path::new(rel)).unwrap();
            p.base.join(p.under)
        };
        assert_eq!(at("conf/a"), PathBuf::from("/h/conf/a"));
        assert_eq!(
            at("win/herdr/config.toml"),
            PathBuf::from("/h/AppData/Roaming/herdr/config.toml")
        );
        assert_eq!(at("conf/deep/x"), PathBuf::from("/h/elsewhere/x"));
        assert_eq!(m.targets(), vec!["conf", "win/herdr", "conf/deep"]);
    }

    /// 試しの --home では、既知のフォルダはその下の既定の場所。
    #[test]
    fn known_folders_default_to_their_place_under_a_given_home() {
        let dirs = crate::platform::Dirs::new(PathBuf::from("/h"), false);
        assert_eq!(dirs.known("home").unwrap(), PathBuf::from("/h"));
        assert_eq!(
            dirs.known("appdata").unwrap(),
            PathBuf::from("/h").join("AppData").join("Roaming")
        );
        assert_eq!(
            dirs.known("localappdata").unwrap(),
            PathBuf::from("/h").join("AppData").join("Local")
        );
        assert_eq!(
            dirs.known("documents").unwrap(),
            PathBuf::from("/h").join("Documents")
        );
        assert!(dirs.known("desktop").is_err());
    }

    #[test]
    fn a_bad_destination_is_rejected() {
        for (toml, want) in [
            (
                "common = [{ path = \"a\", to = \"{appdata}/a\" }]",
                "a Windows folder",
            ),
            (
                "darwin = [{ path = \"a\", to = \"{documents}/a\" }]",
                "a Windows folder",
            ),
            (
                "windows = [{ path = \"a\", to = \"{desktop}/a\" }]",
                "unknown folder",
            ),
            (
                "windows = [{ path = \"a\", to = \"{appdata}/../a\" }]",
                "..",
            ),
            ("common = [{ path = \"a\", to = \"/etc/a\" }]", "absolute"),
            ("common = [{ path = \"a\", to = \"\" }]", "empty"),
            (
                "common = [{ path = \"a\", dest = \"b\" }]",
                "unknown field `dest`",
            ),
            ("common = [{ path = \"a\" }]", "missing field `to`"),
        ] {
            let e = load(&format!("[link]\n{toml}\n")).unwrap_err();
            assert!(format!("{e:#}").contains(want), "{toml}: {e:#}");
        }
        assert!(load("[link]\nwindows = [{ path = \"a\", to = \"{appdata}/a\" }]\n").is_ok());
        assert!(load("[link]\nwindows = [{ path = \"a\", to = \"{documents}\" }]\n").is_ok());
    }

    #[test]
    fn a_merge_output_that_a_link_is_placed_at_is_rejected() {
        let e = load(
            "[link]\ncommon = [{ path = \"s\", to = \".claude/settings.json\" }]\n\n[merge.\".claude/settings.json\"]\nfrom = \"s.json\"\n",
        )
        .unwrap_err();
        assert!(format!("{e:#}").contains("[link]"), "{e:#}");
    }

    #[test]
    fn an_unknown_merge_key_is_rejected() {
        // `own` is not supported yet; silently ignoring it would be worse
        let e =
            load("[link]\ncommon = []\n\n[merge.\"x.json\"]\nfrom = \"s.json\"\nown = [\"a\"]\n")
                .unwrap_err();
        assert!(format!("{e:#}").contains("own"), "{e:#}");
    }
}
