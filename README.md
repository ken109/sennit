# sennit

A dotfiles manager that keeps symlink semantics, and adds templating and drift detection.

> *sennit* — braided cordage, made by plaiting many strands into a single line.

[![crates.io](https://img.shields.io/crates/v/sennit.svg)](https://crates.io/crates/sennit)
[![CI](https://github.com/ken109/sennit/actions/workflows/ci.yml/badge.svg)](https://github.com/ken109/sennit/actions/workflows/ci.yml)
[![license](https://img.shields.io/crates/l/sennit.svg)](LICENSE)

```sh
brew install ken109/tap/sennit   # or: cargo install sennit
```

- [Why another one?](#why-another-one)
- [Quick start](#quick-start)
- [Commands](#commands)
- [What apply will not do](#what-apply-will-not-do)
- [Templating](#templating)
- [Per-machine variation](#per-machine-variation)
- [Running things after placing them](#running-things-after-placing-them)
- [File modes](#file-modes)
- [Secrets](#secrets)
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

`ignore` takes two kinds of pattern: `*.ext` matches by extension, and anything else is a
path prefix rooted at the repository — whole components, from the top. `conf/nvim` matches
`conf/nvim/init.lua` but not `conf/nvim-extra`, and `README.md` matches only a `README.md`
at the root, not one inside a subdirectory. Declared `[render]` templates and `[encrypted]`
ciphertexts are excluded whether or not you list them, which is what makes a forgotten
`ignore = ["*.tmpl"]` harmless.

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
| `sennit verify` | Verify that everything declared actually resolves on this machine. `--export`. |
| `sennit audit` | Cross-check declarations against shell history, to find ones nothing uses. `--history`. |
| `sennit sync` | Install declared packages that are missing. `--dry-run`. |
| `sennit compare` | Diff two `verify --export` reports, to see how two machines differ. |

`--root` and `--home` override where the repository is and where files are placed; both
default to searching upwards for `sennit.toml` and to `$HOME`. `compare` is the one
command that does not need a repository at all.

`--dry-run` writes nothing — not the symlinks, and not the generated files either. It does
not call a secret provider or a decryption command, since both can ask a person to unlock
something, which a command that says it changes nothing has no business doing.

Every path is classified as one of four states, and only the ones that need work are
touched:

| | |
|---|---|
| `linked` | already pointing at the right file |
| `missing` | nothing is there |
| `wrong` | a symlink pointing somewhere else |
| `occupied` | a real file or directory is in the way |

## What apply will not do


`apply` never destroys a file you wrote. When something that is not a symlink is sitting
where a link should go, it is moved to `<name>.sennit-backup` rather than deleted, and
`sennit rollback` puts it back. Directories are moved whole. The record is written the
moment a file is moved, and accumulates across applies, so neither a later apply nor a
failure partway through can strand a file you can no longer find.

If the same path was moved aside more than once, `rollback` restores the most recent and
tells you where the older copies are, rather than renaming each over the last. It is
idempotent: running it twice does not put an older file back over the one it just
restored. And if you wrote something new at that path after the apply, it is moved aside
too rather than overwritten — restoring a backup should not cost you a different file.

`--no-backup` opts out, and is the only way sennit itself discards a file; on a directory
it removes the whole tree, and says how many files that is.

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
| `{{ sennit.os }}` | `darwin` or `linux` |
| `{{ sennit.hostname }}` | short hostname |
| `{{ sennit.profile }}` | the current profile list |
| `{{ env.ANYTHING }}` | environment variables |

`data` in `sennit.toml` lists which files to read; it defaults to `theme.toml` alone. It
is a top-level array, so it goes above the first table header:

```toml
data = ["theme.toml", "colors.toml"]
```

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

## Secrets

Providers are declared rather than built in, since they all have the same shape — run a
command, read what it prints:

```toml
[providers.op]
command = "op read --no-newline {}"

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
Homebrew sits on, then `brew`, `brew-cask`, and `mise` last because `brew` is what installs
it.

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


v0.18. Minimum supported Rust version is 1.90.

The author uses it to manage [ken109/dotfiles](https://github.com/ken109/dotfiles); if you
adopt it, start with `sennit diff` and `--dry-run` before the first `apply`, since `apply`
will replace whatever is currently sitting at a managed path.

## License


MIT
