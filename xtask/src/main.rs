//! Repository automation: `cargo xtask <command>`.
//!
//! The commands only depend on tools that ship with Windows and Linux (`curl`,
//! `unzip`/`tar`). The WinUI projection generator uses `windows-bindgen` so the
//! committed bindings can be regenerated from the pinned metadata packages.

use std::collections::{HashMap, HashSet};
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
        "it-up" => compose_file(&["up", "-d", "--wait"], "docker-compose.it.yml"),
        "it-down" => compose_file(&["down", "-v"], "docker-compose.it.yml"),
        "it-logs" => compose_file(
            &["logs", "--tail", "100", "greenmail"],
            "docker-compose.it.yml",
        ),
        "it-up-tls" => {
            it_certs()?;
            compose_file(&["up", "-d", "--wait"], "docker-compose.it-tls.yml")
        }
        "it-down-tls" => compose_file(&["down", "-v"], "docker-compose.it-tls.yml"),
        "it-certs" => it_certs(),
        "release-version" => release_version(&mut args),
        "check-versions" => check_versions(),
        "release-notes" => release_notes(&mut args),
        "winui-deref" => winui_deref(),
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
           it-up-tls       generate local test CA and start TLS-enabled GreenMail\n\
           it-down-tls     stop the TLS test server and drop its volumes\n\
           it-certs        generate the test CA and GreenMail PKCS12 keystore\n\
           release-version  set the version in Cargo.toml and both packaging formats\n\
           check-versions   verify Cargo.toml, metainfo and Windows packaging agree\n\
           release-notes   render docs/release-notes-template.md for a release tag\n\
           winui-deref     generate interface Deref chains for the projection\n\
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
    check_versions()?;
    run_cargo(&["fmt", "--all", "--", "--check"])?;
    run_cargo(&[
        "clippy",
        "--workspace",
        "--all-targets",
        "--",
        "-D",
        "warnings",
    ])?;
    run_cargo(&["test", "--workspace", "--all-targets"])?;
    Ok(())
}

/// Paths whose version fields must agree with the workspace `Cargo.toml`.
const METAINFO_PATH: &str = "packaging/flatpak/dev.mailin.Mailin.metainfo.xml";
const APPX_PATH: &str = "packaging/windows/AppxManifest.xml";
const INNO_PATH: &str = "packaging/windows/setup.iss";

/// Read the `version` from `[workspace.package]`.
fn workspace_version(text: &str) -> Result<String> {
    let section = text
        .find("[workspace.package]")
        .context("Cargo.toml has no [workspace.package] section")?;
    let rest = &text[section..];
    let marker = "version = \"";
    let start = rest
        .find(marker)
        .map(|index| section + index + marker.len())
        .context("[workspace.package] has no version")?;
    let end = text[start..]
        .find('"')
        .context("workspace version is not terminated")?;
    Ok(text[start..start + end].to_owned())
}

/// Find the first `attr="value"` whose name starts a XML attribute (i.e. is not
/// the tail of a longer name such as `MinVersion`).
fn xml_attr_value(text: &str, attr: &str) -> Result<String> {
    let needle = format!("{attr}=\"");
    let mut from = 0;
    while let Some(relative) = text[from..].find(&needle) {
        let index = from + relative;
        let starts_attribute = text[..index]
            .chars()
            .last()
            .is_none_or(|character| character.is_whitespace());
        if starts_attribute {
            let start = index + needle.len();
            let end = text[start..]
                .find('"')
                .context("XML attribute is not terminated")?;
            return Ok(text[start..start + end].to_owned());
        }
        from = index + needle.len();
    }
    bail!("could not find the {attr} attribute")
}

fn metainfo_version(text: &str) -> Result<String> {
    let marker = "release version=\"";
    let start = text.find(marker).context("metainfo has no <release>")? + marker.len();
    let end = text[start..]
        .find('"')
        .context("metainfo release version is not terminated")?;
    Ok(text[start..start + end].to_owned())
}

fn inno_version(text: &str) -> Result<String> {
    let marker = "#define AppVersion \"";
    let start = text
        .find(marker)
        .context("setup.iss has no AppVersion define")?
        + marker.len();
    let end = text[start..]
        .find('"')
        .context("setup.iss AppVersion is not terminated")?;
    Ok(text[start..start + end].to_owned())
}

