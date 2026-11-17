# Mailin Release Checklist

This checklist must be completed and signed off for each release candidate before tagging.

## Release: v0.1.0

**Date:** 2026-11-17  
**Release Manager:** ___________ (assign before tagging)  
**Git Commit:** ebcf664 (must be the tagged commit; update if the tree changes)

---

## Pre-Release Verification

### Core Functionality (All Platforms)

- [ ] **IMAP/SMTP Authentication**
  - [ ] Plain LOGIN/PASSWORD works
  - [ ] OAuth2 (Google, Microsoft, Yahoo, Fastmail, Custom) works
  - [ ] OAuth2 token refresh works automatically
  - [ ] App password fallback works

- [ ] **Folder Operations**
  - [ ] Sync account (full + incremental)
  - [ ] Folder listing with roles (Inbox, Sent, Drafts, Archive, Trash, Junk)
  - [ ] Move messages between folders (Archive, Trash, Junk, Inbox)
  - [ ] Copy/Move/Delete with UIDPLUS fallback
  - [ ] Empty Trash works
  - [ ] Mark as Read/Unread, Flag/Unflag

- [ ] **Message Operations**
  - [ ] Fetch message headers
  - [ ] Fetch message body (plain + HTML)
  - [ ] Attachments: list, download, save
  - [ ] Search messages

- [ ] **Compose & Send**
  - [ ] New message, Reply, Reply All, Forward
  - [ ] Attachments (up to 50MB)
  - [ ] Queue to outbox, send via SMTP
  - [ ] Copy to Sent folder
  - [ ] Drafts: auto-save (30s), manual save, resume, delete on send

- [ ] **Outbox**
  - [ ] Queued → Sending → Sent states visible
  - [ ] Failed shows error, Retry resets to Queued
  - [ ] Discard deletes entry
  - [ ] Edit re-opens composer

- [ ] **Accounts**
  - [ ] Add account (IMAP + SMTP settings)
  - [ ] Edit account settings
  - [ ] Test Connection
  - [ ] Remove account (cleans DB + keyring)

- [ ] **Settings**
  - [ ] All Config fields editable
  - [ ] Theme changes apply immediately (GTK)
  - [ ] Changes persist across restart

### GTK Front-End (Linux)

- [ ] Application launches without errors
- [ ] Window opens with correct size
- [ ] Shortcuts work (Ctrl+N, Ctrl+F, F5, Ctrl+Shift+A, Ctrl+,, Delete, Ctrl+Delete, Ctrl+Shift+E/J/K)
- [ ] Multi-select in message list works
- [ ] Toolbar actions (Archive, Junk, Trash) work
- [ ] Overflow menu (Not Junk, Delete Permanently, Empty Trash) works
- [ ] Keyboard accelerators work
- [ ] Shortcuts window (F1 / Ctrl+?) opens
- [ ] Notifications work (GNotification)
- [ ] `mailto:` links open composer
- [ ] Remote content banner works
- [ ] Theme switching works (System/Light/Dark)

### WinUI Front-End (Windows)

- [ ] Application launches without errors
- [ ] Window opens with correct size
- [ ] Shortcuts work (same as GTK)
- [ ] Single-select message list works
- [ ] Reader actions (Archive, Junk, Trash, Retry, Discard, Edit) work
- [ ] Keyboard accelerators work
- [ ] Shortcuts window opens
- [ ] Toast notifications work (Action Center)
- [ ] `mailto:` links open composer
- [ ] Remote content banner works
- [ ] WebView2 renders HTML correctly

### Security & Privacy

- [ ] Remote content blocked by default
- [ ] OAuth2 refresh tokens stored in OS keyring (not SQLite)
- [ ] Passwords stored in OS keyring
- [ ] No plaintext secrets in logs
- [ ] TLS verification enabled by default

### First-release UX acceptance (WS1)

