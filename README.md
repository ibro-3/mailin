# Mailin

A native desktop email client with one shared Rust core and two native front-ends:

- **Linux** — GTK 4 + libadwaita
- **Windows** — WinUI 3 (Windows App SDK)

Mailin speaks IMAP and SMTP, keeps a local SQLite cache for offline reading, and
never stores passwords outside the OS keyring.

> **Status:** early, but usable on Linux. Milestones 0–4 are done: workspace,
> CI, configuration, credential storage, SQLite schema, full text search,
> outbox, the IMAP/SMTP engine (folder discovery, incremental sync, IDLE,
> sending), the GTK shell (three-pane window, account wizard, reader) and GTK
> polish (HTML mail, remote-content blocking, attachments, compose, desktop
> notifications and Flatpak packaging). The WinUI front-end is next (see the
> roadmap below).

## Layout

```
crates/mailin-core   UI-agnostic core: accounts, store, credentials, sync
crates/mailin-gtk    GTK 4 front-end: shell, wizard, reader (M3–M4)
crates/mailin-winui  WinUI 3 front-end                     (M5–M6)
xtask                repository automation
docs/                architecture notes
packaging/           Flatpak manifest, desktop metadata and icon
```

## Building

### Linux

```sh
sudo apt install build-essential pkg-config libgtk-4-dev libadwaita-1-dev libwebkitgtk-6.0-dev
cargo build
```

Then start the client:

```sh
cargo run -p mailin-gtk
```

The binary is called `mailin`. The first start shows a welcome page; add an
account with the wizard and folders, message headers and bodies are fetched in
the background. The core alone needs only a Rust toolchain (SQLite is compiled
in through `rusqlite`'s `bundled` feature).

### Flatpak

The manifest lives in `packaging/flatpak/`. Install the builder and the GNOME
runtime once, then build and run the sandboxed app:

```sh
sudo apt install flatpak-builder
flatpak remote-add --if-not-exists flathub https://flathub.org/repo/flathub.flatpakrepo
flatpak install flathub org.gnome.Platform//50 org.gnome.Sdk//50 \
  org.freedesktop.Sdk.Extension.rust-stable//25.08
flatpak-builder --user --install --force-clean \
  packaging/flatpak/build-dir packaging/flatpak/dev.mailin.Mailin.json
flatpak run dev.mailin.Mailin
```

`packaging/flatpak/cargo-sources.json` vendors every crate from `Cargo.lock` for
the offline build. Regenerate it with
[`flatpak-cargo-generator.py`](https://github.com/flatpak/flatpak-builder-tools/tree/master/cargo)
whenever the lockfile changes, and run `appstreamcli validate --no-net` after
editing the metainfo file. CI builds the bundle on every push and pull request.

### Windows

The WinUI front-end is built in CI on `windows-latest` (see
`.github/workflows/ci.yml`). A local Windows checkout needs the MSVC build tools
plus the Windows App SDK runtime; the core itself builds anywhere:

```powershell
cargo build -p mailin-core
```

## Testing

```sh
cargo xtask check                 # fmt + clippy + unit/integration tests
cargo test -p mailin-core         # just the core
cargo test -p mailin-gtk          # GTK helper logic, no display needed
```

The IMAP/SMTP harness talks to [GreenMail](https://greenmail-mail-test.github.io/greenmail/):

```sh
cargo xtask it-up
cargo test -p mailin-core --test imap_it -- --ignored
cargo xtask it-down
```

These tests are ignored by default so a normal `cargo test` needs no Docker.

## Where things live

| What | Linux | Windows |
|---|---|---|
| Preferences | `~/.config/mailin/config.toml` | `%APPDATA%\mailin\Mailin\config\config.toml` |
| Database | `~/.local/share/mailin/mailin.db` | `%LOCALAPPDATA%\mailin\Mailin\data\mailin.db` |
| Passwords | Secret Service (GNOME Keyring, KWallet) | Credential Manager |

Set `MAILIN_HOME` to relocate all three directories (used by tests and portable
installs). Accounts and their server settings live in the database; the config
file holds only application preferences and never contains secrets. The database
and config file are created with `0600` permissions on Unix.

## Roadmap

| Milestone | Scope |
|---|---|
| 0 | Workspace, CI, docs ✅ |
| 1 | Config, credentials, SQLite schema, search, outbox ✅ |
| 2 | IMAP/SMTP engine: folder discovery, incremental sync, IDLE, send ✅ |
| 3 | GTK shell: three-pane window, account wizard, reader ✅ |
| 4 | GTK polish: HTML mail, attachments, compose, notifications, Flatpak ✅ |
| 5 | WinUI 3 binding spike: projection, bootstrapping, window, WebView2 |
| 6 | WinUI front-end parity |
| 7 | Packaging, hardening, accessibility, performance |

## Design notes

- The core is synchronous and cheap to call from a UI thread. Network work is
  isolated in worker tasks; front-ends subscribe to a broadcast channel of
  advisory events and re-read the affected rows.
- One SQLite connection behind a mutex, WAL mode, single transaction per sync
  page. Search uses FTS5; the index row id mirrors `messages.id`, so updates and
  deletes stay cheap.
- UIDVALIDITY, CONDSTORE and IDLE handling are designed in `docs/architecture.md`.
- HTML bodies render in a locked-down WebKit view with JavaScript, plugins and
  remote content disabled until the user explicitly loads images for a message.
- The Flatpak sandbox gets network, Wayland/X11, GPU access, the Secret Service
  and desktop notifications; nothing else is shared with the host.

See [`docs/architecture.md`](docs/architecture.md) for the full picture and the
known Windows/WinUI constraints.

## License

Dual licensed under MIT or Apache-2.0, at your option. See
[`LICENSE-MIT`](LICENSE-MIT) and [`LICENSE-APACHE`](LICENSE-APACHE).