/// Replace exactly one occurrence of `from`, failing when it is absent.
fn replace_once(text: &str, from: &str, to: &str) -> Result<String> {
    let index = text
        .find(from)
        .with_context(|| format!("expected to find {from:?}"))?;
    let mut out = String::with_capacity(text.len() + to.len());
    out.push_str(&text[..index]);
    out.push_str(to);
    out.push_str(&text[index + from.len()..]);
    Ok(out)
}

/// Replace the version inside `[workspace.package]` only, leaving dependency
/// version requirements untouched.
fn replace_workspace_version(text: &str, current: &str, new: &str) -> Result<String> {
    let section = text
        .find("[workspace.package]")
        .context("Cargo.toml has no [workspace.package] section")?;
    let marker = format!("version = \"{current}\"");
    let relative = text[section..]
        .find(&marker)
        .with_context(|| format!("could not find {marker:?} in [workspace.package]"))?;
    let index = section + relative;
    let mut out = String::with_capacity(text.len() + new.len());
    out.push_str(&text[..index]);
    out.push_str(&format!("version = \"{new}\""));
    out.push_str(&text[index + marker.len()..]);
    Ok(out)
}

fn validate_version(version: &str) -> Result<()> {
    let parts: Vec<&str> = version.split('.').collect();
    let valid = parts.len() == 3
        && parts
            .iter()
            .all(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_digit()));
    if !valid {
        bail!("version must look like MAJOR.MINOR.PATCH (got {version:?})");
    }
    Ok(())
}

fn validate_date(date: &str) -> Result<()> {
    let bytes = date.as_bytes();
    let valid_shape = bytes.len() == 10
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes
            .iter()
            .enumerate()
            .all(|(index, byte)| matches!(index, 4 | 7) || byte.is_ascii_digit());
    if !valid_shape {
        bail!("date must be YYYY-MM-DD (got {date:?})");
    }
    let month: u32 = date[5..7].parse().context("invalid month")?;
    let day: u32 = date[8..10].parse().context("invalid day")?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        bail!("date must be a valid month/day (got {date:?})");
    }
    Ok(())
}

/// Set the version everywhere it is stored, then verify the files agree.
fn release_version(args: &mut impl Iterator<Item = String>) -> Result<()> {
    let version = args
        .next()
        .context("usage: cargo xtask release-version <version> [YYYY-MM-DD]")?;
    validate_version(&version)?;
    let date = args.next();
    if let Some(date) = &date {
        validate_date(date)?;
    }

    let root = repo_root()?;

    let cargo_path = root.join("Cargo.toml");
    let cargo_text = fs::read_to_string(&cargo_path).context("could not read Cargo.toml")?;
    let current = workspace_version(&cargo_text)?;
    if current != version {
        let updated = replace_workspace_version(&cargo_text, &current, &version)?;
        fs::write(&cargo_path, updated).context("could not write Cargo.toml")?;
        println!("Cargo.toml: {current} -> {version}");
    }

    let appx_path = root.join(APPX_PATH);
    let appx_text = fs::read_to_string(&appx_path).context("could not read AppxManifest.xml")?;
    let appx_current = xml_attr_value(&appx_text, "Version")?;
    let updated = replace_once(
        &appx_text,
        &format!("Version=\"{appx_current}\""),
        &format!("Version=\"{version}.0\""),
    )?;
    fs::write(&appx_path, updated).context("could not write AppxManifest.xml")?;
    println!("AppxManifest.xml: {appx_current} -> {version}.0");

    let inno_path = root.join(INNO_PATH);
    let inno_text = fs::read_to_string(&inno_path).context("could not read setup.iss")?;
    let inno_current = inno_version(&inno_text)?;
    let updated = replace_once(
        &inno_text,
        &format!("#define AppVersion \"{inno_current}\""),
        &format!("#define AppVersion \"{version}\""),
    )?;
    fs::write(&inno_path, updated).context("could not write setup.iss")?;
    println!("setup.iss: AppVersion {inno_current} -> {version}");

    let metainfo_path = root.join(METAINFO_PATH);
    let metainfo_text =
        fs::read_to_string(&metainfo_path).context("could not read the metainfo file")?;
    let metainfo_current = metainfo_version(&metainfo_text)?;
    let mut updated = replace_once(
        &metainfo_text,
        &format!("release version=\"{metainfo_current}\""),
        &format!("release version=\"{version}\""),
    )?;
    if let Some(date) = date {
        updated = replace_release_date(&updated, &version, &date)?;
        println!("metainfo: release date set to {date}");
    }
    fs::write(&metainfo_path, updated).context("could not write the metainfo file")?;
    println!("metainfo: release version {metainfo_current} -> {version}");

    check_versions()
}

