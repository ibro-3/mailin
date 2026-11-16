# Security Policy

Mailin handles email accounts, message contents, and credentials, so security
reports are taken seriously.

## Supported versions

Security fixes land on the latest release line. At the 0.1.0 release the
supported version is:

| Version | Supported |
|---------|-----------|
| 0.1.x   | Yes       |
| < 0.1   | No        |

## Reporting a vulnerability

**Please do not open a public issue for a security problem.**

- Preferred: open a private report at
  <https://github.com/ibro-3/mailin/security/advisories/new>.
- Alternative: email the maintainers (see the address listed in the repository
  profile) with a description, reproduction steps, affected version and
  platform, and any proof-of-concept.

We aim to acknowledge a report within a few business days and to agree on a
disclosure timeline with the reporter. Please give us a reasonable window to
ship a fix before publishing details.

## Security model

### Credentials

- Passwords and OAuth2 material are stored only in the operating-system
  keyring: Secret Service on Linux, Windows Credential Manager on Windows.
- Secrets are held as `SecretString` in memory and are not written to the
  configuration file, the SQLite database, logs, or diagnostics output.
- Mailin never asks for the keyring to be exported and does not keep a copy of
  secrets on disk.

### Local data

- The SQLite cache (`mailin.db`) contains headers and bodies of cached mail.
  It is stored under the per-user application data directory and relies on the
  operating system's file permissions for at-rest protection.
- Passwords are kept out of the database; a copied database does not by itself
  grant access to the accounts.
- Before a schema migration, Mailin copies the database to a timestamped
  backup. A failed open is handled by a recovery window that can retry, open
  the data folder, or reset the local cache (renaming rather than deleting the
  unreadable files).

### HTML mail

- HTML is rendered in a separate, sandboxed engine: WebKitGTK on Linux and
  WebView2 on Windows.
- The document is served with a restrictive Content-Security-Policy.
- JavaScript, plugins, and file access are disabled.
- Remote content (images, styles, tracking pixels) is blocked by default;
  the user can allow it once or trust the sender's domain.
- Inline `cid:` images are rewritten to `data:` URLs after validation; images
  whose CID has no matching attachment are removed before rendering/sending.

### PGP

- Private keys are handled by the PGP key manager and are not written to the
  message database in plaintext.
- Encryption and signing happen in the core before a message is queued in the
  outbox.

### Supply chain

- Dependencies are pinned by `Cargo.lock`, checked by `cargo deny`, and
  covered by the advisories policy in `deny.toml`.
- Two RustCrypto advisories are currently documented and accepted in
  `deny.toml`; new advisories fail the check.
- Release artifacts carry `SHA256SUMS` and keyless build-provenance
  attestations produced by the release workflow.

## Known caveats

- **Unsigned Windows artifacts.** The MSIX package and Inno Setup installer are
  not code-signed in 0.1.0, so Windows SmartScreen warns on first run. Verify
  the published SHA-256 checksums and the attestation before installing.
- **MSIX capability.** The MSIX package uses the `runFullTrust` capability
  because the WinUI 3 client needs broad filesystem and process access; this is
  expected for a desktop mail client but should be validated by reviewers.
- **GreenMail STARTTLS.** The integration server used in CI does not support
  STARTTLS; that code path is covered by unit tests and manual testing.

## Out of scope

- Vulnerabilities in upstream dependencies that are not reachable through
  Mailin (report them upstream; we will still track them).
- Attacks that require an already-compromised local machine or keyring.
- Social-engineering a user into disabling remote-content blocking.
