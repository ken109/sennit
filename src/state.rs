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
    /// 張ったファイルの、リポジトリルートからの相対パス。
    ///
    /// `placed` に載っていなければ、$HOME からの相対でもある
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
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            // 読めないだけのものを「初回」と読むと、apply がそれを空の記録で
            // 書き潰し、退避の在り処も張ったリンクの記録も失われる
            Err(err) => Err(anyhow::Error::new(err))
                .with_context(|| format!("failed to read {}", p.display())),
        }
    }

    /// 書き出す。一時ファイルに書いてから置き換えるので、途中で落ちても
    /// 前の記録か新しい記録のどちらかが残る。
    ///
    /// 直に書くと、切り詰めたあとで落ちた記録は JSON として読めず、次の apply が
    /// 「壊れている」で止まる。退避の在り処もそこにしか書いていない。
    pub fn save(&self, home: &Path) -> Result<()> {
        let p = Self::path(home);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let text = serde_json::to_string_pretty(self)?;
        let tmp = p.with_extension(format!("json.tmp.{}", std::process::id()));
        std::fs::write(&tmp, text).with_context(|| format!("failed to write {}", tmp.display()))?;
        std::fs::rename(&tmp, &p).map_err(|err| {
            let _ = std::fs::remove_file(&tmp);
            anyhow::Error::new(err).context(format!("failed to write {}", p.display()))
        })
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
    ///
    /// 今回の項目のどれかが同じ場所に置くなら stale ではない(改名・入れ替え・
    /// 平の項目を外して `to` で同じ場所へ置き直す)。そこは張り替えの対象で、
    /// 先に prune すると、張り替えが消えたリンクを外しにいって落ちる。
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
                    .any(|(_, dest)| crate::platform::same_path(dest, &was));
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
        // 別の項目がその場所を使うなら、張り替えであって prune ではない
        let reused = vec![(PathBuf::from("c"), PathBuf::from("/docs/b"))];
        assert_eq!(
            st.stale_dests(&home, &reused),
            vec![(PathBuf::from("a"), PathBuf::from("/h/a"))]
        );
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

    /// 読めない記録を初回と取り違えない。空で書き潰すと退避の在り処が消える。
    #[test]
    fn an_unreadable_state_is_an_error_not_an_empty_one() {
        let home = std::env::temp_dir().join("sennit-state-unreadable");
        let _ = std::fs::remove_dir_all(&home);
        // ファイルの代わりにディレクトリが在る: 読めるが、中身として読めない
        std::fs::create_dir_all(State::path(&home)).unwrap();
        let e = State::load(&home).unwrap_err();
        assert!(format!("{e:#}").contains("failed to read"), "{e:#}");
    }

    /// 書き出しは一時ファイルを経る。終わったあとに一時ファイルが残らない。
    #[test]
    fn saving_leaves_no_temporary_file_behind() {
        let home = std::env::temp_dir().join("sennit-state-atomic");
        let _ = std::fs::remove_dir_all(&home);
        State::default().save(&home).unwrap();
        State::default().save(&home).unwrap();
        let dir = State::path(&home).parent().unwrap().to_path_buf();
        let names: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["state.json".to_string()]);
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