/// Replace the `date` on the `<release>` entry for `version`.
fn replace_release_date(text: &str, version: &str, date: &str) -> Result<String> {
    let marker = format!("release version=\"{version}\"");
    let start = text
        .find(&marker)
        .with_context(|| format!("could not find {marker:?}"))?
        + marker.len();
    let date_marker = "date=\"";
    let date_start = text[start..]
        .find(date_marker)
        .map(|index| start + index + date_marker.len())
        .context("the release entry has no date")?;
    let date_end = text[date_start..]
        .find('"')
        .map(|index| date_start + index)
        .context("the release date is not terminated")?;
    let mut out = String::with_capacity(text.len());
    out.push_str(&text[..date_start]);
    out.push_str(date);
    out.push_str(&text[date_end..]);
    Ok(out)
}

/// Verify that the workspace and both packaging formats carry one version.
fn check_versions() -> Result<()> {
    let root = repo_root()?;
    let version = workspace_version(
        &fs::read_to_string(root.join("Cargo.toml")).context("could not read Cargo.toml")?,
    )?;
    let metainfo = metainfo_version(
        &fs::read_to_string(root.join(METAINFO_PATH))
            .context("could not read the metainfo file")?,
    )?;
    let appx = xml_attr_value(
        &fs::read_to_string(root.join(APPX_PATH)).context("could not read AppxManifest.xml")?,
        "Version",
    )?;
    let inno = inno_version(
        &fs::read_to_string(root.join(INNO_PATH)).context("could not read setup.iss")?,
    )?;

    let mut problems = Vec::new();
    if metainfo != version {
        problems.push(format!(
            "metainfo release version is {metainfo}, expected {version}"
        ));
    }
    let expected_appx = format!("{version}.0");
    if appx != expected_appx {
        problems.push(format!(
            "AppxManifest Version is {appx}, expected {expected_appx}"
        ));
    }
    if inno != version {
        problems.push(format!(
            "setup.iss AppVersion is {inno}, expected {version}"
        ));
    }
    if !problems.is_empty() {
        bail!("version mismatch:\n- {}", problems.join("\n- "));
    }
    println!("versions agree: {version}");
    Ok(())
}

fn compose_file(args: &[&str], file: &str) -> Result<()> {
    let root = repo_root()?;
    let status = Command::new("docker")
        .args(["compose", "-f", file])
        .args(args)
        .current_dir(&root)
        .status()
        .context("failed to run docker compose; is Docker installed and running?")?;
    if !status.success() {
        bail!("docker compose -f {} {} failed", file, args.join(" "));
    }
    Ok(())
}