- [ ] **OAuth2 is honest in both account wizards**: the wizard states that Mailin ships no shared client ID, names the provider registration path / `MAILIN_*_CLIENT_ID`, and keeps browser sign-in disabled until a client ID is present. Verify on GTK and WinUI.
- [ ] **OAuth2 live sign-in verified with at least one provider** using a registered client ID and both loopback redirect URIs (`127.0.0.1` and `localhost`), or OAuth2 is explicitly documented as advanced/BYO in release notes.
- [ ] **Windows minimum window size**: the main window cannot be resized below a usable size; the toolbar and reader are not clipped at 640×480 and 200% text scaling.
- [ ] **Windows error surface**: the banner shows short copy plus **Details** (untruncated error text) and **Retry** only when the core marks the error retryable; Details/Cancel never performs the failed action twice.
- [ ] **Windows theme**: System/Light/Dark applies to the whole window without restart, and Windows high contrast uses system colors. The old "restart required" copy is gone.
- [ ] **Recovery windows (both platforms)** state that server mail is unaffected and offer **Copy diagnostics**; the report contains no passwords, message bodies or addresses.
- [ ] **Documentation matches the build**: default page size 100 (range 1–1000), unified-inbox availability, Windows signing status, OAuth2 client-ID requirement.
- [ ] **Flatpak listing assets**: 3–5 screenshots with captions, branding colors and the release entry present; `appstreamcli validate --no-net` passes. Screenshots must be captured on a real desktop session.

### WinUI visual system (WS2)

- [ ] **Iconography**: toolbar, reader action bar, folder roles, thread chevrons, star and attachment markers use Segoe Fluent Icons glyphs with accessible names; no emoji or text glyphs remain in the chrome.
- [ ] **Primary action**: Compose is the single accented action in the toolbar.
- [ ] **Row anatomy**: message rows show unread dot, initials avatar (except compact density), sender/subject, time, snippet and flags; the same states are readable on GTK and WinUI.
- [ ] **Live theme**: changing System/Light/Dark in Preferences recolors the whole window without a restart (custom-brush controls may need reopening). High contrast overrides the theme.
- [ ] **Visual review**: toolbar and reader action bar do not wrap or clip at 640×480, 1024×768 and 1440×900, including 200% text scaling; light/dark/high-contrast screenshots approved.
- [ ] **Unified inbox (WinUI)**: with two or more accounts the picker offers "All Accounts (Unified Inbox)"; the folder list shows a single unified folder with the combined unread count; opening a message from another account still trusts/loads remote content per that message's account; Sync synchronizes every account; search spans all accounts; account edit/remove is disabled while unified.
- [ ] **Search (WinUI)**: search box has a This folder / All mail scope selector and a recent-queries drop-down above the results; selecting a recent query re-runs it; results are scoped to the selected folder by default and show a result count in the list title.

### State and feedback (WS3)

- [ ] **Five pane states**: folders, message list and reader each cover first-load, refreshing, empty, error and offline states; empty/error states name the cause and offer a working next action (Retry / Clear search / Add account).
- [ ] **Offline and error precedence**: an empty folder while offline or after a sync error shows the offline/error state rather than "no messages"; Retry re-syncs and a successful sync clears it.
- [ ] **Sync status honesty**: the header chip shows `Syncing N/M` with multiple accounts, names affected accounts and the last successful sync in its accessible summary, and colors match the GTK semantics (accent/warning/error).
- [ ] **No silent dead ends**: every action needing a selected message or account gives feedback; no spinner runs indefinitely without an explanatory label.
- [ ] **Result counts**: both list titles show the number of search results and the active scope.
- [ ] **Stale-while-revalidate**: background sync keeps the cached list visible; a small "Updating…" cue shows elapsed seconds, and a sync over 30 s is labeled "taking longer than usual" in the status summary.
- [ ] **Live regions**: empty/error states and the sync cue are polite live regions so screen readers announce them, and the GTK status tooltip includes the last successful sync time.

### Information architecture (WS4)

- [ ] **Preferences → Accounts is actionable**: both clients list every account with its sync status and working Edit, Remove and Add Account actions; removing asks for confirmation and cancel changes nothing. The old explanatory-only page is gone.
- [ ] **Two discovery paths**: account management is reachable from the main window and from Preferences; after an add/edit/remove the list refreshes without restarting.
- [ ] **Account removal safety**: WinUI account removal uses the same destructive confirmation as GTK, naming the account and stating that local data will be deleted.

---

## Platform-Specific Verification

### Linux (Flatpak)

- [ ] `flatpak build` succeeds
- [ ] `flatpak run dev.mailin.Mailin` launches
- [ ] Flatpak manifest validates (`flatpak-builder-lint`)
- [ ] AppStream metadata validates (`appstreamcli validate`)
- [ ] `.desktop` file validates (`desktop-file-validate`)
- [ ] `MimeType=x-scheme-handler/mailto;` present
- [ ] Icons display correctly in app launcher
- [ ] Permissions: network, dbus, keyring access work
- [ ] Sandboxed keyring access works

