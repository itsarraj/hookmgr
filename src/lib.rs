//! Core logic for `hookmgr`: one `.hookmgr.toml` config maps git hook
//! names (`pre-commit`, `commit-msg`, `pre-push`, or any other real git
//! hook name) to a list of shell commands, and this crate can install a
//! thin dispatcher script into `.git/hooks/<name>` for each configured
//! hook, run the configured commands for a given hook in order, and
//! uninstall its own managed block again without disturbing anything
//! else that happens to be in that hook file.
//!
//! The install/uninstall half follows the same non-destructive-append
//! pattern `leakscan`'s `install-hook` uses (marked block, replace in
//! place on re-install, append rather than overwrite when the hook file
//! already has unrelated content) generalized to work for any hook name
//! and to be reversible.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use serde::Deserialize;

pub const CONFIG_FILENAME: &str = ".hookmgr.toml";

#[derive(Debug, Deserialize, Default, Clone, PartialEq, Eq)]
pub struct HookConfig {
    #[serde(default)]
    pub hooks: BTreeMap<String, Vec<String>>,
}

/// Reads `.hookmgr.toml` from `repo_dir`. `Ok(None)` means the file
/// simply isn't there (a legitimate, common state — nothing configured
/// yet, or removed since a hook was installed); a `Toml` parse failure
/// on a file that *does* exist is still a hard `Err`, since that's a
/// real misconfiguration the caller should see, not silently ignore.
pub fn load_config_if_present(repo_dir: &Path) -> Result<Option<HookConfig>> {
    let path = repo_dir.join(CONFIG_FILENAME);
    if !path.is_file() {
        return Ok(None);
    }
    let content =
        fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let config: HookConfig =
        toml::from_str(&content).with_context(|| format!("parsing {}", path.display()))?;
    Ok(Some(config))
}

/// Same as [`load_config_if_present`] but a missing file is itself an
/// error — used by `install`/`list`, where "nothing to do" should be
/// said plainly rather than silently succeeding.
pub fn load_config(repo_dir: &Path) -> Result<HookConfig> {
    load_config_if_present(repo_dir)?
        .ok_or_else(|| anyhow::anyhow!("no {CONFIG_FILENAME} found in {}", repo_dir.display()))
}

fn markers(hook_name: &str) -> (String, String) {
    (
        format!("# >>> hookmgr {hook_name} >>>"),
        format!("# <<< hookmgr {hook_name} <<<"),
    )
}

fn hook_block(hook_name: &str) -> String {
    let (start, end) = markers(hook_name);
    format!(
        "{start}\n\
# Installed by `hookmgr install`. Safe to re-run to update this block in\n\
# place; do not hand-edit between the markers. The commands that actually\n\
# run live in {CONFIG_FILENAME}, not here.\n\
if command -v hookmgr >/dev/null 2>&1; then\n\
    hookmgr run {hook_name} -- \"$@\"\n\
else\n\
    echo \"hookmgr: not found on PATH, skipping {hook_name} checks\" >&2\n\
fi\n\
{end}\n"
    )
}

/// If `existing` already contains a hookmgr block for this hook (from a
/// previous `install`), returns `existing` with that block swapped for
/// `new_block` in place — re-running `install` after upgrading a config
/// doesn't duplicate the block.
fn replace_marked_block(existing: &str, hook_name: &str, new_block: &str) -> Option<String> {
    let (start_marker, end_marker) = markers(hook_name);
    let start = existing.find(&start_marker)?;
    let end_marker_pos = existing[start..].find(&end_marker)? + start;
    let end = end_marker_pos + end_marker.len();
    let mut out = String::with_capacity(existing.len());
    out.push_str(&existing[..start]);
    out.push_str(new_block.trim_end_matches('\n'));
    out.push('\n');
    let mut rest = &existing[end..];
    if let Some(stripped) = rest.strip_prefix('\n') {
        rest = stripped;
    }
    out.push_str(rest);
    Some(out)
}

fn hooks_dir(repo_dir: &Path) -> PathBuf {
    repo_dir.join(".git").join("hooks")
}