/// Generate a local test CA, a localhost server certificate and the PKCS12
/// keystore GreenMail serves TLS with. Certificates land in `target/it-certs`
/// (gitignored) and are only regenerated when missing or forced.
fn it_certs() -> Result<()> {
    let root = repo_root()?;
    let certs = root.join("target/it-certs");
    fs::create_dir_all(&certs).with_context(|| format!("could not create {}", certs.display()))?;

    let ca_crt = certs.join("ca.crt");
    let p12 = certs.join("greenmail.p12");
    if ca_crt.is_file() && p12.is_file() {
        println!("test certificates already present in {}", certs.display());
        return Ok(());
    }

    let openssl = if cfg!(windows) {
        "openssl.exe"
    } else {
        "openssl"
    };
    let sh = |args: &[&str]| -> Result<()> {
        let status = Command::new(openssl)
            .args(args)
            .current_dir(&certs)
            .status()
            .with_context(|| "failed to run openssl; it is required to generate the test CA")?;
        if !status.success() {
            bail!("openssl {:?} failed", args);
        }
        Ok(())
    };

    sh(&[
        "req",
        "-x509",
        "-newkey",
        "rsa:2048",
        "-sha256",
        "-nodes",
        "-days",
        "3650",
        "-subj",
        "/CN=Mailin Test CA",
        "-addext",
        "basicConstraints=critical,CA:TRUE",
        "-keyout",
        "ca.key",
        "-out",
        "ca.crt",
    ])?;
    sh(&[
        "req",
        "-newkey",
        "rsa:2048",
        "-nodes",
        "-subj",
        "/CN=localhost",
        "-addext",
        "subjectAltName=DNS:localhost,IP:127.0.0.1",
        "-keyout",
        "server.key",
        "-out",
        "server.csr",
    ])?;
    let extfile = certs.join("server.ext");
    fs::write(
        &extfile,
        "subjectAltName=DNS:localhost,IP:127.0.0.1\nbasicConstraints=critical,CA:FALSE\n",
    )?;
    sh(&[
        "x509",
        "-req",
        "-in",
        "server.csr",
        "-CA",
        "ca.crt",
        "-CAkey",
        "ca.key",
        "-CAcreateserial",
        "-days",
        "3650",
        "-sha256",
        "-extfile",
        "server.ext",
        "-out",
        "server.crt",
    ])?;
    sh(&[
        "pkcs12",
        "-export",
        "-out",
        "greenmail.p12",
        "-inkey",
        "server.key",
        "-in",
        "server.crt",
        "-certfile",
        "ca.crt",
        "-name",
        "greenmail",
        "-passout",
        "pass:changeit",
    ])?;
    // `openssl pkcs12 -export` creates the keystore with mode 0600. The
    // keystore is mounted read-only into the GreenMail container, which
    // runs as a different user, so the file has to be readable for
    // everyone or the TLS servers never come up. The password is a
    // throwaway test password.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&p12, fs::Permissions::from_mode(0o644))?;
    }
    println!("test certificates generated in {}", certs.display());
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

    // Patch the generated bindings.rs directly
    let bindings_path = root.join("crates/mailin-winui-sys/src/bindings.rs");

    // 1. Fix PropertyPath::CreateInstance to use HSTRING::from
    let content = fs::read_to_string(&bindings_path)?;
    let content = content.replace(
        "core::mem::transmute_copy(&path.into())",
        "core::mem::transmute_copy(&windows_core::HSTRING::from(path))",
    );
    fs::write(&bindings_path, content)?;

    // Also fix the transmute_copy with type annotation
    let content = fs::read_to_string(&bindings_path)?;
    let content = content.replace(
        "core::mem::transmute_copy::<Src, *mut c_void>(&path.into())",
        "core::mem::transmute_copy::<Src, *mut c_void>(&windows_core::HSTRING::from(path))",
    );
    fs::write(&bindings_path, content)?;

    // The projection has no interface-to-interface `Deref`, so regenerate the
    // chains from the same metadata.
    winui_deref()?;

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

