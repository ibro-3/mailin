//! Repository automation: `cargo xtask <command>`.
//!
//! The commands only depend on tools that ship with Windows and Linux (`curl`,
//! `unzip`/`tar`). The WinUI projection generator uses `windows-bindgen` so the
//! committed bindings can be regenerated from the pinned metadata packages.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use anyhow::{Context, Result, bail};

/// Versions of the metadata packages the projection is generated from.
///
/// These are the versions referenced by `Microsoft.WindowsAppSDK` 1.8.260921001.
const FOUNDATION_VERSION: &str = "1.8.260803002";
const WINUI_VERSION: &str = "1.8.260803003";
const INTERACTIVE_EXPERIENCES_VERSION: &str = "1.8.260708001";
const WEBVIEW2_VERSION: &str = "1.0.3179.45";
const SDK_CONTRACTS_VERSION: &str = "10.0.26100.4948";

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
        "winui-runtime" => winui_runtime(),
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
           winui-bindgen   regenerate the WinUI 3 projection from pinned metadata\n\
           winui-runtime   download the Windows App SDK bootstrapper for local builds\n"
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
    let root = repo_root()?;
    let work = root.join("target/winui-bindgen");
    let packages = work.join("packages");
    let extracted = work.join("extracted");
    let metadata = work.join("metadata");
    if metadata.is_dir() {
        fs::remove_dir_all(&metadata).context("could not clear the metadata directory")?;
    }
    fs::create_dir_all(&metadata).context("could not create the metadata directory")?;

    // WinUI and the Windows App SDK. The metadata is extracted from the same
    // NuGet packages that ship the runtime, so the projection and the DLLs the
    // application loads at run time come from one version.
    extract_package(
        "Microsoft.WindowsAppSDK.Foundation",
        FOUNDATION_VERSION,
        &packages,
        &extracted.join("foundation"),
    )?;
    extract_package(
        "Microsoft.WindowsAppSDK.WinUI",
        WINUI_VERSION,
        &packages,
        &extracted.join("winui"),
    )?;
    extract_package(
        "Microsoft.WindowsAppSDK.InteractiveExperiences",
        INTERACTIVE_EXPERIENCES_VERSION,
        &packages,
        &extracted.join("interactive-experiences"),
    )?;
    extract_package(
        "Microsoft.Web.WebView2",
        WEBVIEW2_VERSION,
        &packages,
        &extracted.join("webview2"),
    )?;
    extract_package(
        "Microsoft.Windows.SDK.Contracts",
        SDK_CONTRACTS_VERSION,
        &packages,
        &extracted.join("contracts"),
    )?;

    let mut copied = 0;
    copied += copy_winmds(&extracted.join("foundation/metadata"), &metadata)?;
    copied += copy_winmds(&extracted.join("winui/metadata"), &metadata)?;
    copied += copy_file(
        &extracted.join("interactive-experiences/metadata/10.0.17763.0/Microsoft.UI.winmd"),
        &metadata,
    )?;
    copied += copy_winmds(&extracted.join("webview2/lib"), &metadata)?;
    copied += copy_winmds(&extracted.join("contracts/ref/netstandard2.0"), &metadata)?;

    let filter = root.join("crates/mailin-winui-sys/filter.txt");
    let output = root.join("crates/mailin-winui-sys/src/bindings.rs");
    println!(
        "generating {} from {copied} metadata files",
        output.display()
    );
    windows_bindgen::bindgen([
        "--in",
        metadata.to_str().context("metadata path is not UTF-8")?,
        "--out",
        output.to_str().context("output path is not UTF-8")?,
        "--filter-file",
        filter.to_str().context("filter path is not UTF-8")?,
        "--implement",
        "Microsoft.UI.Xaml.IApplicationOverrides",
        "--implement",
        "Microsoft.UI.Xaml.Input.ICommand",
        "--compose",
        "Microsoft.UI.Xaml.Application",
        "--minimal",
        "--flat",
    ]);
    Ok(())
}