/// Installs (or updates) the dispatcher for one hook type in
/// `repo_dir/.git/hooks/<hook_name>`.
///
/// - No existing hook file: writes a fresh `#!/bin/sh` script containing
///   just the managed block.
/// - An existing file that's already ours (contains the marker):
///   replaces just the marked block in place.
/// - An existing file that's someone else's (a hand-written script, a
///   Husky-installed hook, another tool's hook): appended to, never
///   overwritten. Destroying an existing hook silently is a much worse
///   failure mode than this tool's own check getting skipped.
pub fn install_hook(repo_dir: &Path, hook_name: &str) -> Result<PathBuf> {
    let dir = hooks_dir(repo_dir);
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let hook_path = dir.join(hook_name);
    let block = hook_block(hook_name);

    let new_contents = if hook_path.exists() {
        let existing = fs::read_to_string(&hook_path)
            .with_context(|| format!("reading {}", hook_path.display()))?;
        if let Some(replaced) = replace_marked_block(&existing, hook_name, &block) {
            replaced
        } else {
            let mut updated = existing;
            if !updated.starts_with("#!") {
                updated = format!("#!/bin/sh\n{updated}");
            }
            if !updated.ends_with('\n') {
                updated.push('\n');
            }
            updated.push('\n');
            updated.push_str(&block);
            updated
        }
    } else {
        format!("#!/bin/sh\n{block}")
    };

    fs::write(&hook_path, new_contents)
        .with_context(|| format!("writing {}", hook_path.display()))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&hook_path, fs::Permissions::from_mode(0o755))
            .with_context(|| format!("making {} executable", hook_path.display()))?;
    }

    Ok(hook_path)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UninstallOutcome {
    /// The hook file didn't exist, or existed but had no hookmgr block
    /// in it — nothing for `uninstall` to do.
    NotInstalled,
    /// The hookmgr block was the only real content in the file (modulo
    /// the shebang), so the whole file was removed.
    FileDeleted,
    /// The hookmgr block was removed but other content — someone else's
    /// hook, hand-written before or after installing — was preserved.
    BlockRemoved,
}

/// Removes just this tool's managed block from `.git/hooks/<hook_name>`,
/// preserving any unrelated content the same way `install_hook` refused
/// to clobber it going in.
pub fn uninstall_hook(repo_dir: &Path, hook_name: &str) -> Result<UninstallOutcome> {
    let hook_path = hooks_dir(repo_dir).join(hook_name);
    if !hook_path.is_file() {
        return Ok(UninstallOutcome::NotInstalled);
    }
    let contents = fs::read_to_string(&hook_path)
        .with_context(|| format!("reading {}", hook_path.display()))?;
    let (start_marker, end_marker) = markers(hook_name);
    let Some(start) = contents.find(&start_marker) else {
        return Ok(UninstallOutcome::NotInstalled);
    };
    let Some(end_marker_pos) = contents[start..].find(&end_marker).map(|p| p + start) else {
        return Ok(UninstallOutcome::NotInstalled);
    };
    let end = end_marker_pos + end_marker.len();

    let mut remainder = String::with_capacity(contents.len());
    remainder.push_str(&contents[..start]);
    let mut rest = &contents[end..];
    if let Some(stripped) = rest.strip_prefix('\n') {
        rest = stripped;
    }
    remainder.push_str(rest);

    let trimmed = remainder.trim();
    if trimmed.is_empty() || trimmed == "#!/bin/sh" {
        fs::remove_file(&hook_path).with_context(|| format!("removing {}", hook_path.display()))?;
        Ok(UninstallOutcome::FileDeleted)
    } else {
        fs::write(&hook_path, remainder)
            .with_context(|| format!("writing {}", hook_path.display()))?;
        Ok(UninstallOutcome::BlockRemoved)
    }
}