/// Generate the `Deref` chain for projected interfaces from the pinned
/// metadata.
///
/// The `--minimal` bindings emit a `Deref` from a class to its primary
/// interface, but never from one interface to its parent. The WinUI front-end
/// calls parent-interface members (`SetHeight`, `SetContent`, `ShowAsync`, ...)
/// straight on control values, which needs those chains.
///
/// The interfaces are read from `bindings.rs` (every `define_interface!`
/// entry) and the parent relations come from the `InterfaceImpl` rows of the
/// pinned metadata. The result is written to
/// `crates/mailin-winui-sys/src/deref.rs` and included by that crate.
///
/// Run `cargo xtask winui-bindgen && cargo xtask winui-deref` after changing
/// the projection; `winui-bindgen` calls this command itself.
fn winui_deref() -> Result<()> {
    use windows_metadata::reader::{File, Index, Item, TypeCategory};
    use windows_metadata::{Type, TypeName};

    let root = repo_root()?;
    let metadata = root.join("target/winui-bindgen/metadata");
    if !metadata.is_dir() {
        bail!(
            "no metadata in {}: run `cargo xtask winui-bindgen` first",
            metadata.display()
        );
    }

    let mut files = Vec::new();
    for entry in fs::read_dir(&metadata).context("could not read the metadata directory")? {
        let entry = entry?;
        let path = entry.path();
        if path
            .extension()
            .is_some_and(|extension| extension == "winmd")
            && let Some(file) = File::read(&path)
        {
            files.push(file);
        }
    }
    if files.is_empty() {
        bail!("no .winmd files in {}", metadata.display());
    }
    let index = Index::new(files).leak();

    // Parent interfaces of every interface in the metadata.
    let mut parents: HashMap<(String, String), Vec<(String, String)>> = HashMap::new();
    for (namespace, name, item) in index.iter_items() {
        let Item::Type(def) = item else { continue };
        if def.category() != TypeCategory::Interface {
            continue;
        }
        // Generic interfaces reference their own parameters in the parent
        // signature, so the type arguments have to be supplied.
        let generic_args: Vec<Type> = def
            .generic_params()
            .map(|parameter| Type::Generic(parameter.name().to_string(), parameter.sequence()))
            .collect();
        let implemented = def
            .interface_impls()
            .filter_map(|interface| match interface.interface(&generic_args) {
                Type::ClassName(TypeName {
                    namespace,
                    name,
                    generics,
                }) if generics.is_empty() => Some((namespace, name)),
                _ => None,
            })
            .filter(|(namespace, name)| {
                !(*namespace == "Windows" && (*name == "Foundation" || *name == "Win32"))
                    && *name != "IUnknown"
                    && *name != "IInspectable"
            })
            .collect();
        parents.insert((namespace.to_string(), name.to_string()), implemented);
    }

    // Interfaces that the projection actually emits.
    let bindings = fs::read_to_string(root.join("crates/mailin-winui-sys/src/bindings.rs"))?;
    let projected = projected_interfaces(&bindings);

    // Map every projected (flat) name back to its metadata entry. The flat
    // projection cannot express two types with the same name from different
    // namespaces, so an ambiguous name is dropped rather than guessed.
    let mut by_name: HashMap<String, Vec<(String, String)>> = HashMap::new();
    for key in parents.keys() {
        by_name.entry(key.1.clone()).or_default().push(key.clone());
    }
    let mut full_of: HashMap<String, (String, String)> = HashMap::new();
    for name in &projected {
        let candidates = by_name.get(name).map(Vec::as_slice).unwrap_or_default();
        if let [only] = candidates {
            full_of.insert(name.clone(), only.clone());
        }
    }

    // Walk the parent chain of every projected interface and emit a single
    // `Deref` per interface: the edge to its first projected parent. An
    // interface can list several parents in the metadata, but `Deref` has
    // one target, so the remaining ancestors are reached through the chain.
    // The edge is keyed by the interface at the current level — keying it by
    // the type the walk started at would give one type conflicting impls for
    // every ancestor it reaches.
    let mut edges: Vec<(String, String)> = Vec::new();
    let mut emitted: HashSet<(String, String)> = HashSet::new();
    for name in &projected {
        let mut current = match full_of.get(name) {
            Some(full) => full.clone(),
            None => continue,
        };
        let mut visited: HashSet<(String, String)> = HashSet::new();
        while visited.insert(current.clone()) {
            let parents = match parents.get(&current) {
                Some(parents) => parents,
                None => break,
            };
            // WinUI interfaces form a single chain, but guard against a
            // metadata cycle by following the first projected parent only.
            let Some(next) = parents
                .iter()
                .find(|parent| projected.contains(&parent.1) && full_of.contains_key(&parent.1))
            else {
                break;
            };
            if emitted.insert((current.1.clone(), next.1.clone())) {
                edges.push((current.1.clone(), next.1.clone()));
            }
            current = next.clone();
        }
    }
    edges.sort();

    let out = root.join("crates/mailin-winui-sys/src/deref.rs");
    let mut text = String::from(
        "//! `Deref` chains between projected interfaces.\n\
         //!\n\
         //! Generated by `cargo xtask winui-deref` from the pinned WinUI\n\
         //! metadata; see [`winui_ext`] for the hand-written helpers.\n\
         //!\n\
         //! The `Deref` target is the parent interface, so a value of a\n\
         //! derived interface exposes the members of every ancestor.\n\
         //!\n\
         //! The interfaces come from `bindings.rs`, so they are in scope\n\
         //! through the crate root.\n\
         use super::*;\n\n",
    );
    for (from, to) in &edges {
        text.push_str(&format!(
            "impl core::ops::Deref for {from} {{\n\
             \x20   type Target = {to};\n\
             \x20   fn deref(&self) -> &Self::Target {{\n\
             \x20       unsafe {{ core::mem::transmute(self) }}\n\
             \x20   }}\n\
             }}\n\n"
        ));
    }
    fs::write(&out, text)?;
    println!(
        "wrote {} interface Deref impls to {}",
        edges.len(),
        out.strip_prefix(root).unwrap().display()
    );
    Ok(())
}

