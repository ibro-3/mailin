# Mailin

A native desktop email client with one shared Rust core and two native front-ends:

- **Linux** — GTK 4 + libadwaita
- **Windows** — WinUI 3 (Windows App SDK)

Mailin speaks IMAP and SMTP, keeps a local SQLite cache for offline reading, and
never stores passwords outside the OS keyring.

> **Status:** early, but usable on Linux. Milestones 0–3 are done: workspace,
> CI, configuration, credential storage, SQLite schema, full text search,
> outbox, the IMAP/SMTP engine (folder discovery, incremental sync, IDLE,
> sending) and the GTK shell (three-pane window, account wizard, reader). GTK
> polish and the WinUI front-end are next (see the roadmap below).

## Layout

```
crates/mailin-core   UI-agnostic core: accounts, store, credentials, sync
crates/mailin-gtk    GTK 4 front-end: shell, wizard, reader (M3–M4)
crates/mailin-winui  WinUI 3 front-end                     (M5–M6)
xtask                repository automation
docs/                architecture notes
packaging/           Flatpak and Windows packaging
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
| 4 | GTK polish: HTML mail, attachments, compose, notifications, Flatpak |
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

See [`docs/architecture.md`](docs/architecture.md) for the full picture and the
known Windows/WinUI constraints.

## License

Dual licensed under MIT or Apache-2.0, at your option. See
[`LICENSE-MIT`](LICENSE-MIT) and [`LICENSE-APACHE`](LICENSE-APACHE).
