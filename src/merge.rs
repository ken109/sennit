//! `[merge]`: overlay a shared JSON file onto a file that cannot be a symlink.
//!
//! Some tools keep machine-specific settings in the same file you want to share
//! (Claude Code's `settings.json` has hooks that only make sense on one machine),
//! and rewrite it themselves. A symlink into the repository would either leak
//! those settings into git or be overwritten by the tool. So the shared file is
//! merged into the target instead:
//!
//! - objects are merged recursively;
//! - arrays and scalars are replaced by the shared side;
//! - keys that exist only in the target are left alone.
//!
//! Nothing is ever printed except key paths. The values may be tokens.

use crate::manifest::Manifest;
use crate::plan;
use crate::state::Backup;
use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::path::{Path, PathBuf};

/// One declared merge, with its source already read and checked.
#[derive(Debug)]
pub struct Planned {
    /// As written in the manifest; relative to `$HOME`.
    pub out: String,
    pub rel: PathBuf,
    pub dest: PathBuf,
    pub shared: Value,
}

/// How the target compares with what a merge would produce.
#[derive(Debug, PartialEq)]
pub enum Status {
    Ok,
    Missing,
    /// Key paths in the shared file whose value in the target differs.
    Stale(Vec<String>),
    /// The target cannot be merged into (invalid JSON, a symlink, ...).
    Broken(String),
}

/// Read and check every source. Fails before anything is changed.
pub fn plan(root: &Path, home: &Path, manifest: &Manifest) -> Result<Vec<Planned>> {
    let mut out = Vec::new();
    for (name, m) in &manifest.merge {
        let src = root.join(&m.from);
        match std::fs::metadata(&src) {
            Ok(_) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                bail!("merge source `{}` for `{}` does not exist", m.from, name)
            }
            Err(err) => {
                return Err(anyhow::Error::new(err))
                    .with_context(|| format!("failed to read {}", src.display()))
            }
        }
        crate::must_be_inside(root, &src)?;
        let text = std::fs::read_to_string(&src)
            .with_context(|| format!("failed to read {}", src.display()))?;
        let shared: Value = serde_json::from_str(&text).with_context(|| {
            format!(
                "merge source `{}` is not valid JSON (comments and trailing commas are not supported)",
                m.from
            )
        })?;
        if !shared.is_object() {
            bail!("merge source `{}` must be a JSON object", m.from);
        }
        out.push(Planned {
            out: name.clone(),
            rel: PathBuf::from(name),
            dest: home.join(name),
            shared,
        });
    }
    Ok(out)
}

/// `over` on top of `base`. Objects recurse; everything else is replaced.
/// The key order of `base` is kept, and new keys follow in the order of `over`.
pub fn merge_value(base: Option<&Value>, over: &Value) -> Value {
    match (base, over) {
        (Some(Value::Object(b)), Value::Object(o)) => {
            let mut out = b.clone();
            for (k, v) in o {
                let merged = merge_value(b.get(k), v);
                out.insert(k.clone(), merged);
            }
            Value::Object(out)
        }
        _ => over.clone(),
    }
}

/// Two-space indent and a trailing newline.
pub fn render(v: &Value) -> String {
    let mut s = serde_json::to_string_pretty(v).expect("a JSON value always serializes");
    s.push('\n');
    s
}

/// Dotted key paths of `shared` whose value in `current` is different or absent.
pub fn changed_paths(shared: &Value, current: Option<&Value>) -> Vec<String> {
    let mut out = Vec::new();
    collect(shared, current, &mut Vec::new(), &mut out);
    out
}

fn collect(shared: &Value, current: Option<&Value>, path: &mut Vec<String>, out: &mut Vec<String>) {
    match shared {
        Value::Object(o) if !o.is_empty() => {
            let cur = current.and_then(Value::as_object);
            for (k, v) in o {
                path.push(k.clone());
                collect(v, cur.and_then(|c| c.get(k)), path, out);
                path.pop();
            }
        }
        _ => {
            // An empty object merges to "whatever is there", unless that is not an object
            let same = match (shared, current) {
                (Value::Object(_), Some(Value::Object(_))) => true,
                (a, Some(b)) => a == b,
                (_, None) => false,
            };
            if !same && !path.is_empty() {
                out.push(path.join("."));
            }
        }
    }
}

pub struct Target {
    pub text: String,
    pub value: Value,
    pub mode: u32,
}

