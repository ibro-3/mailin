//! Repository automation: `cargo xtask <command>`.
//!
//! Commands stay dependency free on purpose so they work in CI before the
//! workspace itself has been built.

use std::path::PathBuf;
use std::process::{Command, ExitCode};

use anyhow::{Context, Result, bail};

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let Some(command) = args.next() else {
        usage();
        return Ok(());
    };
    match command.as_str() {
        "check" => check(),
        "it-up" => compose(&["up", "-d", "--wait"]),
        "it-down" => compose(&["down", "-v"]),
        "it-logs" => compose(&["logs", "--tail", "100", "greenmail"]),
        "winui-bindgen" => winui_bindgen(),
        "help" | "--help" | "-h" => {
            usage();
            Ok(())
        }
        other => bail!("unknown command `{other}` (try `cargo xtask help`)"),
    }
}

fn usage() {
    println!(
        "cargo xtask <command>\n\
         \n\
         commands:\n\
           check           run fmt, clippy and tests exactly like CI does\n\
           it-up           start the GreenMail IMAP/SMTP test server\n\
           it-down         stop the test server and drop its volumes\n\
           it-logs         show recent test server logs\n\
           winui-bindgen   regenerate the WinUI 3 projection (milestone 5)\n"
    );
}

fn repo_root() -> Result<PathBuf> {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .map(PathBuf::from)
        .context("xtask must live inside the workspace")
}

fn run_cargo(args: &[&str]) -> Result<()> {
    let status = Command::new("cargo")
        .args(args)
        .current_dir(repo_root()?)
        .status()
        .with_context(|| format!("failed to run cargo {args:?}"))?;
    if !status.success() {
        bail!("cargo {} failed", args.join(" "));
    }
    Ok(())
}

fn check() -> Result<()> {
    run_cargo(&["fmt", "--all", "--", "--check"])?;
    run_cargo(&[
        "clippy",
        "--workspace",
        "--all-targets",
        "--",
        "-D",
        "warnings",
    ])?;
    run_cargo(&["test", "--workspace"])?;
    Ok(())
}

fn compose(args: &[&str]) -> Result<()> {
    let root = repo_root()?;
    let status = Command::new("docker")
        .args(["compose", "-f", "docker-compose.it.yml"])
        .args(args)
        .current_dir(&root)
        .status()
        .context("failed to run docker compose; is Docker installed and running?")?;
    if !status.success() {
        bail!("docker compose {} failed", args.join(" "));
    }
    Ok(())
}

fn winui_bindgen() -> Result<()> {
    bail!("the WinUI 3 projection generator lands with milestone 5; see docs/architecture.md")
}
