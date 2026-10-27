# Mailin Release Checklist

This checklist must be completed and signed off for each release candidate before tagging.

## Release: vX.Y.Z

**Date:** ___________  
**Release Manager:** ___________  
**Git Commit:** ___________

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

## CI/CD Pipeline Verification

- [ ] `cargo fmt --all -- --check` passes
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` passes
- [ ] `cargo test --workspace --all-targets` passes
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

### OAuth2 (Live Providers - Manual)
- [ ] Google (requires registered client ID)
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

## Sign-Off

| Role | Name | Date | Signature |
|------|------|------|-----------|
| Release Manager | | | |
| QA Lead | | | |
| Security Review | | | |

---

## Notes

Record any known issues, workarounds, or follow-up items here:

```
```