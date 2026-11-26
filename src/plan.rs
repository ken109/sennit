use crate::manifest::Manifest;
use crate::platform::Dirs;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

/// 1 つのリンクについて、あるべき姿と現状の差。
#[derive(Debug, Clone)]
pub struct Entry {
    /// リポジトリルートからの相対パス。`to` の無い項目では $HOME からの相対パスでもある。
    pub rel: PathBuf,
    pub src: PathBuf,
    pub dest: PathBuf,
    /// dest = base.join(under)。base は $HOME か、`to` が名指した既知のフォルダ
    pub base: PathBuf,
    pub under: PathBuf,
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
    /// dest の祖先に symlink がある。向き先はリポジトリの中でも外でもよい。
    ///
    /// 旧い配置でディレクトリごと張ったものが典型。dest はその symlink を
    /// 通ってリポジトリの実ファイルに着くので、そのまま進めるとリポジトリの
    /// ファイルを退避するか、自分自身を指すリンクを作る。祖先の symlink だけを
    /// 外して実ディレクトリにし、その下に 1 ファイルずつ張り直す。
    Ancestor { link: PathBuf },
}

impl State {
    pub fn needs_change(&self) -> bool {
        !matches!(self, State::Linked)
    }
}

pub struct Plan {
    pub entries: Vec<Entry>,
    /// 各項目の (配置先の根, 根からの相対パス)
    tops: Vec<(PathBuf, PathBuf)>,
}

impl Plan {
    /// マニフェストとリポジトリの実体から、あるべきリンク一覧を組み立てる。
    ///
    /// ディレクトリはその下のファイル単位で symlink する。ディレクトリごと
    /// symlink すると、他のツールがそこへ書き込んだファイルまでリポジトリに
    /// 現れてしまうため。
    pub fn build(root: &Path, dirs: &Dirs, manifest: &Manifest) -> Result<Self> {
        let mut entries = Vec::new();
        let tops = manifest.tops(dirs)?;

        for target in manifest.targets() {
            let abs = root.join(target);
            // exists() は EACCES も「無い」と答える。読めなくなっただけの
            // ディレクトリを宣言から外れたものとして扱うと、その下のリンクが
            // すべて stale になり $HOME から消える。無いのか読めないのかを
            // 区別する — WalkDir の側と同じ理由。
            match std::fs::metadata(&abs) {
                Ok(_) => {}
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
                Err(err) => {
                    return Err(anyhow::Error::new(err))
                        .with_context(|| format!("failed to read {}", abs.display()))
                }
            }

            if abs.is_file() {
                Self::push(&mut entries, root, dirs, &tops, manifest, Path::new(target))?;
                continue;
            }

            // 読めなかったディレクトリを黙って飛ばさない。飛ばすとその下の
            // パスが「今回の対象」から丸ごと落ち、apply は前回の記録と
            // 突き合わせて $HOME 側のリンクを prune しにかかる。権限を
            // 1 つ間違えただけで利用者の設定が外れる。
            //
            // ignore のディレクトリには入らない。入ると、無関係な木(作業ツリーや
            // node_modules)を全部歩くうえ、その中の読めないディレクトリで落ちる。
            let walker = WalkDir::new(&abs).into_iter().filter_entry(|e| {
                e.path()
                    .strip_prefix(root)
                    .map(|rel| !manifest.is_ignored(rel))
                    .unwrap_or(true)
            });
            for e in walker {
                let e = e.with_context(|| format!("failed to read {}", abs.display()))?;
                if !e.file_type().is_file() {
                    continue;
                }
                let rel = e
                    .path()
                    .strip_prefix(root)
                    .context("path escaped repository root")?;
                Self::push(&mut entries, root, dirs, &tops, manifest, rel)?;
            }
        }

        // 生成物はまだディスクに無いことがある。クローン直後の diff / list が
        // 「これから作られて張られるもの」を数えないと、apply の結果と食い違う。
        // 宣言されている出力は、実体の有無に関わらず対象として数える。
        let renders: Vec<&str> = manifest
            .renders_here()
            .into_iter()
            .map(|(o, _)| o)
            .collect();
        for out in renders
            .into_iter()
            .chain(manifest.encrypted.keys().map(String::as_str))
        {
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
            Self::push(&mut entries, root, dirs, &tops, manifest, rel)?;
        }

        entries.sort_by(|a, b| a.rel.cmp(&b.rel));
        Ok(Plan { entries, tops })
    }

