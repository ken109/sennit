use crate::manifest::Manifest;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

/// 1 つのリンクについて、あるべき姿と現状の差。
#[derive(Debug, Clone)]
pub struct Entry {
    /// リポジトリルートからの相対パス。$HOME からの相対パスでもある。
    pub rel: PathBuf,
    pub src: PathBuf,
    pub dest: PathBuf,
    pub state: State,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    /// 既に正しい symlink が張られている
    Linked,
    /// 何も無い
    Missing,
    /// 別の場所を指す symlink
    Wrong { current: PathBuf },
    /// symlink ではない実体(ファイル/ディレクトリ)が居座っている
    Occupied,
}

impl State {
    pub fn needs_change(&self) -> bool {
        !matches!(self, State::Linked)
    }
}

pub struct Plan {
    pub entries: Vec<Entry>,
}

impl Plan {
    /// マニフェストとリポジトリの実体から、あるべきリンク一覧を組み立てる。
    ///
    /// ディレクトリはその下のファイル単位で symlink する。ディレクトリごと
    /// symlink すると、他のツールがそこへ書き込んだファイルまでリポジトリに
    /// 現れてしまうため。
    pub fn build(root: &Path, home: &Path, manifest: &Manifest) -> Result<Self> {
        let mut entries = Vec::new();

        for target in manifest.targets() {
            let abs = root.join(target);
            if !abs.exists() {
                continue;
            }

            if abs.is_file() {
                Self::push(&mut entries, root, home, manifest, Path::new(target))?;
                continue;
            }

            for e in WalkDir::new(&abs).into_iter().filter_map(Result::ok) {
                if !e.file_type().is_file() {
                    continue;
                }
                let rel = e
                    .path()
                    .strip_prefix(root)
                    .context("path escaped repository root")?;
                Self::push(&mut entries, root, home, manifest, rel)?;
            }
        }

        // 生成物はまだディスクに無いことがある。クローン直後の diff / list が
        // 「これから作られて張られるもの」を数えないと、apply の結果と食い違う。
        // 宣言されている出力は、実体の有無に関わらず対象として数える。
        for out in manifest.render.keys().chain(manifest.encrypted.keys()) {
            let rel = Path::new(out);
            if entries.iter().any(|e| e.rel == rel) {
                continue;
            }
            if !manifest.targets().iter().any(|t| {
                let t = Path::new(t);
                rel == t || rel.starts_with(t)
            }) {
                continue;
            }
            Self::push(&mut entries, root, home, manifest, rel)?;
        }

        entries.sort_by(|a, b| a.rel.cmp(&b.rel));
        Ok(Plan { entries })
    }

    fn push(
        entries: &mut Vec<Entry>,
        root: &Path,
        home: &Path,
        manifest: &Manifest,
        rel: &Path,
    ) -> Result<()> {
        // テンプレートは生成の入力であって配置対象ではない。ignore の宣言に
        // 頼ると書き忘れたときに、未展開の {{ }} を含むファイルがそのまま
        // $HOME に置かれる。[render] の入力は自明に対象外なので自動で外す。
        if manifest.is_template(rel) || manifest.is_ciphertext(rel) || manifest.is_ignored(rel) {
            return Ok(());
        }
        let src = root.join(rel);
        let dest = home.join(rel);
        let state = inspect(&src, &dest);
        entries.push(Entry {
            rel: rel.to_path_buf(),
            src,
            dest,
            state,
        });
        Ok(())
    }

    pub fn changes(&self) -> impl Iterator<Item = &Entry> {
        self.entries.iter().filter(|e| e.state.needs_change())
    }
}

/// dest の現状を分類する。symlink_metadata なので、リンク先の有無に
/// 引きずられずリンクそのものを見る。
fn inspect(src: &Path, dest: &Path) -> State {
    match std::fs::symlink_metadata(dest) {
        Err(_) => State::Missing,
        Ok(meta) => {
            if !meta.file_type().is_symlink() {
                return State::Occupied;
            }
            match std::fs::read_link(dest) {
                Ok(current) if current == src => State::Linked,
                Ok(current) => State::Wrong { current },
                Err(_) => State::Occupied,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("sennit-plan-{name}"));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn write(path: &Path, body: &str) {
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p).unwrap();
        }
        std::fs::write(path, body).unwrap();
    }

    #[test]
    fn inspect_tells_the_four_states_apart() {
        let d = scratch("inspect");
        let src = d.join("src.conf");
        write(&src, "x");

        let missing = d.join("missing.conf");
        assert_eq!(inspect(&src, &missing), State::Missing);

        let occupied = d.join("occupied.conf");
        write(&occupied, "mine");
        assert_eq!(inspect(&src, &occupied), State::Occupied);

        let linked = d.join("linked.conf");
        std::os::unix::fs::symlink(&src, &linked).unwrap();
        assert_eq!(inspect(&src, &linked), State::Linked);

        let elsewhere = d.join("elsewhere.conf");
        write(&elsewhere, "y");
        let wrong = d.join("wrong.conf");
        std::os::unix::fs::symlink(&elsewhere, &wrong).unwrap();
        assert_eq!(inspect(&src, &wrong), State::Wrong { current: elsewhere });
    }

    /// リンク先が消えていても、リンクそのものは張られている。
    /// メタデータを追いかけると Missing に見えてしまう。
    #[test]
    fn a_link_to_a_deleted_file_is_still_a_link() {
        let d = scratch("broken");
        let src = d.join("src.conf");
        write(&src, "x");
        let dest = d.join("dest.conf");
        std::os::unix::fs::symlink(&src, &dest).unwrap();
        std::fs::remove_file(&src).unwrap();

        assert_eq!(inspect(&src, &dest), State::Linked);
    }

    fn manifest_from(toml: &str) -> Manifest {
        toml::from_str(toml).unwrap()
    }

    /// クローン直後は生成物がまだ無い。それを数えないと diff / list が
    /// apply より少ない件数を出す。
    #[test]
    fn a_declared_output_is_planned_before_it_exists() {
        let d = scratch("planned-output");
        let root = d.join("repo");
        let home = d.join("home");
        std::fs::create_dir_all(&home).unwrap();
        write(&root.join("conf/app.conf.tmpl"), "x = {{ ui.bg }}\n");

        let m = manifest_from(
            r#"
[link]
common = ["conf"]
ignore = ["*.tmpl"]

[render]
"conf/app.conf" = "conf/app.conf.tmpl"
"#,
        );
        let plan = Plan::build(&root, &home, &m).unwrap();
        let rels: Vec<_> = plan.entries.iter().map(|e| e.rel.clone()).collect();
        assert_eq!(rels, vec![PathBuf::from("conf/app.conf")]);
        assert_eq!(plan.entries[0].state, State::Missing);
    }

    /// 出力が [link] の対象外なら、宣言されていても配置しない。
    #[test]
    fn an_output_outside_the_link_targets_is_not_planned() {
        let d = scratch("planned-outside");
        let root = d.join("repo");
        let home = d.join("home");
        std::fs::create_dir_all(&home).unwrap();
        write(&root.join("elsewhere/app.conf.tmpl"), "x\n");

        let m = manifest_from(
            r#"
[link]
common = ["conf"]

[render]
"elsewhere/app.conf" = "elsewhere/app.conf.tmpl"
"#,
        );
        let plan = Plan::build(&root, &home, &m).unwrap();
        assert!(plan.entries.is_empty());
    }
}
