//! Everything that differs between Unix and Windows.
//!
//! The rest of sennit calls these instead of reaching for `std::os::unix` or
//! `std::os::windows`, so a platform difference is decided here once. On Unix
//! each function is a thin wrapper over what the standard library does, and
//! behaves as it always has.

use anyhow::{bail, Result};
use std::fs::{Metadata, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Whether POSIX permission bits (`[modes]`) can be applied on this platform.
pub const MODES_ENFORCED: bool = cfg!(unix);

/// The home directory: `$HOME` on Unix, `%USERPROFILE%` on Windows (with
/// `$HOME` as the fallback, for shells that set only that).
pub fn home_dir() -> Result<PathBuf> {
    let vars: &[&str] = if cfg!(windows) {
        &["USERPROFILE", "HOME"]
    } else {
        &["HOME"]
    };
    for v in vars {
        if let Some(h) = std::env::var_os(v).filter(|h| !h.is_empty()) {
            return Ok(PathBuf::from(h));
        }
    }
    if cfg!(windows) {
        bail!("USERPROFILE is not set")
    } else {
        bail!("HOME is not set")
    }
}

/// Like `std::fs::canonicalize`, but never returns a `\\?\` path on Windows.
/// Those are valid but unreadable in messages, and they do not compare equal to
/// the same path spelled normally.
pub fn canonicalize(p: &Path) -> std::io::Result<PathBuf> {
    std::fs::canonicalize(p).map(simplify)
}

/// Drop the `\\?\` prefix that `canonicalize` adds on Windows, when the path
/// is the same without it. A no-op elsewhere.
pub fn simplify(p: PathBuf) -> PathBuf {
    #[cfg(windows)]
    {
        if let Some(s) = p.to_str() {
            if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
                return PathBuf::from(format!(r"\\{rest}"));
            }
            if let Some(rest) = s.strip_prefix(r"\\?\") {
                let b = rest.as_bytes();
                let drive = b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':';
                if drive && (b.len() == 2 || b[2] == b'\\') {
                    return PathBuf::from(rest);
                }
            }
        }
    }
    p
}

/// A path spelled the way Windows stores one: backslashes only, no `\\?\`.
/// The identity on Unix.
pub fn native(p: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        let p = simplify(p.to_path_buf());
        // Rebuilding from components also folds `a\.\b` and `a//b`
        let mut out = PathBuf::new();
        for c in p.components() {
            out.push(c);
        }
        out
    }
    #[cfg(not(windows))]
    {
        p.to_path_buf()
    }
}

/// Whether two paths name the same place, compared as text: Windows ignores
/// case, separators and the `\\?\` prefix; Unix compares exactly.
pub fn same_path(a: &Path, b: &Path) -> bool {
    #[cfg(windows)]
    {
        fold(a) == fold(b)
    }
    #[cfg(not(windows))]
    {
        a == b
    }
}

/// `Path::starts_with`, with Windows' idea of equal names.
pub fn path_starts_with(path: &Path, base: &Path) -> bool {
    #[cfg(windows)]
    {
        let (p, b) = (fold(path), fold(base));
        let (mut pc, mut bc) = (p.components(), b.components());
        loop {
            match (pc.next(), bc.next()) {
                (_, None) => return true,
                (Some(x), Some(y)) if x == y => {}
                _ => return false,
            }
        }
    }
    #[cfg(not(windows))]
    {
        path.starts_with(base)
    }
}

#[cfg(windows)]
fn fold(p: &Path) -> PathBuf {
    let n = native(p);
    PathBuf::from(n.to_string_lossy().to_lowercase())
}

/// The permission bits of `meta`. Windows has none: the answer is what the
/// read-only attribute stands for (0o444, or 0o644 when writable).
pub fn mode_of(meta: &Metadata) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o777
    }
    #[cfg(not(unix))]
    {
        if meta.permissions().readonly() {
            0o444
        } else {
            0o644
        }
    }
}

/// Whether `meta` already has what `want` asks for. On Windows only the owner
/// write bit counts, since the read-only attribute is all there is.
pub fn mode_matches(meta: &Metadata, want: u32) -> bool {
    #[cfg(unix)]
    {
        mode_of(meta) == want
    }
    #[cfg(not(unix))]
    {
        meta.permissions().readonly() == (want & 0o200 == 0)
    }
}

/// Set the permission bits. On Windows this sets or clears the read-only
/// attribute from the owner write bit, and nothing else.
pub fn set_mode(path: &Path, mode: u32) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
    }
    #[cfg(not(unix))]
    {
        let mut perm = std::fs::metadata(path)?.permissions();
        perm.set_readonly(mode & 0o200 == 0);
        std::fs::set_permissions(path, perm)
    }
}

/// Have a file created with `mode`, so there is no moment when it exists with
/// wider permissions. Windows has no creation mode; `set_mode` follows the write.
pub fn create_with_mode(opts: &mut OpenOptions, mode: u32) -> &mut OpenOptions {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(mode)
    }
    #[cfg(not(unix))]
    {
        let _ = mode;
        opts
    }
}