    fn push(
        entries: &mut Vec<Entry>,
        root: &Path,
        dirs: &Dirs,
        tops: &[(PathBuf, PathBuf)],
        manifest: &Manifest,
        rel: &Path,
    ) -> Result<()> {
        // [link] の対象が重なっていると同じパスが二度入り、二度目の
        // symlink が EEXIST で落ちる。diff も --dry-run も何も言わないまま、
        // 最初の apply だけが途中で止まる。
        if entries.iter().any(|e| e.rel == rel) {
            return Ok(());
        }
        // テンプレートは生成の入力であって配置対象ではない。ignore の宣言に
        // 頼ると書き忘れたときに、未展開の {{ }} を含むファイルがそのまま
        // $HOME に置かれる。[render] の入力は自明に対象外なので自動で外す。
        //
        // [merge] の入力も同じ。共有設定そのものを $HOME に出す理由が無い。
        if manifest.is_template(rel)
            || manifest.is_foreign_output(rel)
            || manifest.is_ciphertext(rel)
            || manifest.is_merge_source(rel)
            || manifest.is_ignored(rel)
        {
            return Ok(());
        }
        // [merge] の出力は symlink にしない。リポジトリに同じパスのファイルが
        // あると、link が張った symlink を merge が JSON として書き直す。
        if manifest.is_merge_output(rel) {
            anyhow::bail!(
                "`{}` is a [merge] output and also a file under [link]; \
                 remove it from the repository or from [link]",
                rel.display()
            );
        }
        let place = manifest.placement(dirs, rel)?;
        // 読み込みの検査で断っているが、ここでも確かめる。配置先がフォルダそのものだと
        // apply はそのフォルダを退避するか消してから張る
        if !place
            .under
            .components()
            .any(|c| matches!(c, std::path::Component::Normal(_)))
        {
            anyhow::bail!(
                "`{}` would be placed at {} itself",
                rel.display(),
                place.base.display()
            );
        }
        let src = crate::platform::native(&root.join(rel));
        let dest = crate::platform::native(&place.base.join(&place.under));
        // 2 つの項目が同じ場所へ置こうとすると、後の方が EEXIST で落ちるか、
        // 先に張ったものを「別の場所を指す」として張り替える
        if let Some(other) = entries
            .iter()
            .find(|e| crate::platform::same_path(&e.dest, &dest))
        {
            anyhow::bail!(
                "`{}` and `{}` are both placed at {}",
                other.rel.display(),
                rel.display(),
                dest.display()
            );
        }
        let state = match ancestor_link(root, &place.base, &place.under, tops) {
            Some(link) => State::Ancestor { link },
            None => inspect(&src, &dest),
        };
        // 置くのはファイル。そこに実ディレクトリがあるなら、退避も削除もしない。
        // ディレクトリごと退避すると、中のほかの設定(~/.config 全体など)まで
        // 巻き込む。`to` の書き損じ(`{ path = "starship.toml", to = ".config" }`)で起きる
        if state == State::Occupied
            && std::fs::symlink_metadata(&dest).is_ok_and(|m| m.file_type().is_dir())
        {
            anyhow::bail!(
                "`{}` is a file, but {} is a directory; sennit does not move a directory \
                 aside to put a file there. Check the declaration, or move the directory yourself",
                rel.display(),
                dest.display()
            );
        }
        entries.push(Entry {
            rel: rel.to_path_buf(),
            src,
            dest,
            base: place.base,
            under: place.under,
            state,
        });
        Ok(())
    }

    pub fn changes(&self) -> impl Iterator<Item = &Entry> {
        self.entries.iter().filter(|e| e.state.needs_change())
    }

    /// その項目の祖先で、張り替える symlink。apply が 1 つずつ外していく。
    pub fn ancestor_link(&self, root: &Path, e: &Entry) -> Option<PathBuf> {
        ancestor_link(root, &e.base, &e.under, &self.tops)
    }