### Windows (MSIX + Inno Setup)

- [ ] `cargo xtask package-win` succeeds
- [ ] Self-contained ZIP extracts and runs
- [ ] MSIX installs via `Add-AppxPackage` (requires dev cert for testing)
- [ ] Inno Setup installer (`mailin-setup.exe`) installs correctly
- [ ] Start Menu entry created
- [ ] Taskbar icon correct
- [ ] Protocol handler `mailto:` registered
- [ ] Uninstaller works cleanly
- [ ] SmartScreen reputation (needs EV cert for production)

### macOS (Future)

- [ ] N/A for 1.0

---

## Install Matrix (H3)

Run each scenario on a clean machine or a fresh container/VM and record the
result. Replace the placeholder dates with the actual run dates.

| Scenario | Platform | Result | Date / Notes |
|---|---|---|---|
| Flatpak fresh install from release bundle | Linux | ☐ Pass ☐ Fail | |
| Flatpak upgrade over previous RC | Linux | ☐ Pass ☐ Fail | |
| Flatpak sandbox keyring unlock after reboot | Linux | ☐ Pass ☐ Fail | |
| Self-contained ZIP: extract + run | Windows 11 | ☐ Pass ☐ Fail | |
| MSIX: install / launch / remove | Windows 11 | ☐ Pass ☐ Fail | |
| MSIX: upgrade over previous version | Windows 11 | ☐ Pass ☐ Fail | |
| Inno Setup: install / launch | Windows 11 | ☐ Pass ☐ Fail | |
| Inno Setup: upgrade / uninstall (no leftovers) | Windows 11 | ☐ Pass ☐ Fail | |
| `mailto:` from browser → composer | Linux / Windows | ☐ Pass ☐ Fail | |
| Desktop notification shown + activation | Linux / Windows | ☐ Pass ☐ Fail | |
| Keyring unlock after reboot, no password prompt loop | Linux / Windows | ☐ Pass ☐ Fail | |
| 200% zoom, 640×480 window (header not clipped) | Linux / Windows | ☐ Pass ☐ Fail | |

---

## Failure Drills (H4)

These drills must leave the local data readable or explicitly reset with
renamed files — never silently deleted.

- [ ] **Corrupt database**: close Mailin, replace `mailin.db` with a truncated
      copy, start again → recovery window offers Retry / Open data folder /
      Reset local cache / Quit; Reset renames the unreadable files.
- [ ] **Read-only data directory**: start with the data directory made
      read-only → the app reports a clear error and does not lose data.
- [ ] **Wrong password**: change an account password server-side → sync fails
      with an actionable message, other accounts keep syncing.
- [ ] **Expired OAuth token**: rotate/revoke a test OAuth token → refresh is
      attempted and failure is reported without a crash loop.
- [ ] **Offline start**: start with networking disabled → cached mail opens,
      sync status says Offline, reconnect resumes sync.
- [ ] **No IDLE**: use a server without IDLE → polling still delivers new mail.
- [ ] **No SPECIAL-USE**: use a server that does not advertise SPECIAL-USE →
      folder roles fall back to name heuristics and the wizard still finishes.
- [ ] **10k-message folder**: open and scroll a 10k-message folder → the list
      stays responsive and selection/scroll position survives refresh.
- [ ] **Failed send**: disconnect SMTP mid-send → the outbox shows Retry and a
      retry after reconnecting delivers exactly one copy.

---

## CI/CD Pipeline Verification