fn winui_runtime() -> Result<()> {
    let root = repo_root()?;
    let work = root.join("target/winui-runtime");
    let package = work.join("foundation");
    extract_package(
        "Microsoft.WindowsAppSDK.Foundation",
        FOUNDATION_VERSION,
        &work.join("packages"),
        &package,
    )?;

    let source = package.join("runtimes/win-x64/native/Microsoft.WindowsAppRuntime.Bootstrap.dll");
    let destination = work.join("Microsoft.WindowsAppRuntime.Bootstrap.dll");
    fs::copy(&source, &destination).with_context(|| {
        format!(
            "could not copy {} to {}",
            source.display(),
            destination.display()
        )
    })?;

    println!("bootstrapper: {}", destination.display());
    println!(
        "set MAILIN_WINDOWSAPPSDK_BOOTSTRAP={} before building mailin-winui",
        destination.display()
    );
    println!(
        "the matching runtime can be installed from \
         https://aka.ms/windowsappsdk/1.8/latest/windowsappruntimeinstall-x64.exe"
    );
    Ok(())
}

/// Download a pinned NuGet package and extract it below `destination`.
fn extract_package(id: &str, version: &str, packages: &Path, destination: &Path) -> Result<()> {
    let lower = id.to_ascii_lowercase();
    let archive = packages.join(format!("{lower}.{version}.nupkg"));
    let url =
        format!("https://api.nuget.org/v3-flatcontainer/{lower}/{version}/{lower}.{version}.nupkg");
    download(&url, &archive)?;
    if destination.is_dir() {
        fs::remove_dir_all(destination)
            .with_context(|| format!("could not clear {}", destination.display()))?;
    }
    extract(&archive, destination)
}

fn download(url: &str, destination: &Path) -> Result<()> {
    if destination.is_file() {
        return Ok(());
    }
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("could not create {}", parent.display()))?;
    }
    let status = Command::new("curl")
        .args(["-L", "--fail", "--silent", "--show-error", "-o"])
        .arg(destination)
        .arg(url)
        .status()
        .context("failed to run curl; install curl or download the package manually")?;
    if !status.success() {
        bail!("could not download {url}");
    }
    Ok(())
}

fn extract(archive: &Path, destination: &Path) -> Result<()> {
    fs::create_dir_all(destination)
        .with_context(|| format!("could not create {}", destination.display()))?;
    // Windows ships bsdtar, which understands zip; Linux has unzip.
    let status = if cfg!(windows) {
        Command::new("tar")
            .arg("-xf")
            .arg(archive)
            .arg("-C")
            .arg(destination)
            .status()
    } else {
        Command::new("unzip")
            .args(["-o", "-q"])
            .arg(archive)
            .arg("-d")
            .arg(destination)
            .status()
    }
    .with_context(|| format!("failed to extract {}", archive.display()))?;
    if !status.success() {
        bail!("could not extract {}", archive.display());
    }
    Ok(())
}

/// Copy one metadata file into the flat `destination` directory.
fn copy_file(source: &Path, destination: &Path) -> Result<usize> {
    let name = source
        .file_name()
        .with_context(|| format!("{} has no file name", source.display()))?;
    fs::copy(source, destination.join(name))
        .with_context(|| format!("could not copy {}", source.display()))?;
    Ok(1)
}

/// Copy every `.winmd` below `source` into the flat `destination` directory.
fn copy_winmds(source: &Path, destination: &Path) -> Result<usize> {
    if !source.is_dir() {
        return Ok(0);
    }
    let mut copied = 0;
    for entry in
        fs::read_dir(source).with_context(|| format!("could not read {}", source.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            copied += copy_winmds(&path, destination)?;
        } else if path
            .extension()
            .is_some_and(|extension| extension == "winmd")
        {
            let name = path.file_name().context("a metadata file without a name")?;
            fs::copy(&path, destination.join(name))
                .with_context(|| format!("could not copy {}", path.display()))?;
            copied += 1;
        }
    }
    Ok(copied)
}
