//! Repo automation. Run with `cargo xtask <command>`.

use std::process::{Command, ExitCode};

const HELP: &str = "\
Usage: cargo xtask <command>

Commands:
  fmt     Format all code
  lint    Check formatting and run clippy (warnings are errors)
  test    Run all workspace tests
  ci      Run lint and test, as CI does
";

fn main() -> ExitCode {
    let task = std::env::args().nth(1);
    let result = match task.as_deref() {
        Some("fmt") => cargo(&["fmt", "--all"]),
        Some("lint") => lint(),
        Some("test") => cargo(&["test", "--workspace"]),
        Some("ci") => lint().and_then(|()| cargo(&["test", "--workspace"])),
        _ => {
            eprint!("{HELP}");
            return ExitCode::from(2);
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(msg) => {
            eprintln!("xtask: {msg}");
            ExitCode::FAILURE
        }
    }
}

fn lint() -> Result<(), String> {
    cargo(&["fmt", "--all", "--check"])?;
    cargo(&[
        "clippy",
        "--workspace",
        "--all-targets",
        "--",
        "-D",
        "warnings",
    ])
}

fn cargo(args: &[&str]) -> Result<(), String> {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let status = Command::new(cargo)
        .args(args)
        .status()
        .map_err(|e| format!("failed to run cargo: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("`cargo {}` failed", args.join(" ")))
    }
}