/// Tell the person, once, that `[modes]` does nothing here.
pub fn note_modes_ignored(count: usize) {
    if MODES_ENFORCED || count == 0 {
        return;
    }
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        eprintln!(
            "\x1b[33mwarning\x1b[0m: [modes] is not applied on Windows ({count} entr{} ignored); \
             Windows has no POSIX permission bits",
            if count == 1 { "y" } else { "ies" }
        );
    });
}

/// Create a symlink at `dest` pointing to `src`.
///
/// Windows needs to know whether the target is a file or a directory when the
/// link is made, so it is looked at here. Creating a link needs Developer Mode
/// or an elevated shell; without either, this fails and says so. It never
/// falls back to copying: a copy would look placed and then drift from the
/// repository without anyone noticing.
pub fn symlink(src: impl AsRef<Path>, dest: impl AsRef<Path>) -> Result<()> {
    let (src, dest) = (src.as_ref(), dest.as_ref());
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(src, dest)?;
        Ok(())
    }
    #[cfg(windows)]
    {
        let target = native(src);
        // A relative target is relative to the link's directory
        let seen = match dest.parent() {
            Some(dir) if src.is_relative() => dir.join(src),
            _ => src.to_path_buf(),
        };
        let is_dir = std::fs::metadata(seen).is_ok_and(|m| m.is_dir());
        let made = if is_dir {
            std::os::windows::fs::symlink_dir(&target, dest)
        } else {
            std::os::windows::fs::symlink_file(&target, dest)
        };
        made.map_err(explain_symlink_error)
    }
}

/// Turn the error from creating a link into one a person can act on.
#[cfg(windows)]
fn explain_symlink_error(e: std::io::Error) -> anyhow::Error {
    // ERROR_PRIVILEGE_NOT_HELD
    if e.raw_os_error() == Some(1314) {
        return anyhow::anyhow!(
            "Windows refused to create a symbolic link (error 1314, a required privilege is \
             not held). Turn on Developer Mode (Settings > System > For developers), or run \
             sennit from an elevated terminal, then run it again. sennit does not copy files \
             in place of links."
        );
    }
    e.into()
}

/// Remove a symlink (not what it points at). A directory link on Windows is
/// removed as a directory.
pub fn remove_link(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        std::fs::remove_file(path)
    }
    #[cfg(not(unix))]
    {
        use std::os::windows::fs::FileTypeExt;
        let is_dir_link =
            std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink_dir());
        if is_dir_link {
            std::fs::remove_dir(path)
        } else {
            std::fs::remove_file(path)
        }
    }
}

/// Remove a file, even one marked read-only on Windows.
pub fn remove_file_force(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        #[cfg(windows)]
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            clear_readonly(path);
            std::fs::remove_file(path)
        }
        Err(e) => Err(e),
    }
}

// The lint is about Unix, where this would make a file world-writable. On
// Windows clearing the attribute is all there is to it.
#[allow(clippy::permissions_set_readonly_false)]
#[cfg(windows)]
fn clear_readonly(path: &Path) {
    if let Ok(meta) = std::fs::metadata(path) {
        let mut perm = meta.permissions();
        if perm.readonly() {
            perm.set_readonly(false);
            let _ = std::fs::set_permissions(path, perm);
        }
    }
}

/// Move `from` over `to`, replacing it if it exists. Windows refuses to
/// replace a read-only file; that is cleared and the move retried once.
pub fn replace_file(from: &Path, to: &Path) -> std::io::Result<()> {
    match std::fs::rename(from, to) {
        Ok(()) => Ok(()),
        #[cfg(windows)]
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            clear_readonly(to);
            std::fs::rename(from, to)
        }
        Err(e) => Err(e),
    }
}

/// A command that runs `script` with a POSIX shell: `sh -c` on Unix.
///
/// Windows has none of its own. Hooks are written for a POSIX shell, so Git
/// for Windows' bash is used. `bash` on `PATH` is not trusted: on a machine
/// with WSL it is the launcher in `System32`, which runs the script inside a
/// Linux VM where the repository's Windows paths mean nothing.
pub fn shell(script: &str) -> Result<Command> {
    #[cfg(unix)]
    {
        let mut c = Command::new("sh");
        c.arg("-c").arg(script);
        Ok(c)
    }
    #[cfg(windows)]
    {
        let sh = windows_shell()?;
        let mut c = Command::new(sh);
        c.arg("-c").arg(script);
        Ok(c)
    }
}

