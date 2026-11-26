use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// 前回の apply が何を張ったかの記録。
///
/// これが無いと「今回の宣言から外れたが、前回張った symlink」を知る術がなく、
/// 管理をやめたファイルのリンクが $HOME に残り続ける。実際 sennit 導入前に
/// ghostty と gitui の設定を消したとき、リンクは手で消す必要があった。
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct State {
    /// リポジトリルートからの相対パス。$HOME からの相対でもある
    pub links: Vec<PathBuf>,
    /// 退避したファイル。apply の巻き戻しに使う
    #[serde(default)]
    pub backups: Vec<Backup>,
    /// フック名 -> 最後に走らせたときの監視対象の指紋
    #[serde(default)]
    pub hooks: std::collections::BTreeMap<String, String>,
    /// `[merge]` の出力のうち、sennit が書いたことのあるもの。
    ///
    /// 載っていない既存ファイルは利用者のものとして、最初の 1 回だけ退避する。
    #[serde(default)]
    pub merged: Vec<String>,
    /// `links` のうち、$HOME の同じパス以外へ置いたもの: リポジトリ内のパス -> 配置先。
    ///
    /// `[link]` の `to` で置いたもの。載っていなければ `$HOME/<パス>` に置いたとみなす
    /// (これを知らない版が書いた記録も同じに読める)。
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub placed: std::collections::BTreeMap<PathBuf, PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Backup {
    /// 退避元(= 張った symlink の位置)
    pub dest: PathBuf,
    /// 退避先
    pub kept_at: PathBuf,
}

impl State {
    /// 状態ファイルの位置。$HOME 側に置く。リポジトリはマシン間で共有される
    /// が、何を張ったかはマシンごとに違うため。
    pub fn path(home: &Path) -> PathBuf {
        home.join(".local/state/sennit/state.json")
    }

    pub fn load(home: &Path) -> Result<Self> {
        let p = Self::path(home);
        match std::fs::read_to_string(&p) {
            Ok(text) => serde_json::from_str(&text)
                .with_context(|| format!("failed to parse {}", p.display())),
            // 初回は空
            Err(_) => Ok(Self::default()),
        }
    }

    pub fn save(&self, home: &Path) -> Result<()> {
        let p = Self::path(home);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let text = serde_json::to_string_pretty(self)?;
        std::fs::write(&p, text).with_context(|| format!("failed to write {}", p.display()))
    }

    /// 前回そのパスを置いた場所。
    pub fn placed_at(&self, home: &Path, rel: &Path) -> PathBuf {
        self.placed
            .get(rel)
            .cloned()
            .unwrap_or_else(|| home.join(rel))
    }

    /// 前回張った場所のうち、今回は張らない場所: (リポジトリ内のパス, 前回の配置先)。
    ///
    /// 宣言から外れたものに加え、`to` が変わって置き場所が移ったものも含む。
    /// パスだけで比べると、移った先に張り直したあと、元の場所のリンクが残る。
    pub fn stale_dests(
        &self,
        home: &Path,
        current: &[(PathBuf, PathBuf)],
    ) -> Vec<(PathBuf, PathBuf)> {
        self.links
            .iter()
            .filter_map(|old| {
                let was = self.placed_at(home, old);
                let still = current
                    .iter()
                    .any(|(rel, dest)| rel == old && crate::platform::same_path(dest, &was));
                (!still).then(|| (old.clone(), was))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_lists_what_left_the_manifest() {
        let home = PathBuf::from("/h");
        let st = State {
            links: vec![PathBuf::from("a"), PathBuf::from("b")],
            ..Default::default()
        };
        let at = |rels: &[&str]| -> Vec<(PathBuf, PathBuf)> {
            rels.iter()
                .map(|r| (PathBuf::from(r), home.join(r)))
                .collect()
        };
        assert_eq!(
            st.stale_dests(&home, &at(&["a"])),
            vec![(PathBuf::from("b"), home.join("b"))]
        );
        assert!(st.stale_dests(&home, &at(&["a", "b"])).is_empty());
    }

    /// `to` が変わって置き場所が移ったものも、元の場所は stale。
    #[test]
    fn a_link_that_moved_is_stale_where_it_was() {
        let home = PathBuf::from("/h");
        let mut st = State {
            links: vec![PathBuf::from("a"), PathBuf::from("b")],
            ..Default::default()
        };
        st.placed
            .insert(PathBuf::from("b"), PathBuf::from("/docs/b"));
        let now = vec![
            (PathBuf::from("a"), PathBuf::from("/h/a")),
            (PathBuf::from("b"), PathBuf::from("/onedrive/b")),
        ];
        assert_eq!(
            st.stale_dests(&home, &now),
            vec![(PathBuf::from("b"), PathBuf::from("/docs/b"))]
        );
        // 置き場所が同じなら stale ではない
        let same = vec![
            (PathBuf::from("a"), PathBuf::from("/h/a")),
            (PathBuf::from("b"), PathBuf::from("/docs/b")),
        ];
        assert!(st.stale_dests(&home, &same).is_empty());
    }

    /// これを知らない版が書いた記録(placed が無い)も読める。
    #[test]
    fn a_state_without_placed_reads() {
        let st: State = serde_json::from_str(r#"{"links":["a"]}"#).unwrap();
        assert!(st.placed.is_empty());
        assert_eq!(
            st.placed_at(Path::new("/h"), Path::new("a")),
            PathBuf::from("/h/a")
        );
        // 空なら書き出さない。旧い版の読む形が変わらない
        assert!(!serde_json::to_string(&st).unwrap().contains("placed"));
    }

    #[test]
    fn a_missing_state_file_reads_as_empty() {
        let home = std::env::temp_dir().join("sennit-state-empty");
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        let st = State::load(&home).unwrap();
        assert!(st.links.is_empty());
        assert!(st.backups.is_empty());
    }

    #[test]
    fn what_is_saved_comes_back() {
        let home = std::env::temp_dir().join("sennit-state-roundtrip");
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();

        State {
            links: vec![PathBuf::from("a")],
            backups: vec![Backup {
                dest: PathBuf::from("/h/a"),
                kept_at: PathBuf::from("/h/a.sennit-backup"),
            }],
            hooks: Default::default(),
            merged: Default::default(),
            placed: Default::default(),
        }
        .save(&home)
        .unwrap();

        let st = State::load(&home).unwrap();
        assert_eq!(st.links, vec![PathBuf::from("a")]);
        assert_eq!(st.backups.len(), 1);
    }
}
