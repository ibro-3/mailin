//! Repository automation: `cargo xtask <command>`.
//!
//! The commands only depend on tools that ship with Windows and Linux (`curl`,
//! `unzip`/`tar`). The WinUI projection generator uses `windows-bindgen` so the
//! committed bindings can be regenerated from the pinned metadata packages.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use anyhow::{Context, Result, bail};
use base64::Engine as _;

/// Versions of the metadata packages the projection is generated from.
///
/// These are the versions referenced by `Microsoft.WindowsAppSDK` 1.8.260921001.
const FOUNDATION_VERSION: &str = "1.8.260803002";
const WINUI_VERSION: &str = "1.8.260803003";
const INTERACTIVE_EXPERIENCES_VERSION: &str = "1.8.260708001";
const WEBVIEW2_VERSION: &str = "1.0.3179.45";
const SDK_CONTRACTS_VERSION: &str = "10.0.26100.4948";

/// Windows App SDK runtime bundle version (for self-contained zip)
const WINDOWSAPPSDK_RUNTIME_VERSION: &str = "1.8.260921001";

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
        "package-win" => package_win(),
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
           winui-runtime   download the Windows App SDK bootstrapper for local builds\n\
           package-win     build self-contained Windows zip, MSIX and Inno Setup installer"
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

/// Build the self-contained Windows zip, MSIX and Inno Setup installer.
fn package_win() -> Result<()> {
    let root = repo_root()?;
    let version = env!("CARGO_PKG_VERSION");
    let target = "x86_64-pc-windows-msvc";
    let profile = "release";

    // Build the release binary
    run_cargo(&[
        "build",
        "-p",
        "mailin-winui",
        "--target",
        target,
        "--profile",
        profile,
    ])?;

    // Stage files for self-contained zip
    let exe_path = root.join(format!("target/{target}/{profile}/mailin-winui.exe"));
    let bootstrap_dll = root.join("target/winui-runtime/Microsoft.WindowsAppRuntime.Bootstrap.dll");
    if !bootstrap_dll.exists() {
        bail!(
            "Bootstrap DLL not found at {}. Run `cargo xtask winui-runtime` first.",
            bootstrap_dll.display()
        );
    }

    let windowsappsdk_bundle = download_windowsappsdk_runtime(&root)?;

    let dist_dir = root.join("target/dist");
    fs::create_dir_all(&dist_dir).context("could not create dist directory")?;

    // 1. Self-contained zip
    let zip_staging = dist_dir.join("mailin-winui-staging");
    if zip_staging.exists() {
        fs::remove_dir_all(&zip_staging).context("could not clear staging directory")?;
    }
    fs::create_dir_all(&zip_staging).context("could not create staging directory")?;

    fs::copy(&exe_path, zip_staging.join("mailin-winui.exe")).context("could not copy exe")?;
    fs::copy(
        &bootstrap_dll,
        zip_staging.join("Microsoft.WindowsAppRuntime.Bootstrap.dll"),
    )
    .context("could not copy bootstrap dll")?;
    fs::copy(
        &windowsappsdk_bundle,
        zip_staging.join("WindowsAppRuntimeInstall-x64.exe"),
    )
    .context("could not copy Windows App SDK runtime bundle")?;

    // Write INSTALL.txt
    let install_txt = format!(
        "Mailin {version} — Self-contained Windows package\n\
        \n\
        This package contains:\n\
        - mailin-winui.exe — the application\n\
        - Microsoft.WindowsAppRuntime.Bootstrap.dll — App SDK bootstrapper\n\
        - WindowsAppRuntimeInstall-x64.exe — Windows App SDK 1.8 runtime installer\n\
        \n\
        Installation:\n\
        1. Run WindowsAppRuntimeInstall-x64.exe to install the Windows App SDK runtime\n\
           (requires Administrator). This only needs to be done once per machine.\n\
        2. Place mailin-winui.exe and Microsoft.WindowsAppRuntime.Bootstrap.dll together\n\
           in any folder (e.g., %LOCALAPPDATA%\\Mailin).\n\
        3. Run mailin-winui.exe.\n\
        \n\
        The bootstrap DLL must be next to the executable. The application will fail to\n\
        start with a framework package attachment error if the runtime is not installed.\n\
        \n\
        For a proper installation with Start Menu entry and uninstaller, use the\n\
        Inno Setup installer (mailin-setup.exe) or the MSIX package instead.\n",
        version = version
    );
    fs::write(zip_staging.join("INSTALL.txt"), install_txt)
        .context("could not write INSTALL.txt")?;

    let zip_path = dist_dir.join(format!("mailin-winui-{}-x86_64.zip", version));
    if zip_path.exists() {
        fs::remove_file(&zip_path).context("could not remove existing zip")?;
    }
    create_zip(&zip_staging, &zip_path)?;
    println!("Created self-contained zip: {}", zip_path.display());

    // 2. Unsigned MSIX
    create_msix(&root, &dist_dir, version, target, profile)?;

    // 3. Inno Setup installer
    create_inno_installer(&root, &dist_dir, version, target, profile)?;

    Ok(())
}

