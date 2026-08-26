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