/// The current target. `None` when it does not exist.
///
/// A target that cannot be merged into is an error rather than something to
/// overwrite: writing over a file that failed to parse destroys whatever the
/// person had in it.
pub fn read_target(dest: &Path) -> Result<Option<Target>> {
    use std::os::unix::fs::PermissionsExt;
    let meta = match std::fs::symlink_metadata(dest) {
        Ok(m) => m,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => {
            return Err(anyhow::Error::new(err))
                .with_context(|| format!("failed to read {}", dest.display()))
        }
    };
    if meta.file_type().is_symlink() {
        bail!(
            "{} is a symlink; a merge target is rewritten in place, not through a link",
            dest.display()
        );
    }
    if !meta.is_file() {
        bail!("{} is not a regular file", dest.display());
    }
    let text = std::fs::read_to_string(dest)
        .with_context(|| format!("failed to read {}", dest.display()))?;
    let value: Value = serde_json::from_str(&text).with_context(|| {
        format!(
            "{} is not valid JSON (comments and trailing commas are not supported); \
             it was left untouched",
            dest.display()
        )
    })?;
    if !value.is_object() {
        bail!(
            "{} is not a JSON object; it was left untouched",
            dest.display()
        );
    }
    Ok(Some(Target {
        text,
        value,
        mode: meta.permissions().mode() & 0o777,
    }))
}

/// What a merge needs to know about the run it is part of.
pub struct Env<'a> {
    pub root: &'a Path,
    pub home: &'a Path,
    pub manifest: &'a Manifest,
    /// Paths (relative to `$HOME`) holding a link that this run is about to
    /// prune: recorded last time, no longer declared, and ours. A merge target
    /// that is one of them is treated as absent, since apply removes the link
    /// before merging and a preview should say what apply will find.
    pub pending: Vec<PathBuf>,
}

impl<'a> Env<'a> {
    pub fn new(
        root: &'a Path,
        home: &'a Path,
        manifest: &'a Manifest,
        plan: &plan::Plan,
        previous: &crate::state::State,
    ) -> Self {
        let current: Vec<PathBuf> = plan.entries.iter().map(|e| e.rel.clone()).collect();
        let pending = previous
            .stale(&current)
            .into_iter()
            .filter(|rel| {
                let dest = home.join(rel);
                std::fs::symlink_metadata(&dest).is_ok_and(|m| m.file_type().is_symlink())
                    && plan::link_into_repo(root, &dest)
            })
            .collect();
        Env {
            root,
            home,
            manifest,
            pending,
        }
    }

    /// Like `new`, reading the plan and the state itself. For commands that
    /// have neither at hand.
    pub fn load(root: &'a Path, home: &'a Path, manifest: &'a Manifest) -> Result<Self> {
        let plan = plan::Plan::build(root, home, manifest)?;
        let previous = crate::state::State::load(home)?;
        Ok(Self::new(root, home, manifest, &plan, &previous))
    }
}

/// The target as this run will find it.
fn target_of(p: &Planned, env: &Env) -> Result<Option<Target>> {
    if env.pending.contains(&p.rel) {
        return Ok(None);
    }
    read_target(&p.dest)
}

pub fn status(p: &Planned, env: &Env) -> Status {
    // Through a link into the repository the target is not a file of its own yet
    if plan::repo_ancestor_link(env.root, env.home, &p.rel).is_some() {
        return Status::Missing;
    }
    match target_of(p, env) {
        Err(e) => Status::Broken(format!("{e:#}")),
        Ok(None) => Status::Missing,
        Ok(Some(t)) => {
            let keys = changed_paths(&p.shared, Some(&t.value));
            if keys.is_empty() {
                Status::Ok
            } else {
                Status::Stale(keys)
            }
        }
    }
}

/// A short, value-free description of a set of key paths.
pub fn describe_keys(keys: &[String]) -> String {
    const SHOWN: usize = 8;
    let mut s = keys
        .iter()
        .take(SHOWN)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    if keys.len() > SHOWN {
        s.push_str(&format!(", and {} more", keys.len() - SHOWN));
    }
    s
}

struct Prepared {
    text: String,
    existed: bool,
    unchanged: bool,
    keys: Vec<String>,
    mode: u32,
}

fn prepare(p: &Planned, env: &Env, dry_run: bool) -> Result<Prepared> {
    let blocked = plan::repo_ancestor_link(env.root, env.home, &p.rel);
    if let Some(link) = &blocked {
        // Apply replaces a link into the repository with a real directory
        // before it gets here. Still finding one means it was not under [link],
        // and writing through it would change the repository.
        if !dry_run {
            bail!(
                "{} is a symlink into the repository, so {} would be written into it; \
                 replace the link with a directory first",
                link.display(),
                p.dest.display()
            );
        }
    }
    let current = if blocked.is_some() {
        None
    } else {
        target_of(p, env)?
    };
    let base = current.as_ref().map(|t| &t.value);
    let merged = merge_value(base, &p.shared);
    let text = render(&merged);
    Ok(Prepared {
        keys: changed_paths(&p.shared, base),
        unchanged: current.as_ref().is_some_and(|t| t.text == text),
        existed: current.is_some(),
        mode: current.as_ref().map_or(0o600, |t| t.mode),
        text,
    })
}