/// Names of every interface the projection emits, read from the
/// `define_interface!` invocations in `bindings.rs`.
fn projected_interfaces(bindings: &str) -> HashSet<String> {
    let mut names = HashSet::new();
    let mut lines = bindings.lines();
    while let Some(line) = lines.next() {
        if !line.contains("define_interface!(") {
            continue;
        }
        // The interface name is the first argument on the next line.
        if let Some(name) = lines.next() {
            let name = name.trim().trim_end_matches(',');
            if !name.is_empty()
                && name
                    .chars()
                    .all(|character| character.is_alphanumeric() || character == '_')
            {
                names.insert(name.to_string());
            }
        }
    }
    names
}

/// Collect the `- ` bullets of one `###` section in a version's CHANGELOG entry.
fn changelog_bullets(changelog: &str, version: &str, heading: &str) -> Vec<String> {
    let release_marker = format!("## [{version}]");
    let Some(start) = changelog.find(&release_marker) else {
        return Vec::new();
    };
    let rest = &changelog[start..];
    let end = rest
        .find("\n## ")
        .map(|index| start + index)
        .unwrap_or(changelog.len());
    let section = &changelog[start..end];
    let heading_marker = format!("### {heading}");
    let Some(heading_start) = section.find(&heading_marker) else {
        return Vec::new();
    };
    let after = &section[heading_start + heading_marker.len()..];
    let section_end = after.find("\n### ").unwrap_or(after.len());
    let mut bullets: Vec<String> = Vec::new();
    let mut current: Option<String> = None;
    for line in after[..section_end].lines() {
        if line.starts_with("- ") {
            if let Some(previous) = current.take() {
                bullets.push(previous);
            }
            current = Some(line.to_owned());
        } else if let Some(item) = current.as_mut() {
            if line.trim().is_empty() {
                continue;
            }
            if line.starts_with("  ") {
                item.push('\n');
                item.push_str(line);
            } else {
                bullets.push(item.clone());
                current = None;
            }
        }
    }
    if let Some(item) = current {
        bullets.push(item);
    }
    bullets
}

/// Render the release-notes template for a tag. The result is printed to
/// stdout so the release workflow can use it as `body_path` after redirecting.
fn release_notes(args: &mut impl Iterator<Item = String>) -> Result<()> {
    let version = args
        .next()
        .context("usage: cargo xtask release-notes <version> [YYYY-MM-DD]")?;
    let date = args.next().unwrap_or_else(|| "TBD".to_owned());
    let root = repo_root()?;
    let template = fs::read_to_string(root.join("docs/release-notes-template.md"))
        .context("could not read docs/release-notes-template.md")?;
    let changelog =
        fs::read_to_string(root.join("CHANGELOG.md")).context("could not read CHANGELOG.md")?;
    print!(
        "{}",
        render_release_notes(&template, &changelog, &version, &date)
    );
    Ok(())
}