/// Download the Windows App SDK runtime bundle for self-contained deployment.
fn download_windowsappsdk_runtime(root: &Path) -> Result<PathBuf> {
    let work = root.join("target/winui-runtime");
    let package = work.join("runtime-bundle");
    extract_package(
        "Microsoft.WindowsAppSDK.Runtime.Bootstrap",
        WINDOWSAPPSDK_RUNTIME_VERSION,
        &work.join("packages"),
        &package,
    )?;

    // The runtime bundle is at runtimes/win-x64/native/WindowsAppRuntimeInstall-x64.exe
    let bundle = package.join("runtimes/win-x64/native/WindowsAppRuntimeInstall-x64.exe");
    if !bundle.exists() {
        bail!(
            "Windows App SDK runtime bundle not found at {}",
            bundle.display()
        );
    }

    // Copy to a stable location
    let destination = work.join("WindowsAppRuntimeInstall-x64.exe");
    fs::copy(&bundle, &destination).context("could not copy runtime bundle")?;
    Ok(destination)
}

/// Create a zip archive from a directory (uses system tar/zip).
fn create_zip(source: &Path, destination: &Path) -> Result<()> {
    let status = if cfg!(windows) {
        Command::new("tar")
            .args([
                "-a",
                "-cf",
                destination.to_str().unwrap(),
                "-C",
                source.to_str().unwrap(),
                ".",
            ])
            .status()
    } else {
        Command::new("zip")
            .args(["-r", "-q", destination.to_str().unwrap(), "."])
            .current_dir(source)
            .status()
    }
    .with_context(|| format!("failed to create zip at {}", destination.display()))?;

    if !status.success() {
        bail!("could not create zip at {}", destination.display());
    }
    Ok(())
}

/// Create the unsigned MSIX package.
fn create_msix(
    root: &Path,
    dist_dir: &Path,
    version: &str,
    target: &str,
    profile: &str,
) -> Result<()> {
    let exe_path = root.join(format!("target/{target}/{profile}/mailin-winui.exe"));
    let bootstrap_dll = root.join("target/winui-runtime/Microsoft.WindowsAppRuntime.Bootstrap.dll");

    let msix_staging = dist_dir.join("msix-staging");
    if msix_staging.exists() {
        fs::remove_dir_all(&msix_staging).context("could not clear MSIX staging")?;
    }
    fs::create_dir_all(&msix_staging).context("could not create MSIX staging")?;

    // Copy executable and bootstrap DLL
    fs::copy(&exe_path, msix_staging.join("mailin-winui.exe"))
        .context("could not copy exe to MSIX staging")?;
    fs::copy(
        &bootstrap_dll,
        msix_staging.join("Microsoft.WindowsAppRuntime.Bootstrap.dll"),
    )
    .context("could not copy bootstrap dll to MSIX staging")?;

    // Copy assets
    let assets_src = root.join("packaging/windows/assets");
    let assets_dst = msix_staging.join("Assets");
    if assets_src.exists() {
        fs::create_dir_all(&assets_dst).context("could not create Assets directory")?;
        copy_dir_all(&assets_src, &assets_dst)?;
    } else {
        // Create placeholder assets if they don't exist
        create_placeholder_assets(&assets_dst)?;
    }

    // Copy and version the AppxManifest
    let manifest_src = root.join("packaging/windows/AppxManifest.xml");
    let manifest_dst = msix_staging.join("AppxManifest.xml");
    let manifest_content =
        fs::read_to_string(&manifest_src).context("could not read AppxManifest")?;
    let manifest_content = manifest_content.replace("0.1.0.0", &format!("{}.0", version));
    fs::write(&manifest_dst, manifest_content).context("could not write versioned AppxManifest")?;

    // Run makeappx
    let msix_path = dist_dir.join(format!("mailin-{}-x86_64.msix", version));
    if msix_path.exists() {
        fs::remove_file(&msix_path).context("could not remove existing MSIX")?;
    }

    let status = Command::new("makeappx")
        .args([
            "pack",
            "/d",
            msix_staging.to_str().unwrap(),
            "/p",
            msix_path.to_str().unwrap(),
            "/l",
            "/nc",
        ])
        .status()
        .context("failed to run makeappx; is the Windows 10 SDK installed?")?;

    if !status.success() {
        bail!("makeappx failed");
    }

    println!("Created unsigned MSIX: {}", msix_path.display());
    Ok(())
}