- [ ] `cargo fmt --all -- --check` passes
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` passes
- [ ] `cargo test --workspace --all-targets` passes
- [ ] `cargo run -p xtask -- check-versions` passes (version agreement)
- [ ] `cargo deny check` passes with no new `deny.toml` ignore entries
- [ ] `cargo tree -i libsqlite3-sys` shows 0.39.1 or newer (no RUSTSEC-2026-0459)
- [ ] `packaging/flatpak/cargo-sources.json` matches `Cargo.lock`
- [ ] WinUI permanent-delete confirmation prompts on every path (Delete in Trash, Ctrl+Delete,
      reader Trash, bulk bar, quick action, Empty Trash); Cancel/Esc performs no deletion
- [ ] WinUI quick-action and message-default settings persist across restart
- [ ] Multi-recipient autocomplete works on GTK and WinUI (commas, semicolons, quoted names)
- [ ] `flatpak-builder-lint manifest packaging/flatpak/dev.mailin.Mailin.json` passes
- [ ] `cargo test -p mailin-core --test imap_it -- --ignored` passes (plaintext)
- [ ] `cargo test -p mailin-core --test imap_it_tls -- --ignored` passes (IMAPS 993 / SMTPS 465)
- [ ] `cargo test -p mailin-core --test imap_it_starttls -- --ignored` passes (STARTTLS 143/587)
- [ ] Flatpak bundle builds and uploads
- [ ] Windows artifacts build and upload (ZIP, MSIX, EXE, SHA256SUMS)
- [ ] GitHub Release created with all artifacts
- [ ] SHA256SUMS file present and correct

---

## Integration Tests (Required for Each Release)

### Plaintext (Port 143/25/3143/3025)
- [ ] IMAP LOGIN authentication
- [ ] SMTP AUTH LOGIN
- [ ] Sync → Send → Fetch body → Set flags round-trip

### TLS/SSL (Port 993/465)
- [ ] IMAPS (IMAP over TLS on 993)
- [ ] SMTPS (SMTP over TLS on 465)
- [ ] Certificate verification
- [ ] Sync → Send → Fetch body round-trip

### STARTTLS (Port 143/587)
- [ ] IMAP STARTTLS upgrade on 143
- [ ] SMTP STARTTLS upgrade on 587
- [ ] Certificate verification
- [ ] Sync → Send → Fetch body round-trip

### OAuth2 (Live Providers - Manual, BYO client ID)
- [ ] Google (requires a client ID from a registered application)
- [ ] Microsoft 365/Outlook (requires registered client ID)
- [ ] Yahoo (requires registered client ID)
- [ ] Fastmail (requires registered client ID)

---

## Documentation & Changelog

- [ ] CHANGELOG.md updated with release notes
- [ ] Version bumped in:
  - [ ] `Cargo.toml` (workspace)
  - [ ] `packaging/flatpak/dev.mailin.Mailin.metainfo.xml`
  - [ ] `packaging/windows/AppxManifest.xml`
  - [ ] `packaging/windows/setup.iss`
- [ ] `cargo xtask release-version <version> <date>` used for the bump
- [ ] README.md updated if needed
- [ ] Migration guide (if breaking changes)

---

## Post-Release

- [ ] Git tag pushed (`git push origin vX.Y.Z`)
- [ ] GitHub Release published with artifacts
- [ ] SHA256SUMS verified against downloaded artifacts
- [ ] Flatpak submitted to Flathub (if not automated)
- [ ] Announcement prepared (blog, social, mailing list)
- [ ] Next version development branch created

---

## Automated Evidence (Sprint 3, Linux host)

Recorded during the RC1 preparation pass. Commands marked "not run" require the
Windows CI runner or a manual desktop session and must be ticked there instead.

- [x] `cargo fmt --all`
- [x] `cargo clippy --workspace --all-targets -- -D warnings`
- [x] `cargo test --workspace --all-targets`
- [x] `cargo run -p xtask -- check-versions` → `versions agree: 0.1.0`
- [x] `cargo test -p xtask --bin xtask` (7 unit tests passed)
- [x] `appstreamcli validate --no-net packaging/flatpak/dev.mailin.Mailin.metainfo.xml`
- [ ] WinUI build / manual QA — not runnable on Linux; see F2
- [ ] Screenshot review in Light / Dark / high contrast — manual
- [ ] Install matrix and failure drills — manual

Do not mark a box above as passed unless the command was actually run for the
tag being signed off.

---

## Sign-Off

| Role | Name | Date | Signature |
|------|------|------|-----------|
| Release Manager | | | |
| QA Lead | | | |
| Security Review | | | |

---

## Notes

Record any known issues, workarounds, or follow-up items here:

- **D1 — RUSTSEC-2026-0459:** fixed by upgrading, not by ignoring. Workspace MSRV
  raised to 1.95 and `libsqlite3-sys` resolved to 0.39.1. The two unrelated
  RustCrypto ignores remain; no new ignore was added.
- **D2 — Windows signing:** shipping unsigned for v0.1.0. MSIX and installer are
  unsigned, `SHA256SUMS` is published, and the SmartScreen warning is documented
  in the changelog and release notes. Revisit when an EV certificate is available.
- **D3 — Flathub:** publish the GitHub release and verify artifacts first;
  submit to Flathub only after that verification passes.
- **D4 — Live OAuth:** not tested against live providers (no registered client
  IDs); recorded as a known limitation rather than a passed checklist item.