#[cfg(windows)]
fn windows_shell() -> Result<PathBuf> {
    // An explicit choice wins, and is not second-guessed
    if let Some(p) = std::env::var_os("SENNIT_SHELL").filter(|p| !p.is_empty()) {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Ok(p);
        }
        bail!(
            "SENNIT_SHELL is set to {}, which is not a file",
            p.display()
        );
    }
    let mut tried: Vec<PathBuf> = Vec::new();
    let mut candidates: Vec<PathBuf> = Vec::new();

    // Git for Windows: bash sits at <root>\bin\bash.exe, and git.exe is in
    // <root>\cmd, <root>\bin or <root>\mingw64\bin
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            let git = dir.join("git.exe");
            if !git.is_file() {
                continue;
            }
            for root in git.ancestors().skip(2).take(3) {
                candidates.push(root.join("bin").join("bash.exe"));
            }
        }
    }
    for var in ["ProgramFiles", "ProgramW6432", "ProgramFiles(x86)"] {
        if let Some(base) = std::env::var_os(var) {
            candidates.push(PathBuf::from(base).join("Git").join("bin").join("bash.exe"));
        }
    }
    if let Some(base) = std::env::var_os("LOCALAPPDATA") {
        candidates.push(
            PathBuf::from(base)
                .join("Programs")
                .join("Git")
                .join("bin")
                .join("bash.exe"),
        );
    }
    // Any other bash or sh on PATH (MSYS2, Cygwin), except the WSL launchers
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            for name in ["bash.exe", "sh.exe"] {
                candidates.push(dir.join(name));
            }
        }
    }

    for c in candidates {
        if tried.contains(&c) {
            continue;
        }
        tried.push(c.clone());
        if c.is_file() && !is_wsl_launcher(&c) {
            return Ok(c);
        }
    }
    bail!(
        "no POSIX shell was found to run hooks. Install Git for Windows (its bash is used), \
         or set SENNIT_SHELL to the path of a bash.exe or sh.exe. The bash in System32 and \
         WindowsApps is the WSL launcher and is not used."
    )
}

#[cfg(windows)]
fn is_wsl_launcher(p: &Path) -> bool {
    let s = p.to_string_lossy().to_lowercase().replace('/', "\\");
    s.contains("\\windows\\system32\\") || s.contains("\\windowsapps\\")
}

/// Whether `dir\name` is something `PATH` lookup would run. Windows adds the
/// extensions in `PATHEXT`; Unix checks the execute bit.
pub fn find_in_dir(dir: &Path, name: &str) -> Option<PathBuf> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let p = dir.join(name);
        let ok =
            p.is_file() && std::fs::metadata(&p).is_ok_and(|m| m.permissions().mode() & 0o111 != 0);
        ok.then_some(p)
    }
    #[cfg(windows)]
    {
        let exact = dir.join(name);
        if exact.is_file() && Path::new(name).extension().is_some() {
            return Some(exact);
        }
        let exts = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into());
        exts.split(';')
            .filter(|e| !e.is_empty())
            .map(|e| dir.join(format!("{name}{e}")))
            .find(|p| p.is_file())
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    /// Without Developer Mode Windows answers 1314. The message has to say what
    /// to do, and must not suggest that a copy was made instead.
    #[test]
    fn a_missing_symlink_privilege_says_how_to_fix_it() {
        let e = explain_symlink_error(std::io::Error::from_raw_os_error(1314));
        let text = format!("{e:#}");
        assert!(text.contains("Developer Mode"), "{text}");
        assert!(text.contains("does not copy"), "{text}");
    }

    #[test]
    fn other_errors_pass_through() {
        let e = explain_symlink_error(std::io::Error::from_raw_os_error(5));
        assert!(!format!("{e:#}").contains("Developer Mode"));
    }

    #[test]
    fn verbatim_prefixes_are_dropped_only_when_harmless() {
        assert_eq!(
            simplify(PathBuf::from(r"\\?\C:\a\b")),
            PathBuf::from(r"C:\a\b")
        );
        assert_eq!(
            simplify(PathBuf::from(r"\\?\UNC\srv\share\a")),
            PathBuf::from(r"\\srv\share\a")
        );
        // a device path is not a drive path; leave it
        assert_eq!(
            simplify(PathBuf::from(r"\\?\Volume{1234}\a")),
            PathBuf::from(r"\\?\Volume{1234}\a")
        );
    }

    #[test]
    fn paths_compare_without_regard_to_case_or_separators() {
        assert!(same_path(
            Path::new(r"C:\Repo\a.conf"),
            Path::new("c:/repo/a.conf")
        ));
        assert!(same_path(
            Path::new(r"\\?\C:\Repo\a"),
            Path::new(r"C:\repo\a")
        ));
        assert!(!same_path(Path::new(r"C:\Repo\a"), Path::new(r"C:\Repo\b")));
        assert!(path_starts_with(
            Path::new(r"C:\Repo\sub\a"),
            Path::new("c:/repo")
        ));
        // a name that merely begins with the same letters is not inside
        assert!(!path_starts_with(
            Path::new(r"C:\Repo2\a"),
            Path::new(r"C:\Repo")
        ));
    }
}