/// Create the Inno Setup installer.
fn create_inno_installer(
    root: &Path,
    dist_dir: &Path,
    version: &str,
    target: &str,
    profile: &str,
) -> Result<()> {
    let exe_path = root.join(format!("target/{target}/{profile}/mailin-winui.exe"));
    let bootstrap_dll = root.join("target/winui-runtime/Microsoft.WindowsAppRuntime.Bootstrap.dll");
    let windowsappsdk_bundle = download_windowsappsdk_runtime(root)?;

    let iss_path = root.join("packaging/windows/setup.iss");
    if !iss_path.exists() {
        bail!("Inno Setup script not found at {}", iss_path.display());
    }

    // Create a temporary directory for the installer files
    let installer_staging = dist_dir.join("installer-staging");
    if installer_staging.exists() {
        fs::remove_dir_all(&installer_staging).context("could not clear installer staging")?;
    }
    fs::create_dir_all(&installer_staging).context("could not create installer staging")?;

    // Copy files to staging
    fs::copy(&exe_path, installer_staging.join("mailin-winui.exe"))
        .context("could not copy exe to installer staging")?;
    fs::copy(
        &bootstrap_dll,
        installer_staging.join("Microsoft.WindowsAppRuntime.Bootstrap.dll"),
    )
    .context("could not copy bootstrap dll to installer staging")?;
    fs::copy(
        &windowsappsdk_bundle,
        installer_staging.join("WindowsAppRuntimeInstall-x64.exe"),
    )
    .context("could not copy runtime bundle to installer staging")?;

    // Copy assets
    let assets_src = root.join("packaging/windows/assets");
    let assets_dst = installer_staging.join("Assets");
    if assets_src.exists() {
        fs::create_dir_all(&assets_dst)
            .context("could not create Assets directory for installer")?;
        copy_dir_all(&assets_src, &assets_dst)?;
    }

    // Run Inno Setup compiler
    let status = Command::new("iscc")
        .args([
            "/Q",
            &format!("/O{}", dist_dir.display()),
            &format!("/Fmailin-setup-{}", version),
            iss_path.to_str().unwrap(),
        ])
        .status()
        .context(
            "failed to run iscc (Inno Setup compiler); is Inno Setup installed and in PATH?",
        )?;

    if !status.success() {
        bail!("Inno Setup compilation failed");
    }

    let installer_path = dist_dir.join(format!("mailin-setup-{}.exe", version));
    println!("Created Inno Setup installer: {}", installer_path.display());
    Ok(())
}

/// Recursively copy a directory.
fn copy_dir_all(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst).context("could not create destination directory")?;
    for entry in fs::read_dir(src).context("could not read source directory")? {
        let entry = entry?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if src_path.is_dir() {
            copy_dir_all(&src_path, &dst_path)?;
        } else {
            fs::copy(&src_path, &dst_path)
                .with_context(|| format!("could not copy {}", src_path.display()))?;
        }
    }
    Ok(())
}

/// Create placeholder assets for MSIX if real ones aren't provided.
fn create_placeholder_assets(assets_dir: &Path) -> Result<()> {
    fs::create_dir_all(assets_dir).context("could not create assets directory")?;

    // Create simple 1x1 pixel PNGs as placeholders
    // These are base64-encoded 1x1 transparent PNGs
    let transparent_png = base64::engine::general_purpose::STANDARD.decode(
        "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==",
    ).unwrap();

    let sizes = [
        ("Square44x44Logo.png", 44),
        ("Square150x150Logo.png", 150),
        ("Wide310x150Logo.png", 310),
        ("SplashScreen.png", 620),
    ];
    for (name, _size) in sizes {
        let path = assets_dir.join(name);
        if !path.exists() {
            fs::write(&path, &transparent_png).context("could not write placeholder asset")?;
        }
    }
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
