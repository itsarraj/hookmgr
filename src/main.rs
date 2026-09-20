use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Result;
use clap::{Parser, Subcommand};
use hookmgr::{
    install_hook, load_config, run_commands, uninstall_hook, UninstallOutcome, CONFIG_FILENAME,
};

/// A multi-hook-type git hook manager driven by one `.hookmgr.toml`.
#[derive(Parser)]
#[command(name = "hookmgr", version, about)]
struct Cli {
    /// Repo directory (must contain a `.git`). Defaults to the current directory —
    /// which is what git itself sets as the cwd when it invokes a hook.
    #[arg(long = "dir", global = true, default_value = ".")]
    dir: PathBuf,

    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Install a dispatcher script into `.git/hooks/<name>` for each hook
    /// type named in `.hookmgr.toml` (or just the ones given).
    Install {
        /// Hook types to install (default: every key under `[hooks]`).
        hooks: Vec<String>,
    },
    /// Remove hookmgr's managed block from `.git/hooks/<name>`, leaving
    /// any unrelated content in that file untouched.
    Uninstall {
        /// Hook types to uninstall (default: every key under `[hooks]`).
        hooks: Vec<String>,
    },
    /// Run the commands configured for one hook type, in order. This is
    /// what the installed dispatcher script actually calls — not
    /// normally invoked by hand except to test a config.
    Run {
        /// The hook type to run, e.g. `pre-commit`, `commit-msg`, `pre-push`.
        hook: String,
        /// Positional args to forward to each command (e.g. the message
        /// file path git passes to `commit-msg`).
        #[arg(last = true)]
        args: Vec<String>,
    },
    /// Print what's configured for each hook type.
    List,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(code) => ExitCode::from(code),
        Err(err) => {
            eprintln!("hookmgr: {err:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<u8> {
    match cli.command {
        Cmd::Install { hooks } => {
            let config = load_config(&cli.dir)?;
            let targets = resolve_targets(&config, hooks);
            if targets.is_empty() {
                println!("hookmgr: no hooks configured in {CONFIG_FILENAME}, nothing to install");
                return Ok(0);
            }
            for hook in targets {
                let path = install_hook(&cli.dir, &hook)?;
                println!(
                    "installed {} ({} command(s) configured)",
                    path.display(),
                    config.hooks[&hook].len()
                );
            }
            Ok(0)
        }
        Cmd::Uninstall { hooks } => {
            let targets = if hooks.is_empty() {
                match load_config(&cli.dir) {
                    Ok(config) => config.hooks.keys().cloned().collect(),
                    Err(_) => vec![],
                }
            } else {
                hooks
            };
            if targets.is_empty() {
                println!("hookmgr: nothing to uninstall");
                return Ok(0);
            }
            for hook in targets {
                match uninstall_hook(&cli.dir, &hook)? {
                    UninstallOutcome::NotInstalled => println!("{hook}: not installed, skipped"),
                    UninstallOutcome::FileDeleted => {
                        println!("{hook}: removed (hook file deleted)")
                    }
                    UninstallOutcome::BlockRemoved => {
                        println!("{hook}: hookmgr block removed, other hook content preserved")
                    }
                }
            }
            Ok(0)
        }
        Cmd::Run { hook, args } => {
            let code = run_commands(&cli.dir, &hook, &args)?;
            Ok(code.clamp(0, 255) as u8)
        }
        Cmd::List => {
            let config = load_config(&cli.dir)?;
            if config.hooks.is_empty() {
                println!("hookmgr: {CONFIG_FILENAME} exists but has no [hooks] configured");
                return Ok(0);
            }
            for (hook, commands) in &config.hooks {
                println!("{hook}:");
                for command in commands {
                    println!("  - {command}");
                }
            }
            Ok(0)
        }
    }
}

fn resolve_targets(config: &hookmgr::HookConfig, requested: Vec<String>) -> Vec<String> {
    if requested.is_empty() {
        config.hooks.keys().cloned().collect()
    } else {
        requested
    }
}
