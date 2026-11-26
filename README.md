# sennit

A dotfiles manager that keeps symlink semantics, and adds templating and drift detection.

> *sennit* — braided cordage, made by plaiting many strands into a single line.

[![crates.io](https://img.shields.io/crates/v/sennit.svg)](https://crates.io/crates/sennit)
[![CI](https://github.com/ken109/sennit/actions/workflows/ci.yml/badge.svg)](https://github.com/ken109/sennit/actions/workflows/ci.yml)
[![license](https://img.shields.io/crates/l/sennit.svg)](LICENSE)

```sh
brew install ken109/tap/sennit   # or: cargo install sennit
```

On Windows, download `sennit-x86_64-pc-windows-msvc.zip` from the
[latest release](https://github.com/ken109/sennit/releases/latest) (`sennit.exe` is at the
root of the zip, with a `.sha256` beside it), or run `cargo install sennit`. See
[Windows](#windows) for what differs there.

- [Why another one?](#why-another-one)
- [Quick start](#quick-start)
- [Commands](#commands)
- [What apply will not do](#what-apply-will-not-do)
- [Templating](#templating)
- [Per-machine variation](#per-machine-variation)
- [Files that cannot be symlinks](#files-that-cannot-be-symlinks)
- [Running things after placing them](#running-things-after-placing-them)
- [File modes](#file-modes)
- [Windows](#windows)
- [Secrets](#secrets)
- [Checking that secrets still work](#checking-that-secrets-still-work)
- [Encrypted files](#encrypted-files)
- [Installing packages](#installing-packages)
- [Three ways of being wrong](#three-ways-of-being-wrong)
- [Drift detection](#drift-detection)

## Why another one?


Most dotfiles managers make you choose between two models, and both cost something:

- **Symlink-based** (GNU Stow, hand-rolled scripts) — `~/.config/nvim/init.lua` *is* the
  file in your repository. Edit it in place and it shows up in `git diff`. But there is no
  templating, no way to preview what an apply will change, and nothing checks that the
  packages your configs depend on are actually installed.
- **Copy-based** (chezmoi) — you get templating and per-machine variants, but the source
  and the target are different files. Editing `~/.config` directly is a mistake that gets
  silently reverted on the next apply.

sennit takes the position that this is a false choice. **Templating and symlinking are only
in tension for the files that actually need templating** — usually a handful that carry a
color palette. So sennit renders those into your repository — not committed, rebuilt on
every apply — and symlinks everything the same way. The live-edit loop survives for every
other config.

## Quick start


In the root of your dotfiles repository:

```toml
# sennit.toml — what to place
[link]
common = [".config", ".zshenv"]
darwin = [".hammerspoon"]
linux  = [".local"]
```

Nothing is inferred from a file's name. `[link]` says what to place, `[render]` says what to
generate, and a path declared as the input to a render or a decryption is never placed —
so forgetting `ignore = ["*.tmpl"]` cannot put an unexpanded template in `$HOME`.

```sh
sennit diff    # see what would change
sennit apply   # place the symlinks
```

`sennit` looks for `sennit.toml` by walking up from the current directory, so it works
from anywhere inside your repository.

Directories are linked file by file rather than as a whole, so that a tool writing into
`~/.config/something/` does not make untracked files appear inside your repository.

`ignore` takes glob patterns, matched against paths relative to the repository root:

| Pattern | Matches |
|---|---|
| `*.tmpl` | any file ending in `.tmpl`, at any depth |
| `*.test.ts`, `*.tar.gz` | likewise; the whole name is matched, not just the last extension |
| `**/node_modules` | a `node_modules` directory anywhere, and everything under it |
| `.claude/worktrees` | that path from the root, and everything under it |
| `README.md`, `conf/nvim` | a plain name matches from the root only, as whole components |

`ignore` can also differ per platform. Written as a table, it takes the same keys as the
lists that say what to place: `common` applies everywhere, and `darwin`, `linux` and
`windows` only there.

```toml
[link.ignore]
common  = ["*.tmpl", "**/node_modules"]
windows = [".config/zsh"]
```

The array form is the same as `common` alone. sennit 0.21 and earlier refuse the table.

`*` and `?` stay within one path component; `**` as a whole component spans any number of
them. A pattern with a slash is rooted at the repository. A pattern with a wildcard and no
slash applies at any depth, as in gitignore. A plain name with neither is a rooted prefix
and nothing more, which is how it always behaved: `conf/nvim` matches `conf/nvim/init.lua`
but not `conf/nvim-extra`, and `README.md` matches the one at the root, not one inside a
subdirectory. An ignored directory is not entered at all, so a large or unreadable tree
inside it (a `node_modules`, a pile of worktrees) costs nothing and cannot fail the run.
Declared `[render]` templates, `[encrypted]` ciphertexts and `[merge]` sources are
excluded whether or not you list them, which is what makes a forgotten
`ignore = ["*.tmpl"]` harmless.

Behaviour change in 0.20, a bug fix: before, only `*.ext` with a single extension was a
wildcard, and anything else was a prefix. `*.test.ts` and `*.tar.gz` therefore matched
nothing and the files they named were placed anyway. They now match. If you had such a
line and relied on it doing nothing, remove it. The same goes for a line that used to match
nothing because it was not one of the two old shapes, such as `.*`: it was a literal name
with a star in it, and is now a wildcard that ignores every dotfile and dot-directory at any
depth, `.claude` included.

A path in the repository is placed at the same path under `$HOME`, unless the entry says
otherwise. Written as a table, an entry takes the repository path as `path` and where it goes
as `to`, relative to `$HOME`:

```toml
[link]
common  = [".config", { path = "herdr-windows", to = "AppData/Roaming/herdr" }]
windows = [
    { path = "powershell", to = "{documents}/PowerShell" },
    { path = ".config/alacritty", to = "{appdata}/alacritty" },
]
```

`to` may start with a folder in braces: `{home}` anywhere, and, in the `windows` list only,
`{appdata}` (Roaming), `{localappdata}` and `{documents}`; see [Windows](#windows) for how
they are found. The same repository path can be placed differently per platform by listing it
in each platform's list. Everything else — `ignore`, `[render]`, `[modes]` — still names
the path in the repository. When several entries contain a file, the deepest one decides
where it goes, and two entries that would put files at the same place are refused before
anything is linked. Moving or removing a `to` prunes the links at the old place, which is
recorded in the state for that purpose; a place that another entry now uses is relinked,
not pruned. `to` must name something under its folder: `{home}`, `.` or `{appdata}` alone,
and `{appdata}herdr` without the `/`, are refused. sennit 0.21 and earlier refuse a
manifest that uses the table form.

`packages.toml` sits beside `sennit.toml` and is required by `check`, `verify`, `audit`
and `sync`; `apply` does not read it. What was linked is recorded in
`<home>/.local/state/sennit/state.json`, which follows `--home`.

## Commands


| | |
|---|---|
| `sennit apply` | Render templates, then place symlinks. Only touches what needs changing. `--dry-run`, `--secrets`, `--no-backup`. |
| `sennit rollback` | Put back files that an apply moved aside. `--dry-run`. |
| `sennit diff` | Show what an apply would change, before it happens. |
| `sennit list` | Show the current state of every managed path. `--changed`. |
| `sennit render` | Expand templates and decrypt encrypted files. `--secrets`. |
| `sennit check` | Verify that every dependency your configs reference is declared. |
| `sennit verify` | Verify that everything declared actually resolves on this machine. `--export`, `--secrets`. |
| `sennit audit` | Cross-check declarations against shell history, to find ones nothing uses. `--history`. |
| `sennit sync` | Install declared packages that are missing. `--dry-run`. |
| `sennit compare` | Diff two `verify --export` reports, to see how two machines differ. |

`--root` and `--home` override where the repository is and where files are placed; both
default to searching upwards for `sennit.toml` and to `$HOME` (`%USERPROFILE%` on Windows). `compare` is the one
command that does not need a repository at all.

`--dry-run` writes nothing — not the symlinks, and not the generated files either. It does
not call a secret provider or a decryption command, since both can ask a person to unlock
something, which a command that says it changes nothing has no business doing.

Every path is classified as one of five states, and only the ones that need work are
touched:

| | |
|---|---|
| `linked` | already pointing at the right file |
| `missing` | nothing is there |
| `wrong` | a symlink pointing somewhere else |
| `occupied` | a real file or directory is in the way |
| `relink-parent` | a directory above it is a symlink (see below) |

### A symlink above the destination

An older layout may have linked a whole directory (`~/.claude/skills/gog`) somewhere: into
this repository, or into another one. Every file below such a link is the file it leads to,
not a path of its own. Into this repository, placing a link there would mean moving the
repository's file aside and pointing it at itself; into another tree, it would move that
tree's file aside and put a link inside it. So when a directory above a declared
destination is such a symlink, `apply` removes that one symlink, creates a real directory in
its place, and links the files inside it one by one. What the old link led to is never
touched. The state is recorded before anything changes (and again afterwards, since the
state file itself can live behind the link), and `diff`, `list` and `--dry-run` show the
path as `relink-parent`.

A link that led outside the repository is moved to `<name>.sennit-backup` and recorded, so
`sennit rollback` puts the original link back (the directory that replaced it is moved
aside, as with any other path). A link into the repository is simply removed, since it can
be recreated; `--no-backup` drops either.

Two kinds of link are left alone:

- the path the manifest declares, when it leads outside the repository. With
  `common = [".config"]` and `~/.config -> ~/Dropbox/config`, the link is a deliberate
  layout: files are placed inside it, as before 0.20, and `apply` and `diff` say so in one
  line (`note: ~/.config points to ..., outside the repository`). A link *below* the
  declared path (`.config/app`) is still replaced;
- a symlink above a path the repository does not declare.

## What apply will not do


`apply` never destroys a file you wrote. When something that is not a symlink is sitting
where a link should go, it is moved to `<name>.sennit-backup` rather than deleted, and
`sennit rollback` puts it back. The record is written the
moment a file is moved, and accumulates across applies, so neither a later apply nor a
failure partway through can strand a file you can no longer find.

If the same path was moved aside more than once, `rollback` restores the most recent and
tells you where the older copies are, rather than renaming each over the last. It is
idempotent: running it twice does not put an older file back over the one it just
restored. And if you wrote something new at that path after the apply, it is moved aside
too rather than overwritten — restoring a backup should not cost you a different file.

`--no-backup` opts out, and is the only way sennit itself discards a file.

A real directory where a file is to be linked is neither moved aside nor removed:
`apply` (and `--dry-run`, `diff`) refuses before linking anything, and names both. Moving
the directory would take everything else in it along — `{ path = "starship.toml", to =
".config" }` would have moved all of `~/.config`. Up to 0.21 the directory was moved whole,
or with `--no-backup` deleted. For the same reason a `to` that names the folder itself —
`{home}`, `.`, `{documents}` — is refused when the manifest loads.

It also remembers what it linked. A path that was linked last time and is no longer
declared gets its symlink removed, so dropping a config from the repository does not leave
a dangling link behind in `$HOME`. Only links pointing into the repository are removed: if
you replaced one with your own, pointing elsewhere, it is left alone and reported.

Declared paths are relative to the repository root, and one containing `..` or written as
an absolute path is refused when the manifest loads — both would place files outside the
two directories sennit is allowed to touch.

## Templating


Add a `theme.toml` (or any TOML file of values) and declare what is generated from what:

```toml
# sennit.toml
[render]
".config/alacritty/alacritty.toml" = ".config/alacritty/alacritty.toml.tmpl"
```

```toml
# theme.toml
[ui]
bg = "#1a1b26"
fg = "#c0caf5"
```

```toml
# .config/alacritty/alacritty.toml.tmpl
[colors.primary]
background = "{{ ui.bg }}"
foreground = "{{ ui.fg }}"
```

Templates substitute values and, where a platform genuinely differs, drop a block:

```
{{ if sennit.os == "darwin" }}
option_as_alt = "Both"
{{ else }}
option_as_alt = "None"
{{ end }}
```

When a whole output belongs to some platforms only, say so instead of wrapping the file in a
condition. The long form takes the template as `from` and the platforms as `os`, spelled as
in `packages.toml`:

```toml
[render]
".config/alacritty/alacritty.toml" = ".config/alacritty/alacritty.toml.tmpl"   # every OS
".config/zsh/rc/theme.zsh" = { from = ".config/zsh/rc/theme.zsh.tmpl", os = ["darwin", "linux"] }
```

On any other platform the output is not generated, not linked even if an old copy is in the
repository, and a template that reads secrets is not reported as deferred. The template
itself is never placed, on any platform. sennit 0.21 and earlier refuse a manifest that uses
the long form.

`==`, `!=`, and a bare variable meaning "not empty". No loops, no functions, no pipelines
— those are what turn a template into something you can no longer read as the file it
produces. A block that disappears leaves the rest intact.

Only what is inside `{{ }}` is interpreted, so a config containing a literal `[end]` or
`{ if }` is left alone. An unknown variable is an error rather than an empty string, both
in a substitution and in a condition, so a typo cannot quietly ship a broken config. Two
exceptions, both because the alternative makes conditions useless:

- A branch that is being dropped is not read at all — neither its substitutions nor the
  conditions nested inside it. Guarding a Linux-only variable behind
  `{{ if sennit.os == "linux" }}` is the point of having conditions, and evaluating the
  inside would fail on macOS every time.
- `{{ env.SOMETHING }}` that is not set counts as empty in a condition rather than
  failing. Not being set is a normal state for an environment variable, and treating it as
  an error would leave `{{ if env.WORK_LAPTOP }}` writable only for variables that are
  always set.

A template sits next to what it produces — `alacritty.toml.tmpl` beside
`alacritty.toml` — and only the template is committed. Since the generated file is the one
your editor opens through the symlink, it is written read-only: an edit that would be
thrown away by the next render fails to save instead. `[modes]` overrides that if
something really does need to be writable.

Generated files are not meant to be committed. `apply` renders before it links, so a fresh
clone produces them, and adding them to git would only mean the same change showing up
twice in every diff. It also keeps secrets out: a template can pull from 1Password, and
nothing it produces reaches the repository.

```toml
# .config/something.tmpl
token = "{{ op://Personal/GitHub/token }}"
```

## Per-machine variation


`os = ["darwin"]` restricts a declaration to one platform. `profiles` restricts it to a
purpose:

```toml
[packages.slack]
manager  = "brew-cask"
profiles = ["work"]
```

The profile comes from `SENNIT_PROFILE`, which takes a comma-separated list. A declaration
with no `profiles` always applies; one with `profiles` applies only when it overlaps, so
an unset `SENNIT_PROFILE` installs less rather than more.

Templates see the same context, alongside whatever is in your data files:

| | |
|---|---|
| `{{ sennit.os }}` | `darwin`, `linux` or `windows` |
| `{{ sennit.hostname }}` | short hostname |
| `{{ sennit.profile }}` | the current profile list |
| `{{ env.ANYTHING }}` | environment variables |

`data` in `sennit.toml` lists which files to read; it defaults to `theme.toml` alone. It
is a top-level array, so it goes above the first table header:

```toml
data = ["theme.toml", "colors.toml"]
```

## Files that cannot be symlinks

Some files hold settings that are yours alone next to settings you want everywhere, and the
tool that owns them rewrites them itself. Claude Code's `settings.json` is the example:
a hook that only makes sense on one machine sits beside permissions you want on all of
them. A symlink into the repository would put the machine-specific part in git, or be
overwritten by the tool. `[merge]` overlays a shared JSON file onto the real file instead:

```toml
[merge.".claude/settings.json"]     # the output, relative to $HOME like [link]
from = ".claude/settings.shared.json"
```

The rules are the ones a hand-written merge script would use:

- objects are merged recursively;
- arrays and plain values are replaced by the repository's side;
- keys that exist only in the target are left alone, and the target's key order is kept.

Hooks are keyed by event (`PreToolUse`, `Stop`, ...), each holding an array. So an event the
shared file names has its array replaced as a whole — list every hook for that event in
the shared file — while an event only the target has, a machine-specific `Notification`
hook for instance, survives. A key removed from the shared file stays in each target until
you remove it by hand.

`apply` merges after linking and before modes and hooks. It reads the target (a missing one
counts as `{}`), merges, writes it with two-space indent and a trailing newline, and does
nothing if that is what the file already says. A write goes to a temporary file that is then
renamed over the target, so the tool never sees a half-written file; an existing file keeps
its mode and a new one is 0600, unless `[modes]` declares one for the output, in which case
`apply` sets it and `verify` checks it on the file in `$HOME`. The first time sennit touches a target that already exists,
it keeps a copy at `<name>.sennit-backup` and records it with the other backups, so
`sennit rollback` puts the original back. Output names key paths only (`merge
.claude/settings.json (3 key(s))`) and never a value.

`diff`, `list` and `verify` compare the merge result with the target: a target that lacks
or differs in a shared key is `stale`, an absent one is `missing`. Keys the machine added on
its own are not drift. `--dry-run` writes nothing, but still fails on a target that cannot
be merged.

It fails rather than guess:

- a `from` file that is missing, not valid JSON, or not an object fails when the plan is
  built, before anything is placed;
- a target that is not valid JSON (comments and trailing commas included), is not an
  object, is empty (a zero-byte file is not `{}`), or is a symlink is reported and left
  exactly as it was. A file that does not parse is never overwritten;
- an output that is also placed by `[link]` (or rendered, or encrypted) is refused.

Known behaviour, so it does not surprise you:

- the file is parsed and written back, so a number may be written differently from how you
  or the tool wrote it (`1e3` comes back as `1000.0`), and a `\u00e9` escape comes back as
  the character. The value is the same;
- `sennit rollback` puts the first-touch backup back and moves the merged file it replaces
  aside to `<name>.sennit-backup.N`, as it does for any path, so one is left behind;
- the first-touch backup is skipped if `<name>.sennit-backup` already exists, whatever the
  state says. Do not run an older sennit (0.19 or earlier) over a merged file: it rewrites
  the state without knowing about merges.
- the target is read again just before it is replaced. If the tool that owns it wrote in
  between, the merge is redone from what is there, up to three times, and fails rather
  than overwrite it.

The `from` file is never linked into `$HOME`, whether or not `ignore` lists it. Removing a
declaration leaves the target alone; `apply` says once that it is no longer declared.

**Upgrade every machine to 0.20 or later before you push a manifest that uses `[merge]`.**
An older sennit rejects keys it does not know, so it would refuse to read the whole
manifest, not just that section.

## Running things after placing them


Placing a file is often only half of it. `.config/bat/themes/` is useless until
`bat cache --build` has registered what is in it, and until then `BAT_THEME` silently does
not resolve and everything downstream falls back to default colours. That relationship
lived in a shell script, ran unconditionally, and was written down nowhere.

```toml
[hooks.bat-themes]
when-changed = [".config/bat/themes"]
run = "bat cache --build"
```

`apply` runs a hook after linking, and only when what it watches has changed. A hook with
no `when-changed` runs every time. `cwd` sets where it runs, relative to the repository
root; the default is the root itself. A hook that exits non-zero fails the apply — the
links are already placed and recorded by then, so `sennit rollback` still works.

Hooks run under `sh -c`. Windows has no `sh`; see [Windows](#windows) for which shell is
used there.

`os` limits a hook to some platforms, spelled as in `packages.toml`; elsewhere it neither
runs nor forgets when it last ran. `shell` picks what reads `run`: `sh` (the default),
`powershell` (Windows PowerShell) or `pwsh` (PowerShell 7). PowerShell gets the script as
`-EncodedCommand`, with `-NoProfile -NonInteractive` (and `-ExecutionPolicy Bypass` on
Windows), so nothing in it is re-quoted on the way. A PowerShell hook fails the apply when
its last command fails or it calls `exit` with a non-zero code.

```toml
[hooks.bat-themes]
os = ["darwin", "linux"]
when-changed = [".config/bat/themes"]
run = "command -v bat >/dev/null && bat cache --build >/dev/null || true"

[hooks.bat-themes-windows]
os = ["windows"]
shell = "powershell"
when-changed = [".config/bat/themes"]
run = "if (Get-Command bat -ErrorAction Ignore) { bat cache --build | Out-Null }"
```

sennit 0.21 and earlier refuse `os` and `shell`.

## File modes


A config holding a token still works perfectly at mode 0644, which is exactly why nobody
notices. Declare what it should be and `apply` sets it, `verify` checks it:

```toml
[modes]
".npmrc" = "600"
```

A declaration applies to the path it names and nothing else — a directory declaration sets
the directory's own mode, not the modes of the files inside it, and applies whether or not
anything links that path. A directory mode that would take away your own read or execute
bit is refused: sennit has to walk that directory on the next run, and a tool that can lock
itself out of its own repository in one step is not much use. (It used to descend, which
made `".ssh" = "700"` produce an executable `known_hosts` while leaving the directory
untouched, so `verify` failed on it forever.) Since files are placed by symlink, the mode
is set on the copy in your repository, which is the same file your `$HOME` path resolves
to. Three octal digits; `verify` compares the same three.

Without a declaration, generated output is read-only — 0444, or 0400 when it holds a
secret, since the default umask would otherwise publish it. Generated files are created
with that mode rather than chmod-ed afterwards, so a token never sits in a 0644 file even
briefly. A mode must be exactly three octal digits — `"60"` would otherwise be read as
`0060`, quietly producing a file its owner cannot read — and anything else is refused when
the manifest loads: a restriction that silently does not apply is worse than one that
fails loudly. `verify` reports a declared path it could not read rather than passing over
it.

## Windows

sennit runs on Windows (x86_64, MSVC). The model is the same — files are placed as symbolic
links, `[merge]` rewrites a real file, `rollback` puts things back — and a few things differ.
Everything below was exercised on Windows 11 and runs in CI on `windows-latest`.

**Symbolic links need Developer Mode, or an elevated terminal.** Windows only lets an
ordinary process create a symlink once *Settings > System > For developers > Developer
Mode* is on. Without it `apply` stops at the first link with the reason and the fix. It
never falls back to copying: a copy looks placed, then drifts from the repository without
anyone noticing, which is the failure sennit exists to prevent. Windows also wants to know
whether a link points at a file or a directory when it is made; sennit looks at the target
and creates the right kind.

**What goes in `[link]`.** Windows uses `common` and a new `windows` list, the way macOS
uses `common` and `darwin`:

```toml
[link]
common  = [".config", ".claude"]
windows = [".gitconfig"]
```

An older sennit rejects the `windows` key along with the rest of the manifest, so upgrade
every machine before you push a manifest that uses it. (`packages.toml` accepts
`os = ["windows"]` too.)

**Where `$HOME` is.** `%USERPROFILE%`, then `HOME` if that is unset. `--home` overrides
either, which is also how to try sennit on a scratch directory.

**Known folders.** A `to` in the `windows` list can start with `{appdata}`,
`{localappdata}` or `{documents}`. sennit asks Windows for them (`SHGetKnownFolderPath`)
rather than assuming `%USERPROFILE%\Documents`, because Documents is often moved — OneDrive's
backup of the Documents folder is the usual reason — and PowerShell reads its profile from
wherever Documents really is:

```toml
[link]
windows = [{ path = "Documents/PowerShell", to = "{documents}/PowerShell" }]
```

With `--home`, they are the folders' default places under that directory
(`AppData\Roaming`, `AppData\Local`, `Documents`), so a trial run against a scratch
directory never reaches your real profile.

**User environment variables.** Some tools only find their config through a variable —
on Windows, bat reads `%APPDATA%\bat` unless `BAT_CONFIG_DIR` says otherwise, and git,
lazygit and nvim look under `~/.config` only when `XDG_CONFIG_HOME` is set. `[env.windows]`
declares them:

```toml
[env.windows]
XDG_CONFIG_HOME   = "{home}/.config"
BAT_CONFIG_DIR    = "{home}/.config/bat"
HERDR_CONFIG_PATH = "{appdata}/herdr/config.toml"
GH_CONFIG_DIR     = { value = "{appdata}/GitHub CLI", if-unset = true }
```

- A value that starts with a folder (`{home}`, `{appdata}`, `{localappdata}`,
  `{documents}`, found as for `to`) is a path, and every `/` in it becomes `\`. Any other
  value is written exactly as given.
- `apply` writes each value that differs to `HKCU\Environment` as `REG_SZ`, then broadcasts
  `WM_SETTINGCHANGE` so Explorer — and the terminals it starts afterwards — pick them up.
  Terminals already open keep their old environment. Hooks in the same `apply` already see
  the new values.
- `if-unset = true` sets the variable only when the user has none; a value of their own is
  left alone, and `verify` accepts it.
- `verify` reports a variable that is unset or holds something else.
- With `--home`, nothing is written: there is one user environment, and a trial run must not
  point it at a scratch directory. `apply` prints what it would set, and `verify` skips the
  check. `--dry-run` prints the same.
- `Path` cannot be declared: as a single `REG_SZ` it would replace the whole user `Path`,
  including what installers appended to it.
- Removing a variable from `[env.windows]` does not unset it.

**On macOS and Linux, `[env]` does nothing.** There is no user environment to write to
there — a shell's environment comes from its startup files, which sennit already places —
so the only table is `windows` (`[env.darwin]` is refused as an unknown key), and `apply`
and `verify` ignore it on other platforms. sennit 0.21 and earlier refuse a manifest with
`[env]`.

**Paths.** Link targets are written with backslashes and without the `\\?\` prefix that
Windows' path resolution adds. A link that already points at the right file is recognised
whatever its case or separators, so `C:/Repo/a` and `c:\repo\a` are the same place.
`sennit.toml` still uses forward slashes, as on every platform; a declared path that is
rooted (`\foo`) or has a drive (`C:foo`) is refused like an absolute one.

**`[modes]` is not applied on Windows.** There are no POSIX permission bits to set, so
`apply` and `verify` skip every `[modes]` entry and say so once on stderr. The only
mapping is for generated files (rendered templates, decrypted files): they are still made
read-only, as the read-only attribute, when their effective mode has no owner write bit —
the default for generated output, 0444 or 0400. A `[merge]` output keeps the read-only
attribute of the file it replaces.

**Hooks run under Git for Windows' bash**, unless the hook says `shell = "powershell"` or
`"pwsh"` (see [Running things after placing them](#running-things-after-placing-them)).
Hook scripts are written for a POSIX shell by default, so sennit runs `bash.exe -c "<run>"`
from the first of these it finds:

1. the file named by the `SENNIT_SHELL` environment variable (any `bash.exe` or `sh.exe`);
2. Git for Windows' `bin\bash.exe`, found from `git.exe` on `PATH`, then from
   `%ProgramFiles%\Git` and `%LOCALAPPDATA%\Programs\Git`;
3. any other `bash.exe` or `sh.exe` on `PATH` (MSYS2, Cygwin).

The `bash.exe` in `System32` or `WindowsApps` is skipped on purpose: on a machine with WSL it
is the WSL launcher, which would run the hook inside a Linux VM where the repository's
Windows paths mean nothing. If none is found, a hook fails with an error that says so; a
manifest without hooks needs no shell at all. A hook's `cwd` and the environment are
passed through as they are.

**Packages are installed with winget.** `sennit sync` drives winget from the `winget`
entries in `packages.toml`; see [Installing packages](#installing-packages). Up to 0.21 it
refused to run on Windows, and `verify` treated every package as not verifiable.

**Not supported on Windows.**

- sennit never schedules anything: there is no `autosync`-style timer to port. If you want
  `apply` on a schedule, create a Task Scheduler task that runs `sennit apply` yourself.
- Directory junctions. Windows reports a junction as a mount point, not a symbolic link,
  so sennit sees an ordinary directory there and never replaces it. Use symbolic links.

## Secrets

Providers are declared rather than built in, since they all have the same shape — run a
command, read what it prints:

```toml
[providers.op]
command = "op read --no-newline op://{}"

[providers.pass]
command = "pass show {}"

[providers.vault]
command = "vault kv get -field=value {}"

[providers.keychain]
command = "security find-generic-password -s {} -w"
```

`{}` becomes the part after `://`, passed as an argument rather than through a shell. With
nothing declared, `op` is assumed. A reference to an undeclared scheme fails and lists the
ones that are, as long as some provider is declared. `trim = false` keeps the trailing
newline, which is otherwise dropped since most CLIs add one.

Any such reference is read at render time — but only when you ask for it. `apply` skips
those templates and names the providers they need; `apply --secrets` renders them.

That split is not a convenience. 1Password needs a person to sign in, enable the CLI
integration, and unlock the app, none of which can happen partway through an unattended
install, and none of which exist at all on a headless Linux box or inside a container. If
secrets were rendered by default, the first run on a new machine would always fail. This
way `apply` always succeeds, and the secret-bearing files simply are not there until you
run it again with `--secrets`.

A template that references `op://` also makes `check` require the `op` command, so the
dependency shows up the moment it exists rather than on whichever machine first tries to
use it.

## Checking that secrets still work

A secret being present and a secret being accepted are different facts. A token that was
revoked sits in your vault, in the generated file, and on the machine, and nothing that
only looks will notice. Declare how to ask the service, and `verify --secrets` asks:

```toml
[probes.github]
secret  = "op://Cloud/GitHub PAT/token"
command = "curl -fsS --max-time 10 -o /dev/null -H @- https://api.github.com/user"
input   = "Authorization: Bearer {}"
invalid-exit = [22]
```

`secret` is a reference resolved through `[providers]`, the same as in a template. The
check is a command, run without a shell, and an exit status of 0 means the secret is
accepted. What counts as "accepted" depends on the service, so sennit does not know any
of them; the command is yours.

The value reaches the command on standard input only, shaped by `input` with `{}` standing
for the secret. It is never an argument, which other processes can read with `ps`, and
never printed: the command's output is discarded, and the report names the probe and the
result, nothing else.

Three outcomes, because they mean different things:

| | |
|---|---|
| accepted | the command exited 0 |
| `invalid` | the command exited with a status in `invalid-exit` (any non-zero status, if the list is empty) |
| `unchecked` | the secret could not be read, the command could not run or timed out, or it exited with a status `invalid-exit` does not list |

`invalid-exit` is what keeps "the network is down" from being reported as "the token was
revoked": curl exits 22 on an HTTP error and a different status when it cannot connect.
Both `invalid` and `unchecked` fail the command. A check that could not run is not a pass;
otherwise a CI job with a locked vault would be green without having checked anything.
`timeout` (seconds, default 30) stops a check that hangs.

Like `apply --secrets`, this is opt-in. Reading the secret may need someone to unlock the
manager, and the check goes over the network, so plain `verify` does neither. With
`--secrets` the package and mode checks still run, and both sets of results are reported.

## Encrypted files


A provider fetches a value from somewhere else. Encryption goes the other way: the secret
lives in the repository, and a key opens it.

```toml
[encryption]
command  = "age -d -i ~/.config/sennit/age.key {}"
identity = "~/.config/sennit/age.key"

[encrypted]
".ssh/config" = ".ssh/config.age"
```

The command takes the ciphertext path as `{}` and prints the plaintext. `age`, `gpg -d`
and `sops -d` all fit.

The difference from a provider matters more than it looks. Nothing has to be signed into
and nothing has to be unlocked by a person, so this works unattended — on a fresh machine,
in a container, in CI — provided the key is there. Where 1Password cannot help during a
first install, this can. The two cover different halves of the problem.

If the declared `identity` is absent, the file is deferred rather than failed: not having
put the key on this machine yet is a different thing from being broken. Decrypted output
is written 0400 unless `[modes]` says otherwise. sennit does not touch `.gitignore`;
add the generated paths to it yourself.

## Installing packages


`sync` reads the same `packages.toml` and installs what is missing:

```sh
sennit sync --dry-run
sennit sync
```

It asks each manager what is already installed and only installs the difference, so
idempotency does not depend on the manager's own behaviour.

| manager | |
|---|---|
| `brew` | default; works on Linux too via Homebrew on Linux |
| `brew-cask` | macOS only, except font casks which install on Linux as well |
| `mise` | runtimes |
| `apt` / `yay` | Linux; selected automatically from what the machine has |
| `winget` | Windows; see below |
| `zed-extension` | declared so `check` knows about it; the editor installs it |
| `none` | declared, but sennit does not install it |

A `manager` outside that list is refused when `packages.toml` loads, rather than dropping
the package from `sync` while `verify` goes on reporting it as missing.

`kind` says what sort of thing it is — `command` (the default), `font`, `extension`, or
`library` — which is how `verify` knows what it can judge.

Package names differ between distributions, so declare them where they do:

```toml
[packages.libyaml]
apt = "libyaml-dev"      # on Debian-like systems, use apt with this name
yay = "libyaml"          # on Arch-like systems, use yay with this name
```

On Linux, an `apt` or `yay` entry decides both the manager and the name. Without one,
the default `manager` is used. Entries marked `optional`, or restricted to another OS via
`os = ["darwin"]`, are skipped. Editor extensions are declared so that `check` knows about
them, but are installed by the editor itself.

Managers run in dependency order — `apt`/`yay` first, since distribution packages are what
Homebrew sits on (and `winget` on Windows), then `brew`, `brew-cask`, and `mise` last because
`brew` or `winget` is what installs it.

On Windows, a `winget` entry decides both the manager and the name, the way `apt` and `yay`
do on Linux. The value is the winget package ID, as `winget search` shows it:

```toml
[packages]
starship = { winget = "Starship.Starship" }
ripgrep  = { provides = ["rg"], winget = "BurntSushi.ripgrep.MSVC" }

[packages.powershell]
provides = ["pwsh"]
os       = ["windows"]
winget   = { id = "Microsoft.PowerShell", upgrade = true }   # upgraded on every sync
```

Without a `winget` entry, a package is installed on Windows only if its `manager` is `mise`
(or `winget`, with the package name as the ID); `brew`, `brew-cask`, `apt` and `yay` do not
exist there, so those packages are left out of `sync` and `verify` treats them as not
verifiable. Other platforms ignore `winget` entirely.

`sync` asks `winget list --id <ID> --exact` about each package, and installs the ones that
are missing with `winget install --id <ID> --exact --source winget`, accepting the source and
package agreements and never prompting. A package winget does not list but whose command
(its `provides`, or its name) is already on `PATH` is left alone and shown with `=`: it was
installed some other way, with scoop or by hand, and a second copy would only compete with
it. `upgrade = true` runs `winget upgrade` on an installed package every time.

winget's exit codes are read by what they mean rather than as "non-zero is a failure":

| Code | Meaning | `sync` |
| --- | --- | --- |
| 0x8A15002B, 0x8A15004F, 0x8A150050, 0x8A150068 | upgrade: no newer version, not newer, version unknown, pinned | success |
| 0x8A150061, 0x8A15010D | install: already installed | success |
| 0x8A150109 | restart to finish | success, with a note to restart |
| 0x8A150101, 0x8A150103 | the application or a file is in use | skipped, with a warning |

The in-use case is mostly PowerShell itself: `sync` run from PowerShell 7 cannot upgrade
`Microsoft.PowerShell` while it is running, so close it, run `sync` from another shell, and
the upgrade goes through. Any other failure does not stop the remaining packages; `sync`
fails at the end and names each one with its exit code.

`verify` checks a winget package's command on `PATH` first, and asks winget about the ID only
when that fails, so an application without a command, or one installed after this shell
started, is not reported missing.

`apt` needs root. sennit works out how to get it before running anything: as root it calls
`apt-get` directly, otherwise it uses `sudo` when that is passwordless or when there is a
terminal for `sudo` to prompt on. When neither holds — a script or a container build where
`sudo` would block on a password nobody can type — it stops with an explanation rather than
hanging. `yay` is never run through `sudo`, since it refuses to run as root.

`sync` does not replace your bootstrap script: something still has to install Homebrew,
or `sudo`, before sennit can run at all.

## Three ways of being wrong


Declarations go stale in three different directions, and each needs a different kind of
evidence:

| | question | evidence |
|---|---|---|
| `check` | is everything the configs need declared? | the repository |
| `verify` | does everything declared actually exist here? | this machine |
| `audit` | does anything actually use it? | shell history |

`check` is static and machine-independent, so it can gate CI. `verify` catches a
declaration that names something wrong — Homebrew's formula is `gnupg`, not `gpg`, and the
difference is invisible until you look at the machine. It only judges what can be judged:
commands on `PATH` and installed font families. GUI applications, libraries, editor
extensions, and anything managed by `mise` are counted and skipped, because their absence
from `PATH` means nothing.

`audit` covers the gap the other two cannot see: tools you only ever type. `rg` and `fd`
appear in no config file, so removing their declarations breaks nothing that `check` can
notice. It cross-references history with the configs, so a tool that runs automatically —
`starship`, `delta` — is not mistaken for an unused one. It never fails the build: history
is per machine and gets trimmed, so absence is a prompt to look, not proof.

None of the three can answer "what breaks if I remove this". That needs removing it and
running the install, which is a job for CI rather than for this binary.

## Drift detection


The problem `check` solves is specific: **you update a config, and forget to update the
package list.** The config references a tool that a fresh machine will never install, and
nothing tells you until you set up that machine months later.

```toml
# packages.toml
[packages]
git-delta = { provides = ["delta"] }   # the command it installs is named differently
neovim    = { provides = ["nvim"] }

[packages.font-hack-nerd-font]
manager  = "brew-cask"
kind     = "font"
provides = ["Hack Nerd Font Mono"]

[packages.kubectl]
optional = true   # known dependency, deliberately not installed automatically

[ignore]
commands = ["brew", "git", "curl"]   # provided by the system or the bootstrap
```

`check` reads your configs with format-aware detectors rather than grepping for strings:

- commands invoked from `git config` (`core.pager`, `interactive.diffFilter`, credential helpers)
- font families in terminal and editor settings
- editor extensions declared for auto-install
- commands used in shell startup files
- the mere existence of `.config/<tool>/`, which is sometimes the *only* evidence of a dependency
  — and likewise `AppData/Roaming/<tool>/` and `AppData/Local/<tool>/` for Windows, and a
  `[link]` entry whose `to` places something there (`to = "{appdata}/herdr"` counts as
  `herdr`). These are read whatever platform `check` runs on, so CI on macOS also catches a
  Windows config whose tool `packages.toml` forgot

Detectors are written per format, so a format nobody wrote one for is invisible. Rather
than wait for that, a config can say so itself, in whatever passes for a comment there:

```
# sennit: requires command hunk
# sennit: requires font "Hack Nerd Font Mono"
```

Anything you deliberately do not install is declared `optional = true`, so the file records
what is intentional rather than hiding it in an ignore list.

`check` also looks the other way, at fonts and editor extensions that are declared but
that nothing references. That is the drift you get when a config stops using something and
the package stays behind — easy to miss, because everything keeps working. Commands are
left out of this direction on purpose: plenty of them (`bat`, `fd`, `rg`) are used daily
from the shell without appearing in any config file.

## Status


v0.21. Minimum supported Rust version is 1.90.

The author uses it to manage [ken109/dotfiles](https://github.com/ken109/dotfiles); if you
adopt it, start with `sennit diff` and `--dry-run` before the first `apply`, since `apply`
will replace whatever is currently sitting at a managed path.

## License


MIT
