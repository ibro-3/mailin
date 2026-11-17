# Mailin <version> (<date>)

<!--
Copy this file to the GitHub release body and fill in every placeholder.
Keep the section order stable so users can compare releases.
Delete sections that are empty, but keep "Known limitations" and "Verification".
-->

One-paragraph summary of the release and who should upgrade.

## Highlights

- <feature or change worth calling out>
- <another highlight>

## Fixes

- <bug fixed, with the user-visible symptom>
- <another fix>

## Platform notes

### Linux (Flatpak)

- Bundle: `mailin-<version>.flatpak`
- Requires the GNOME Platform/SDK runtime used by the manifest.
- Install/upgrade instructions: <paste install commands or link to the README section>.

### Windows (WinUI 3)

- Artifacts: `mailin-winui-<version>-x86_64.zip`, `mailin-<version>-x86_64.msix`,
  `mailin-setup-<version>.exe`.
- The Windows App SDK 1.8 runtime is required; the zip and installer include or
  fetch the runtime installer.
- The MSIX and installer are **unsigned**, so SmartScreen warns on first run.
  Choose "More info" → "Run anyway", or verify the checksums below first.

## Known limitations

- <for example: WinUI bulk selection uses checkboxes rather than a multi-select list>
- <for example: rules and CalDAV are not available in the UI yet>
- <for example: STARTTLS is not covered by the automated GreenMail suite>

## Verification

- `SHA256SUMS` is attached to the release. Verify with:

  ```sh
  sha256sum -c SHA256SUMS
  ```

- Build-provenance attestations are in `attestation/`. Verify with:

  ```sh
  gh attestation verify <artifact> --repo ibro-3/mailin
  ```

## Install

### Flatpak

```sh
flatpak install mailin-<version>.flatpak
flatpak run dev.mailin.Mailin
```

### Windows zip

1. Extract `mailin-winui-<version>-x86_64.zip` anywhere.
2. Run the bundled Windows App SDK runtime installer once (Administrator).
3. Run `mailin-winui.exe`.

### Windows MSIX

```powershell
Add-AppxPackage mailin-<version>-x86_64.msix
```

### Windows installer

Run `mailin-setup-<version>.exe` and follow the wizard. It installs the Start
Menu shortcut, optional desktop icon, and uninstaller, and installs the Windows
App SDK runtime if needed.

## Upgrade notes

- <schema/data migration notes, if any>
- <breaking changes, if any>
- <settings that reset or need attention>

## Thanks

Thanks to everyone who tested the release candidates and reported issues.