    /// `to` で $HOME の同じパス以外へ置く項目の、リポジトリ内のパス -> 配置先。
    /// state に残し、宣言から外れたときに prune する場所を知るのに使う。
    pub fn placed_elsewhere(&self, home: &Path) -> std::collections::BTreeMap<PathBuf, PathBuf> {
        self.entries
            .iter()
            .filter(|e| !crate::platform::same_path(&e.dest, &home.join(&e.rel)))
            .map(|e| (e.rel.clone(), e.dest.clone()))
            .collect()
    }

    /// 今回の項目の (リポジトリ内のパス, 配置先)。
    pub fn dests(&self) -> Vec<(PathBuf, PathBuf)> {
        self.entries
            .iter()
            .map(|e| (e.rel.clone(), e.dest.clone()))
            .collect()
    }
}

/// `rel` の祖先(home の直下から dest の親まで)にある symlink のうち、
/// いちばん外側のもの。向き先は問わない。
fn first_symlink(home: &Path, rel: &Path) -> Option<PathBuf> {
    let mut cur = home.to_path_buf();
    let comps: Vec<_> = rel.components().collect();
    for c in comps.iter().take(comps.len().saturating_sub(1)) {
        cur.push(c);
        let Ok(meta) = std::fs::symlink_metadata(&cur) else {
            // ここから先は無い。無いものは symlink ではありえない
            return None;
        };
        if meta.file_type().is_symlink() {
            return Some(cur);
        }
    }
    None
}

/// 張り替える祖先 symlink。`rel` の祖先のうちいちばん外側の symlink で、
///
/// - 向き先がリポジトリの中なら、どこにあっても張り替える。そのまま進むと
///   リポジトリ自身のファイルを退避し、自分を指すリンクを作る。
/// - 向き先が外なら、manifest に書かれたトップレベルのパスより**深い**ものだけ
///   張り替える。旧い配置が別のリポジトリへ向けていたディレクトリが典型で、
///   symlink は中身を持たないので、外せば向き先には触れずに済む。
///   トップレベルそのもの(`.config -> ~/Dropbox/config`)や、その上のリンクは
///   利用者が意図した配置なので触らず、0.19 までと同じく向き先の中に置く。
///
/// `base` と `rel` は配置先の根とそこからの相対パス(`to` の無い項目なら $HOME と
/// リポジトリ内のパス)。`tops` は各項目の同じ組で、同じ根の項目どうしで比べる。
pub fn ancestor_link(
    root: &Path,
    base: &Path,
    rel: &Path,
    tops: &[(PathBuf, PathBuf)],
) -> Option<PathBuf> {
    let link = first_symlink(base, rel)?;
    if link_into_repo(root, &link) {
        return Some(link);
    }
    let at = link.strip_prefix(base).ok()?;
    tops.iter()
        .filter(|(b, _)| crate::platform::same_path(b, base))
        .any(|(_, t)| at.starts_with(t) && at != t.as_path())
        .then_some(link)
}

/// manifest のトップレベルのパスのうち、それ自身が外を指す symlink のもの。
/// 配置はその向き先の中に行われる。
/// 1 つ目は表示用の名前(`to` の無い項目は `~/` からの相対、ある項目は配置先そのもの)。
pub fn outside_top_levels(root: &Path, dirs: &Dirs, manifest: &Manifest) -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    let Ok(tops) = manifest.tops(dirs) else {
        return out;
    };
    for (base, top) in tops {
        let path = base.join(&top);
        let is_link = std::fs::symlink_metadata(&path)
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false);
        if is_link && !link_into_repo(root, &path) {
            if let Ok(to) = std::fs::read_link(&path) {
                let shown = if crate::platform::same_path(&base, &dirs.home) {
                    format!("~/{}", top.display())
                } else {
                    path.display().to_string()
                };
                out.push((shown, to));
            }
        }
    }
    out
}

/// 上の一覧を 1 行ずつ知らせる。
pub fn note_outside_top_levels(root: &Path, dirs: &Dirs, manifest: &Manifest) {
    for (shown, to) in outside_top_levels(root, dirs, manifest) {
        println!(
            "note: {shown} points to {}, outside the repository; files are placed inside it",
            to.display()
        );
    }
}

