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

    /// 前回張ったが今回は対象外になったもの。
    pub fn stale(&self, current: &[PathBuf]) -> Vec<PathBuf> {
        self.links
            .iter()
            .filter(|old| !current.contains(old))
            .cloned()
            .collect()
    }
}