/// Runs the commands configured for `hook_name`, in order, each via
/// `sh -c`, with `args` forwarded as `$1 $2 ...` (the shape git itself
/// passes to hooks like `commit-msg`, which gets the path to the
/// message file as `$1`). Stops at the first command that exits
/// non-zero and returns that exit code — the same fail-fast behavior
/// Husky/pre-commit give you.
///
/// Returns `Ok(0)` — not an error — when there's no `.hookmgr.toml` at
/// all, or when it exists but has nothing configured for this
/// particular hook: an *installed* dispatcher calling into a hook type
/// nobody configured should be a silent no-op, not a broken commit.
pub fn run_commands(repo_dir: &Path, hook_name: &str, args: &[String]) -> Result<i32> {
    let Some(config) = load_config_if_present(repo_dir)? else {
        return Ok(0);
    };
    let Some(commands) = config.hooks.get(hook_name) else {
        return Ok(0);
    };

    for command in commands {
        let status = Command::new("sh")
            .arg("-c")
            .arg(command)
            .arg("sh")
            .args(args)
            .current_dir(repo_dir)
            .status()
            .with_context(|| format!("running configured `{hook_name}` command: {command}"))?;
        if !status.success() {
            return Ok(status.code().unwrap_or(1));
        }
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn scratch_dir(name: &str) -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "hookmgr-test-{}-{}-{}",
            std::process::id(),
            name,
            n
        ));
        fs::create_dir_all(dir.join(".git").join("hooks")).unwrap();
        dir
    }

    fn write_config(dir: &Path, toml_body: &str) {
        fs::write(dir.join(CONFIG_FILENAME), toml_body).unwrap();
    }

    #[test]
    fn missing_config_is_none_not_an_error() {
        let dir = scratch_dir("missing-config");
        assert!(load_config_if_present(&dir).unwrap().is_none());
        assert!(load_config(&dir).is_err());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn parses_multiple_hook_types_in_order() {
        let dir = scratch_dir("parse-multi");
        write_config(
            &dir,
            "[hooks]\npre-commit = [\"echo one\", \"echo two\"]\ncommit-msg = [\"echo three\"]\n",
        );
        let config = load_config(&dir).unwrap();
        assert_eq!(
            config.hooks.get("pre-commit").unwrap(),
            &vec!["echo one".to_string(), "echo two".to_string()]
        );
        assert_eq!(
            config.hooks.get("commit-msg").unwrap(),
            &vec!["echo three".to_string()]
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn bad_toml_is_a_hard_error() {
        let dir = scratch_dir("bad-toml");
        write_config(&dir, "this is not valid toml {{{");
        assert!(load_config(&dir).is_err());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn install_writes_a_fresh_executable_hook() {
        let dir = scratch_dir("fresh-install");
        let path = install_hook(&dir, "pre-commit").unwrap();
        assert_eq!(path.file_name().unwrap(), "pre-commit");
        let contents = fs::read_to_string(&path).unwrap();
        assert!(contents.starts_with("#!/bin/sh"));
        assert!(contents.contains("hookmgr run pre-commit"));
        let mode = fs::metadata(&path).unwrap().permissions().mode();
        assert!(mode & 0o111 != 0, "hook must be executable");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn install_appends_to_existing_unrelated_hook_without_destroying_it() {
        let dir = scratch_dir("append");
        let hook_path = hooks_dir(&dir).join("pre-commit");
        fs::write(&hook_path, "#!/bin/sh\necho 'run by husky'\n").unwrap();

        install_hook(&dir, "pre-commit").unwrap();
        let contents = fs::read_to_string(&hook_path).unwrap();
        assert!(
            contents.contains("run by husky"),
            "pre-existing hook content was destroyed"
        );
        assert!(contents.contains("hookmgr run pre-commit"));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn install_is_idempotent_across_repeated_runs() {
        let dir = scratch_dir("idempotent");
        install_hook(&dir, "pre-commit").unwrap();
        install_hook(&dir, "pre-commit").unwrap();
        install_hook(&dir, "pre-commit").unwrap();
        let contents = fs::read_to_string(hooks_dir(&dir).join("pre-commit")).unwrap();
        assert_eq!(contents.matches("hookmgr run pre-commit").count(), 1);
        assert_eq!(contents.matches("# >>> hookmgr pre-commit >>>").count(), 1);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn install_handles_multiple_distinct_hook_types_independently() {
        let dir = scratch_dir("multi-type");
        install_hook(&dir, "pre-commit").unwrap();
        install_hook(&dir, "commit-msg").unwrap();
        install_hook(&dir, "pre-push").unwrap();
        for name in ["pre-commit", "commit-msg", "pre-push"] {
            let contents = fs::read_to_string(hooks_dir(&dir).join(name)).unwrap();
            assert!(contents.contains(&format!("hookmgr run {name}")));
        }
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn uninstall_removes_block_and_deletes_file_when_nothing_else_remains() {
        let dir = scratch_dir("uninstall-full");
        install_hook(&dir, "pre-commit").unwrap();
        let outcome = uninstall_hook(&dir, "pre-commit").unwrap();
        assert_eq!(outcome, UninstallOutcome::FileDeleted);
        assert!(!hooks_dir(&dir).join("pre-commit").exists());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn uninstall_preserves_unrelated_content() {
        let dir = scratch_dir("uninstall-preserve");
        let hook_path = hooks_dir(&dir).join("pre-commit");
        fs::write(&hook_path, "#!/bin/sh\necho 'run by husky'\n").unwrap();
        install_hook(&dir, "pre-commit").unwrap();

        let outcome = uninstall_hook(&dir, "pre-commit").unwrap();
        assert_eq!(outcome, UninstallOutcome::BlockRemoved);
        let contents = fs::read_to_string(&hook_path).unwrap();
        assert!(contents.contains("run by husky"));
        assert!(!contents.contains("hookmgr run pre-commit"));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn uninstall_on_a_never_installed_hook_is_a_no_op() {
        let dir = scratch_dir("uninstall-noop");
        let outcome = uninstall_hook(&dir, "pre-commit").unwrap();
        assert_eq!(outcome, UninstallOutcome::NotInstalled);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn run_commands_is_a_silent_noop_with_no_config() {
        let dir = scratch_dir("run-no-config");
        let code = run_commands(&dir, "pre-commit", &[]).unwrap();
        assert_eq!(code, 0);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn run_commands_is_a_silent_noop_for_an_unconfigured_hook_type() {
        let dir = scratch_dir("run-unconfigured-type");
        write_config(&dir, "[hooks]\ncommit-msg = [\"exit 0\"]\n");
        let code = run_commands(&dir, "pre-push", &[]).unwrap();
        assert_eq!(code, 0);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn run_commands_runs_all_passing_commands_in_order() {
        let dir = scratch_dir("run-order");
        let marker_a = dir.join("a-ran");
        let marker_b = dir.join("b-ran");
        write_config(
            &dir,
            &format!(
                "[hooks]\npre-commit = [\"touch {}\", \"test -f {} && touch {}\"]\n",
                marker_a.display(),
                marker_a.display(),
                marker_b.display()
            ),
        );
        let code = run_commands(&dir, "pre-commit", &[]).unwrap();
        assert_eq!(code, 0);
        assert!(marker_a.exists());
        assert!(
            marker_b.exists(),
            "second command should see the first command's effect"
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn run_commands_stops_at_the_first_failure() {
        let dir = scratch_dir("run-stop");
        let marker = dir.join("should-not-exist");
        write_config(
            &dir,
            &format!(
                "[hooks]\npre-commit = [\"exit 1\", \"touch {}\"]\n",
                marker.display()
            ),
        );
        let code = run_commands(&dir, "pre-commit", &[]).unwrap();
        assert_eq!(code, 1);
        assert!(
            !marker.exists(),
            "later commands must not run after an earlier one fails"
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn run_commands_propagates_a_nonzero_exit_code() {
        let dir = scratch_dir("run-exit-code");
        write_config(&dir, "[hooks]\ncommit-msg = [\"exit 7\"]\n");
        let code = run_commands(&dir, "commit-msg", &[]).unwrap();
        assert_eq!(code, 7);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn run_commands_forwards_positional_args_to_the_command() {
        let dir = scratch_dir("run-args");
        // commit-msg-shaped: git passes the message-file path as $1.
        write_config(&dir, "[hooks]\ncommit-msg = [\"grep -q OK \\\"$1\\\"\"]\n");
        let msg_file = dir.join("msg.txt");
        fs::write(&msg_file, "OK this is fine\n").unwrap();
        let code = run_commands(&dir, "commit-msg", &[msg_file.display().to_string()]).unwrap();
        assert_eq!(code, 0);

        fs::write(&msg_file, "not fine\n").unwrap();
        let code = run_commands(&dir, "commit-msg", &[msg_file.display().to_string()]).unwrap();
        assert_ne!(code, 0);
        fs::remove_dir_all(&dir).ok();
    }
}