/// その symlink の向き先がリポジトリの中か。
///
/// 向き先を解決できればその実体で、できなければ(リンク先が消えている)
/// 字句で比べる。`repo/../other` は文字としては `repo` で始まるが外なので、
/// 字句の比較の前に `..` を畳む。
pub fn link_into_repo(root: &Path, link: &Path) -> bool {
    let Ok(target) = std::fs::read_link(link) else {
        return false;
    };
    let target = if target.is_absolute() {
        target
    } else {
        match link.parent() {
            Some(p) => p.join(target),
            None => return false,
        }
    };
    match crate::platform::canonicalize(&target) {
        Ok(real) => crate::platform::path_starts_with(&real, root),
        Err(_) => crate::platform::path_starts_with(&collapse(&target), root),
    }
}

/// `.` と `..` を字句で畳む。symlink は解決しない。
fn collapse(p: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            other => out.push(other),
        }
    }
    out
}

/// 祖先の symlink のうち、リポジトリの中を指すものだけ。
///
/// [merge] の出力が使う。出力は plan の対象ではなく、利用者が自分で外へ
/// 向けたリンクの先に書くのは従来どおりでよい。中を指すリンクを通すと
/// リポジトリを書き換えてしまうので、それだけを見つける。
pub fn repo_ancestor_link(root: &Path, home: &Path, rel: &Path) -> Option<PathBuf> {
    first_symlink(home, rel).filter(|l| link_into_repo(root, l))
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
                Ok(current) if crate::platform::same_path(&current, src) => State::Linked,
                Ok(current) => State::Wrong { current },
                Err(_) => State::Occupied,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::symlink;

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
        symlink(&src, &linked).unwrap();
        assert_eq!(inspect(&src, &linked), State::Linked);

        let elsewhere = d.join("elsewhere.conf");
        write(&elsewhere, "y");
        let wrong = d.join("wrong.conf");
        symlink(&elsewhere, &wrong).unwrap();
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
        symlink(&src, &dest).unwrap();
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
        let plan = Plan::build(&root, &Dirs::new(home.clone(), false), &m).unwrap();
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
        let plan = Plan::build(&root, &Dirs::new(home.clone(), false), &m).unwrap();
        assert!(plan.entries.is_empty());
    }

    /// ignore のディレクトリには入らない。その中が読めなくても落ちない。
    #[cfg(unix)]
    #[test]
    fn an_ignored_directory_is_not_walked() {
        use std::os::unix::fs::PermissionsExt;
        let d = scratch("pruned-walk");
        let root = d.join("repo");
        let home = d.join("home");
        std::fs::create_dir_all(&home).unwrap();
        write(&root.join("conf/a.conf"), "a\n");
        write(&root.join("conf/worktrees/x/secret.txt"), "x\n");
        write(&root.join("conf/hooks/node_modules/dep/index.js"), "x\n");
        write(&root.join("conf/hooks/a.test.ts"), "x\n");
        let locked = root.join("conf/worktrees");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();

        let m = manifest_from(
            r#"
[link]
common = ["conf"]
ignore = ["conf/worktrees", "**/node_modules", "*.test.ts"]
"#,
        );
        let plan = Plan::build(&root, &Dirs::new(home.clone(), false), &m);
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        // root can read anything, which is fine: the point is that nothing fails
        let plan = plan.unwrap();
        let rels: Vec<_> = plan.entries.iter().map(|e| e.rel.clone()).collect();
        assert_eq!(rels, vec![PathBuf::from("conf/a.conf")]);
    }

    #[test]
    fn a_link_into_the_repository_above_the_destination_is_found() {
        let d = scratch("ancestor");
        let root = d.join("repo");
        let home = d.join("home");
        write(&root.join("skills/gog/SKILL.md"), "x\n");
        std::fs::create_dir_all(home.join("skills")).unwrap();
        symlink(root.join("skills/gog"), home.join("skills/gog")).unwrap();

        let rel = Path::new("skills/gog/SKILL.md");
        let m = manifest_from("[link]\ncommon = [\"skills\"]\n");
        assert_eq!(
            ancestor_link(
                &root,
                &home,
                rel,
                &m.tops(&Dirs::new(home.clone(), false)).unwrap()
            ),
            Some(home.join("skills/gog"))
        );
        // a path with no link above it
        assert_eq!(
            ancestor_link(
                &root,
                &home,
                Path::new("skills/other/a.md"),
                &m.tops(&Dirs::new(home.clone(), false)).unwrap()
            ),
            None
        );
    }

    #[test]
    fn a_relative_link_into_the_repository_is_found_too() {
        let d = scratch("ancestor-relative");
        let root = d.join("repo");
        let home = d.join("repo/../home");
        write(&root.join("skills/gog/SKILL.md"), "x\n");
        std::fs::create_dir_all(home.join("skills")).unwrap();
        symlink("../../repo/skills/gog", home.join("skills/gog")).unwrap();
        let root = crate::platform::canonicalize(&root).unwrap();
        assert!(repo_ancestor_link(&root, &home, Path::new("skills/gog/SKILL.md")).is_some());
    }

    /// A link is found wherever it leads; only the ones into the repository
    /// count for `repo_ancestor_link`.
    #[test]
    fn a_link_outside_the_repository_is_found_but_is_not_a_repository_link() {
        let d = scratch("ancestor-outside");
        let root = d.join("repo");
        let home = d.join("home");
        let elsewhere = d.join("life/skills/gog");
        write(&root.join("skills/gog/SKILL.md"), "x\n");
        write(&elsewhere.join("SKILL.md"), "old\n");
        std::fs::create_dir_all(home.join("skills")).unwrap();
        symlink(&elsewhere, home.join("skills/gog")).unwrap();

        let rel = Path::new("skills/gog/SKILL.md");
        assert!(!link_into_repo(&root, &home.join("skills/gog")));
        assert_eq!(repo_ancestor_link(&root, &home, rel), None);
        // below a declared top-level path, it is replaced ...
        let m = manifest_from("[link]\ncommon = [\"skills\"]\n");
        assert_eq!(
            ancestor_link(
                &root,
                &home,
                rel,
                &m.tops(&Dirs::new(home.clone(), false)).unwrap()
            ),
            Some(home.join("skills/gog"))
        );
        // ... but the top-level path itself, or a link above it, is not
        let m = manifest_from("[link]\ncommon = [\"skills/gog\"]\n");
        assert_eq!(
            ancestor_link(
                &root,
                &home,
                rel,
                &m.tops(&Dirs::new(home.clone(), false)).unwrap()
            ),
            None
        );
        let m = manifest_from("[link]\ncommon = [\"skills/gog/SKILL.md\"]\n");
        assert_eq!(
            ancestor_link(
                &root,
                &home,
                rel,
                &m.tops(&Dirs::new(home.clone(), false)).unwrap()
            ),
            None
        );
    }

    /// `repo/../other` starts with `repo` as text but is not inside it.
    #[test]
    fn a_target_that_climbs_out_of_the_repository_is_outside() {
        let d = scratch("dotdot");
        let root = crate::platform::canonicalize(&d).unwrap().join("repo");
        let home = d.join("home");
        write(&root.join("a"), "x\n");
        write(&d.join("other/p"), "x\n");
        std::fs::create_dir_all(&home).unwrap();

        // the target exists, so it can be resolved
        let to_existing = root.join("../other/p");
        symlink(&to_existing, home.join("existing")).unwrap();
        assert!(!link_into_repo(&root, &home.join("existing")));
        // the target is gone, so only the text is left; `..` still has to count
        let to_missing = root.join("../nowhere/p");
        symlink(&to_missing, home.join("dangling")).unwrap();
        assert!(!link_into_repo(&root, &home.join("dangling")));
        // a `..` that stays inside is inside
        let inside = root.join("sub/../a");
        symlink(&inside, home.join("inside")).unwrap();
        assert!(link_into_repo(&root, &home.join("inside")));
        let gone = root.join("sub/../gone");
        symlink(&gone, home.join("inside-gone")).unwrap();
        assert!(link_into_repo(&root, &home.join("inside-gone")));
    }
}