/// Write to a temporary file beside the target and rename it over.
/// The mode of an existing target is kept; a new file is private.
fn write_atomic(dest: &Path, text: &str, mode: u32) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let name = dest.file_name().context("merge target has no file name")?;
    let tmp = dest.with_file_name(format!("{}.sennit-tmp", name.to_string_lossy()));
    let _ = std::fs::remove_file(&tmp);
    let result = (|| -> Result<()> {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(&tmp)
            .with_context(|| format!("failed to write {}", tmp.display()))?;
        f.write_all(text.as_bytes())?;
        f.flush()?;
        drop(f);
        // The umask can have narrowed it
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode))?;
        std::fs::rename(&tmp, dest).with_context(|| format!("failed to replace {}", dest.display()))
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// Records the backups and the merged targets so far.
pub type SaveState<'a> = dyn FnMut(&[Backup], &[String]) -> Result<()> + 'a;

/// Merge every declared target. Returns how many were (or would be) changed.
///
/// A target that sennit has not touched before and that already exists is copied
/// to `<name>.sennit-backup` first, once, and the copy is recorded with the other
/// backups so `rollback` puts it back. `merged` is the list of targets sennit has
/// written; the caller saves it.
#[allow(clippy::too_many_arguments)]
pub fn apply_all(
    env: &Env,
    merges: &[Planned],
    merged: &mut Vec<String>,
    kept: &mut Vec<Backup>,
    backup: bool,
    dry_run: bool,
    save: &mut SaveState,
) -> Result<usize> {
    let manifest = env.manifest;
    let mut changed = 0usize;

    // Taking a declaration away does not undo what it did: the keys stay in the file
    for gone in merged.iter().filter(|m| !manifest.merge.contains_key(*m)) {
        println!(
            "  {:>8}  {}  (no longer declared; the file is left as it is)",
            "release", gone
        );
    }
    if !dry_run {
        merged.retain(|m| manifest.merge.contains_key(m));
    }

    for p in merges {
        if dry_run && plan::repo_ancestor_link(env.root, env.home, &p.rel).is_some() {
            println!(
                "  {:>8}  {}  (once the link above it is replaced)",
                "would merge", p.out
            );
            changed += 1;
            continue;
        }
        let prep = prepare(p, env, dry_run)?;
        if prep.unchanged {
            if !dry_run && !merged.contains(&p.out) {
                merged.push(p.out.clone());
            }
            continue;
        }
        changed += 1;

        let first_touch = prep.existed && !merged.contains(&p.out);
        let what = if prep.keys.is_empty() {
            "reformatted".to_string()
        } else {
            format!("{} key(s)", prep.keys.len())
        };
        let verb = if dry_run { "would merge" } else { "merge" };
        println!("  {:>8}  {}  ({})", verb, p.out, what);
        if !prep.keys.is_empty() {
            println!("            {}", describe_keys(&prep.keys));
        }
        if dry_run {
            if first_touch && backup {
                println!("            the existing file would be kept at <name>.sennit-backup");
            }
            continue;
        }

        if first_touch && backup {
            let to = crate::backup_path(&p.dest)?;
            std::fs::copy(&p.dest, &to).with_context(|| {
                format!(
                    "failed to keep a copy of {} at {}",
                    p.dest.display(),
                    to.display()
                )
            })?;
            println!("            kept the old file at {}", to.display());
            kept.push(Backup {
                dest: p.dest.clone(),
                kept_at: to,
            });
        }
        if !merged.contains(&p.out) {
            merged.push(p.out.clone());
        }
        // The copy and the claim go on record before the file is touched
        save(kept, merged)?;
        write_atomic(&p.dest, &prep.text, prep.mode)?;
    }
    Ok(changed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn merged(base: Value, over: Value) -> Value {
        merge_value(Some(&base), &over)
    }

    #[test]
    fn objects_are_merged_recursively() {
        let got = merged(
            json!({"a": {"x": 1, "y": 2}, "keep": true}),
            json!({"a": {"y": 3, "z": 4}}),
        );
        assert_eq!(got, json!({"a": {"x": 1, "y": 3, "z": 4}, "keep": true}));
    }

    #[test]
    fn arrays_and_scalars_are_replaced() {
        let got = merged(
            json!({"list": [1, 2, 3], "n": 1, "s": "old"}),
            json!({"list": [9], "n": "now a string", "s": {"nested": 1}}),
        );
        assert_eq!(
            got,
            json!({"list": [9], "n": "now a string", "s": {"nested": 1}})
        );
    }

    /// A hook event is a key holding an array, so the shared side replaces the
    /// array for an event it names; an event only the target has survives.
    #[test]
    fn a_shared_hook_event_replaces_the_array_and_other_events_survive() {
        let got = merged(
            json!({"hooks": {
                "Stop": [{"hooks": [{"command": "machine-only-stop"}]}],
                "Notification": [{"hooks": [{"command": "machine-only"}]}],
            }}),
            json!({"hooks": {"Stop": [{"hooks": [{"command": "shared-stop"}]}]}}),
        );
        assert_eq!(
            got,
            json!({"hooks": {
                "Stop": [{"hooks": [{"command": "shared-stop"}]}],
                "Notification": [{"hooks": [{"command": "machine-only"}]}],
            }})
        );
    }

    #[test]
    fn key_order_of_the_target_is_kept_and_new_keys_follow() {
        let base: Value = serde_json::from_str(r#"{"z": 1, "a": 2, "m": 3}"#).unwrap();
        let over: Value = serde_json::from_str(r#"{"a": 9, "new2": 1, "new1": 2}"#).unwrap();
        let got = merge_value(Some(&base), &over);
        let keys: Vec<_> = got.as_object().unwrap().keys().cloned().collect();
        assert_eq!(keys, ["z", "a", "m", "new2", "new1"]);
    }

    #[test]
    fn a_missing_target_becomes_the_shared_file() {
        let over = json!({"a": {"b": 1}});
        assert_eq!(merge_value(None, &over), over);
    }

    #[test]
    fn output_is_two_space_indented_with_a_newline() {
        assert_eq!(render(&json!({"a": [1]})), "{\n  \"a\": [\n    1\n  ]\n}\n");
    }

    #[test]
    fn changed_paths_name_leaves_that_differ() {
        let shared = json!({"a": {"x": 1, "y": 2}, "list": [1], "same": true});
        let current = json!({"a": {"x": 1, "y": 3}, "list": [2], "same": true, "extra": 1});
        assert_eq!(changed_paths(&shared, Some(&current)), ["a.y", "list"]);
        assert!(changed_paths(&shared, Some(&merge_value(Some(&current), &shared))).is_empty());
        // with no target, every leaf is new
        assert_eq!(changed_paths(&shared, None), ["a.x", "a.y", "list", "same"]);
    }

    #[test]
    fn changed_paths_never_carry_values() {
        let shared = json!({"env": {"TOKEN": "hunter2"}});
        let keys = changed_paths(&shared, None);
        assert_eq!(keys, ["env.TOKEN"]);
        assert!(!describe_keys(&keys).contains("hunter2"));
    }

    #[test]
    fn a_long_key_list_is_cut_short() {
        let keys: Vec<String> = (0..12).map(|i| format!("k{i}")).collect();
        let s = describe_keys(&keys);
        assert!(s.ends_with("and 4 more"), "{s}");
    }

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("sennit-merge-{name}"));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn a_broken_target_is_an_error_not_something_to_overwrite() {
        let d = scratch("broken");
        let p = d.join("settings.json");
        std::fs::write(&p, "{ // a comment\n}").unwrap();
        let e = read_target(&p).err().unwrap();
        assert!(format!("{e:#}").contains("not valid JSON"), "{e:#}");
        std::fs::write(&p, "[]").unwrap();
        assert!(read_target(&p).is_err());
        assert!(read_target(&d.join("absent.json")).unwrap().is_none());
    }

    #[test]
    fn a_symlinked_target_is_refused() {
        let d = scratch("symlink");
        std::fs::write(d.join("real.json"), "{}").unwrap();
        std::os::unix::fs::symlink(d.join("real.json"), d.join("link.json")).unwrap();
        let e = read_target(&d.join("link.json")).err().unwrap();
        assert!(format!("{e:#}").contains("symlink"), "{e:#}");
    }

    #[test]
    fn an_atomic_write_keeps_the_mode_and_leaves_no_temp_file() {
        use std::os::unix::fs::PermissionsExt;
        let d = scratch("atomic");
        let p = d.join("settings.json");
        write_atomic(&p, "{}\n", 0o600).unwrap();
        assert_eq!(
            std::fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o600
        );
        write_atomic(&p, "{\"a\": 1}\n", 0o640).unwrap();
        assert_eq!(
            std::fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o640
        );
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "{\"a\": 1}\n");
        let names: Vec<_> = std::fs::read_dir(&d)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["settings.json"]);
    }
}
