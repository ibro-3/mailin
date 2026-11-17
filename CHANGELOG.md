# Changelog

All notable changes to Mailin are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project uses
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.0] - 2026-11-17

The first public release: a native IMAP/SMTP desktop client for Linux (GTK 4 +
libadwaita) and Windows (WinUI 3), sharing one Rust core.

### Added

- **Accounts and sync**: IMAP/SMTP accounts with automatic configuration,
  folder discovery (including SPECIAL-USE roles), incremental sync, IDLE for
  new mail, and CONDSTORE `CHANGEDSINCE` flag deltas with a full-sync fallback.
- **Offline-first storage**: a local SQLite cache for headers and bodies, so
  mail remains readable offline and on the next start.
- **Linux client (GTK 4 / libadwaita)**: three-pane shell, account wizard,
  per-account last-folder and window/split persistence, unified inbox for
  multiple accounts, HTML reader in a sandboxed WebKit view, remote-content
  blocking with per-sender trust, attachments, compose/reply/forward,
  a single Insert menu for templates, contacts, signatures and attachments,
  full-text search, and desktop notifications.
- **Windows client (WinUI 3)**: three-pane shell, account wizard, reader with
  WebView2, attachments, compose, search, a unified inbox for multiple accounts,
  the core event pump, and toast notifications with activation.
- **Mail features**: PGP encryption and signing with a key manager, queueing in
  an outbox with retry, draft auto-save, `mailto:` handling, and
  `src/mailto` composition prefills.
- **Search**: full-text search across the current folder or all mail, with
  per-account search history (delete one query or clear all) and highlighted
  matches in result snippets.
- **Packaging**: Flatpak bundle and desktop metadata for Linux; a
  self-contained zip, an unsigned MSIX, and an Inno Setup installer for
  Windows, with `SHA256SUMS`, build-provenance attestations, and a release
  workflow.
- **Supportability**: a rolling log (`mailin.log`, two files of 5 MB), a panic
  hook with a next-start notice, a "Copy diagnostics" action, a "Back up local
  data…" action, a pre-migration database snapshot, and a recovery window for
  unopenable local data (retry, open the data folder, reset the local cache,
  quit).
- **Accessibility**: keyboard navigation, a shortcuts overlay, focus handling
  in dialogs, screen-reader labels for icon-only buttons, and unread counts
  spelled out.

### Changed

- Preferences apply their changes on save instead of immediately on selection.
- The default message page size is 100 (range 1–1000).
- Theme changes apply without restarting the Linux client.
- Raised the workspace MSRV to 1.95; the child crates now inherit it.

### Fixed

- The account wizard no longer shows the password field and OAuth2 panel at the
  same time.
- A fresh account no longer opens on the virtual Outbox before folders arrive.
- Synchronization preferences now control real behaviour instead of being
  no-ops.
- The virtual Outbox no longer shows a misleading unread badge.
- Actions that need a selected message or account now give feedback instead of
  doing nothing.
- Repository and documentation URLs point at the real project location.
- Permanent deletion now asks for confirmation on every Windows path, including
  Delete in Trash, Ctrl+Delete, the reader and bulk actions, quick actions and
  Empty Trash. Cancelling leaves the messages untouched.
- Recipient autocomplete now works for the 2nd and later recipients on both
  front-ends, including semicolon-separated lists and quoted display names that
  contain commas.
- WinUI preferences now expose quick-action toggles and message defaults, with
  a "Restore message defaults" button; selections persist across restarts.

### Security

- Upgraded bundled SQLite (`libsqlite3-sys`) to 0.39.1 for `RUSTSEC-2026-0459`;
  no advisory ignore was added.

### Known limitations

- **WinUI parity**: the Windows client uses checkbox-based bulk selection
  rather than GTK's multi-select list, and its two-pane reader keeps one
  message open at a time.
- **Windows signing**: the MSIX and installer are unsigned, so SmartScreen
  warns on first run.
- **Narrow windows and accessibility**: the WinUI main window has no enforced
  minimum size; the 640×480 / 200% zoom, keyboard and screen-reader passes are
  manual before RC1.
- **Windows App SDK**: the 1.8 runtime is required; the zip and installer
  include or fetch it.
- **STARTTLS**: the GreenMail integration server used in CI does not support
  STARTTLS, so that path is covered by unit tests and manual testing rather
  than the automated integration suite.
- **OAuth2**: Mailin does not ship a shared client ID. To sign in with a
  provider you need a client ID from a registered application (or an
  administrator-supplied `MAILIN_*_CLIENT_ID`); the account wizard says so and
  keeps browser sign-in disabled until one is entered.
- **Not yet available in the UI**: rules, CalDAV/calendar, snooze, virtual
  folders, a plugin marketplace, tray/background-on-close, and macOS.
- **Local search scaling**: list virtualization is not incremental; very large
  folders (well beyond 5k messages) may feel slow.

[0.1.0]: https://github.com/ibro-3/mailin/releases/tag/v0.1.0