/// Drop the editor instructions and placeholder bullets from the template, then
/// fill in the version, date and artifact name. Keeping this pure makes it
/// testable without a release tag.
fn render_release_notes(template: &str, changelog: &str, version: &str, date: &str) -> String {
    let mut out = String::with_capacity(template.len());
    let mut in_comment = false;
    for line in template.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("<!--") {
            in_comment = true;
        }
        if in_comment {
            if trimmed.ends_with("-->") {
                in_comment = false;
            }
            continue;
        }
        if trimmed.starts_with("- <")
            || trimmed.starts_with("One-paragraph summary")
            || trimmed.is_empty() && out.ends_with("\n\n")
        {
            continue;
        }
        let rendered = line
            .replace("<version>", version)
            .replace("<date>", date)
            .replace("<artifact>", "mailin-*.flatpak");
        out.push_str(&rendered);
        out.push('\n');
        // Fill the three summary sections from the CHANGELOG entry so the
        // published notes cannot drift away from the maintained history.
        for (heading, source) in [
            ("## Highlights", "Added"),
            ("## Fixes", "Fixed"),
            ("## Known limitations", "Known limitations"),
        ] {
            if line == heading {
                for bullet in changelog_bullets(changelog, version, source) {
                    out.push_str(&bullet);
                    out.push('\n');
                }
            }
        }
    }
    out.trim_end().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_are_validated() {
        assert!(validate_version("0.1.0").is_ok());
        assert!(validate_version("1.2.3").is_ok());
        for bad in ["0.1", "1.2.3.4", "v1.2.3", "1..3", ""] {
            assert!(validate_version(bad).is_err(), "{bad} should be rejected");
        }
    }

    #[test]
    fn dates_are_validated() {
        assert!(validate_date("2026-12-18").is_ok());
        for bad in ["2026-1-1", "2026/12/18", "20261218", "2026-13-01"] {
            assert!(validate_date(bad).is_err(), "{bad} should be rejected");
        }
    }

    #[test]
    fn xml_attr_ignores_longer_names() {
        let xml = r#"<Identity MinVersion="10.0.17763.0" Version="1.2.3.0" />"#;
        assert_eq!(xml_attr_value(xml, "Version").unwrap(), "1.2.3.0");
    }

    #[test]
    fn packaging_versions_are_read() {
        let metainfo =
            r#"<releases><release version="4.5.6" date="2026-01-02"></release></releases>"#;
        assert_eq!(metainfo_version(metainfo).unwrap(), "4.5.6");
        assert_eq!(
            inno_version("#define AppVersion \"4.5.6\"\n").unwrap(),
            "4.5.6"
        );
        let cargo = "[workspace.package]\nversion = \"4.5.6\"\n[workspace.dependencies]\nsome = { version = \"4.5.6\" }";
        assert_eq!(workspace_version(cargo).unwrap(), "4.5.6");
    }

    #[test]
    fn workspace_bump_leaves_dependencies_alone() {
        let cargo = "[workspace.package]\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[workspace.dependencies]\nfoo = { version = \"0.1.0\" }\n";
        let updated = replace_workspace_version(cargo, "0.1.0", "0.2.0").unwrap();
        assert!(updated.contains("[workspace.package]\nversion = \"0.2.0\""));
        assert!(updated.contains("foo = { version = \"0.1.0\" }"));
    }

    #[test]
    fn release_date_is_replaced_for_the_right_version() {
        let text = r#"<release version="0.2.0" date="2026-01-01"><description/></release>"#;
        let updated = replace_release_date(text, "0.2.0", "2026-12-18").unwrap();
        assert!(updated.contains("date=\"2026-12-18\""));
    }

    #[test]
    fn release_notes_drop_instructions_and_fill_placeholders() {
        let template = "<!--\nCopy this file\n-->\n# Mailin <version> (<date>)\n\nOne-paragraph summary.\n\n- <feature>\n- Real highlight\n\n```sh\ngh attestation verify <artifact>\n```\n";
        let notes = render_release_notes(template, "", "0.1.0", "2026-12-18");
        assert!(notes.starts_with("# Mailin 0.1.0 (2026-12-18)"));
        assert!(!notes.contains("Copy this file"));
        assert!(!notes.contains("One-paragraph summary"));
        assert!(!notes.contains("<feature>"));
        assert!(notes.contains("Real highlight"));
        assert!(notes.contains("gh attestation verify mailin-*.flatpak"));
    }
}
