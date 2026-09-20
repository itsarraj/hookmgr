# hookmgr

A multi-hook-type git hook manager driven by one `.hookmgr.toml`. Husky
(Node) and the `pre-commit` framework (Python) both do this job well,
but both mean a language runtime and a package manager show up as a
prerequisite for running your commit hooks — this is a static binary
that reads a config file and runs shell commands, nothing else.

## Usage

```bash
hookmgr install                  # install a dispatcher for every hook type in .hookmgr.toml
hookmgr install pre-commit       # install just one
hookmgr list                     # show what's configured
hookmgr uninstall                # remove hookmgr's own block from every installed hook
hookmgr run pre-commit           # run the configured pre-commit commands directly (mostly for testing a config)
```

`.hookmgr.toml`, at the repo root:

```toml
[hooks]
pre-commit = [
  "cargo fmt --check",
  "! git diff --cached | grep -q FORBIDDEN_TOKEN",
]
commit-msg = ["grep -qE '^(feat|fix|chore): ' \"$1\""]
pre-push = ["! git rev-parse --abbrev-ref HEAD | grep -q wip"]
```

Any key under `[hooks]` can be any real git hook name — `pre-commit`,
`commit-msg`, `pre-push`, `post-checkout`, `pre-rebase`, whatever git
itself calls. `hookmgr` doesn't special-case any of them; it just
installs a dispatcher into `.git/hooks/<name>` for each key present and
runs that key's command list, in order, when git invokes it. Each
command runs through `sh -c`, in the repo root, with git's own arguments
to that hook forwarded as `$1 $2 ...` — so a `commit-msg` command can
read `"$1"` for the path to the message file exactly like a hand-written
hook would. The first command that exits non-zero stops the list right
there and that exit code becomes the hook's exit code, which is what
actually blocks the commit/push — same fail-fast behavior Husky and
`pre-commit` both give you.

## Installing without clobbering what's already there

`hookmgr install` never assumes `.git/hooks/<name>` is empty or already
its own. It writes a marked block (`# >>> hookmgr <name> >>>` /
`# <<< hookmgr <name> <<<`) and:

- if the hook file doesn't exist yet, creates it with just that block;
- if it exists and already has hookmgr's block (a previous install),
  replaces just that block in place — re-running `install` after
  editing `.hookmgr.toml` doesn't duplicate anything;
- if it exists with *someone else's* content — a hand-written script,
  something Husky or another tool installed — appends the block after
  it, and adds a `#!/bin/sh` shebang first if the file didn't have one.
  The existing content keeps running exactly as it did before.

`hookmgr uninstall` is the same idea in reverse: it removes just the
marked block. If that block was the only real content in the file, the
whole file is deleted; if there was other content around it, that
content is written back untouched.

## Status: built, and live-verified in a real scratch git repo across all three hook types the config example above uses

- **16 unit tests** (`cargo test --lib`): config parsing (multiple hook
  types, command order preserved, a missing file returning `None`
  rather than an error while a *present-but-invalid* TOML file is a
  hard error); hook install for a fresh file, appending to an existing
  unrelated hook without destroying its content, idempotent re-install
  (no duplicated block across three repeated runs), and independent
  handling of three distinct hook types installed side by side;
  uninstall removing the whole file when the block was the only content,
  preserving unrelated content when it wasn't, and a no-op on a hook
  that was never installed; and the command runner — a silent no-op
  with no config file at all, a silent no-op for a hook type nobody
  configured, multiple commands actually running in the configured
  order (verified via one command's file-existence check gating the
  next, not just call counting), stopping dead at the first failing
  command (confirmed a later command's side effect never happens),
  propagating a specific non-zero exit code (`7`) all the way through,
  and `$1` actually reaching a command's shell environment (a
  `commit-msg`-shaped test that greps a real message file passed as an
  argument).
- **Full live run in a real scratch git repo**, using the exact config
  from the Usage section above, with `hookmgr install` on `PATH` so the
  installed dispatcher's own `command -v hookmgr` check succeeds:
  - **Non-destructive install confirmed first**: a `pre-commit` hook
    that echoes `run by pre-existing husky-style hook` was written by
    hand *before* running `hookmgr install`. After install, that echo
    is still the first line of the file, hookmgr's block is appended
    after it, and — critically — the echo actually ran on every
    subsequent real commit below, proving the pre-existing hook wasn't
    just left in the file but kept executing.
  - **`pre-commit` genuinely blocked** a real `git commit` whose staged
    diff contained `FORBIDDEN_TOKEN`: exit code 1, `git log` unchanged,
    the file still sitting staged afterward.
  - **`commit-msg` genuinely blocked** a real `git commit -m "this
    message has no valid prefix"`: exit code 1, no new commit.
  - **A fully passing commit** (clean content, `chore: add a clean
    file`) **succeeded**: exit code 0, a real new commit landed.
  - **`pre-push` genuinely blocked** a real `git push` from a branch
    named `wip-experiment` to a real local bare repo: exit code 1, and
    `git ls-remote` on the bare repo afterward showed nothing was
    received.
  - **Pushing from `main`** to that same bare repo **succeeded**: exit
    code 0, `git ls-remote` showed the real pushed commit.
  - **`hookmgr uninstall` verified after all of that**: the `pre-commit`
    file kept the hand-written husky-style line and lost only hookmgr's
    block; `commit-msg` and `pre-push`, which had no other content, were
    deleted entirely.
  - The scratch repo and bare remote were both deleted after the run.

**Not done / deliberately deferred**: no per-hook `--no-verify`-style
bypass flag of its own (git's own `--no-verify` already skips the
dispatcher script entirely, which is arguably enough); no parallel
command execution within a single hook (each command runs and completes
before the next starts, which is the same model Husky and `pre-commit`
both use, but means a slow `cargo test` in `pre-push` blocks a fast
lint after it rather than running alongside it); and no equivalent of
`pre-commit`'s per-language "hook environments" (a command that needs
`node_modules` or a `venv` present has to arrange that itself — this
tool only ever hands your command string to `sh -c`).
