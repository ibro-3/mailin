# Architecture

## Goals

Mailin is a native mail client, not a web view in a window. The same Rust core
powers two front-ends that use each platform's real toolkit:

| Platform | Toolkit | HTML rendering |
|---|---|---|
| Linux | GTK 4 via `gtk4-rs`, libadwaita for layout and styling | WebKitGTK 6 (`webkit6`) |
| Windows | WinUI 3 (Windows App SDK) | WinUI `WebView2` control (WebView2 runtime) |

Both front-ends are thin: they render state, forward user intent, and never talk
to the network themselves. Everything protocol related lives in `mailin-core`.

## Crate graph

```
mailin-gtk ─┐
            ├──> mailin-core ──> IMAP / SMTP servers
mailin-winui┘        │
                     └──> SQLite (local cache) + OS keyring

mailin-winui ──> mailin-winui-sys (generated Windows App SDK projection)
```

`mailin-core` has no UI dependency and no platform specific code. The only
feature flag that changes behaviour is `keyring`, which enables the OS
credential store; tests use `MemoryCredentials`.

## Core invariants

1. **Accounts live in the database, preferences live in `config.toml`.**
   Account settings are stored as JSON inside `accounts.settings_json`, so
   renaming an account or editing a server cannot desynchronize two files.
   `config.toml` contains UI and sync preferences only.
2. **Passwords never touch either.** They are stored through
   `CredentialsStore`, backed by Secret Service on Linux and Credential Manager
   on Windows.
3. **One writer.** `Store` owns a single SQLite connection behind a mutex. WAL
   mode allows concurrent readers; the API is synchronous so UI callbacks can
   use it directly for small reads. Network workers write whole pages in one
   transaction (`upsert_messages`).
4. **UI-agnostic events.** `Mailin::subscribe()` hands out a broadcast receiver
   of `Event`s. Events are advisory: the UI re-reads the affected rows, which
   keeps ordering between the database and the event stream irrelevant.
5. **No `unsafe` in the core.** The workspace denies `unsafe_code`. The WinUI
   crates relax this per crate because COM/WinRT interop is inherently unsafe;
   the core and the GTK front-end stay safe.

## Data model

| Table | Purpose |
|---|---|
| `accounts` | id, address, display name, full settings JSON, creation time |
| `folders` | IMAP path, delimiter, role, UIDVALIDITY/UIDNEXT/HIGHESTMODSEQ cursors |
| `messages` | headers, flags, size, snippet, body state; unique per `(folder_id, uid)` |
| `bodies` | parsed plain text and HTML, fetched lazily |
| `attachments` | MIME part metadata plus decoded bytes (`data BLOB`, migration 0002) |
| `outbox` | queued RFC 5322 messages with attempt counters |
| `messages_fts` | FTS5 index over subject, participants and plain text body |

Folder roles (`inbox`, `sent`, `drafts`, `trash`, `junk`, `archive`) come from
IMAP `SPECIAL-USE` attributes (RFC 6154) with a fallback on the leaf name.

Flags are stored as a space separated string (`"\Seen \Flagged"`), matching the
wire format and keeping unread queries a single `instr()` check.

### Full text search

`messages_fts` is a normal (not contentless, not external-content) FTS5 table
whose rowid is `messages.id`. That choice matters:

- updates and deletes are plain `UPDATE … WHERE rowid = ?`, safe and fast;
- bodies can be indexed later without rebuilding the table;
- the index is populated for headers on arrival, so search works before a body
  is fetched.

Trade-off: header text is stored twice. Headers are small; correctness and
simple migrations are worth more here.

Search input is never passed to FTS5 verbatim. `fts_query` keeps up to eight
alphanumeric terms, quotes them and appends `*` for prefix matching, joined with
`AND`. This prevents syntax errors from user input and avoids accidental
operator injection.

## Synchronization design (milestone 2)

Each account has one supervised worker with one IMAP connection. Commands,
`IDLE` wake-ups and periodic polls are serialized through that connection, so
the selected mailbox and UIDVALIDITY cannot be changed under another operation.
Commands for an account that is reconnecting are queued and replayed.

On connect the worker runs `LIST`, upserts the folders (roles come from
`SPECIAL-USE` with a name fallback) and prunes folders the server no longer
reports.

A **full sync** of a folder:

1. `SELECT` the mailbox and read `UIDVALIDITY`, `UIDNEXT`, `HIGHESTMODSEQ`.
2. If `UIDVALIDITY` changed, discard the folder's cached messages and start
   over — UIDs are meaningless under a new validity.
3. `UID SEARCH ALL`; fetch headers for UIDs missing locally in chunks of 50 with
   `BODY.PEEK[HEADER]`, `FLAGS`, `INTERNALDATE` and `RFC822.SIZE`, and persist
   each chunk in one transaction. Headers are decoded with `mail-parser`, so
   RFC 2047 subjects and address lists are stored in readable form.
4. Delete cached messages whose UID disappeared on the server.
5. Refresh flags for every message (skipped above 50 000 messages to bound the
   response; new messages still carry their flags).
6. Persist the new cursor and publish `MessagesChanged`/`FoldersChanged`.

An **incremental sync** is the cheap path: `UID SEARCH UID <max_cached + 1>:*`,
filter off the range quirk, and fetch headers only for the new UIDs. It runs on
every IDLE wake-up and every poll tick.

`IDLE` watches the INBOX when the server advertises it; otherwise the worker
polls every `poll_interval_secs`. A full sync of the INBOX runs every ten
minutes to notice deletions and flag edits made by other clients.

`SELECT` uses the CONDSTORE variant when available, which makes
`HIGHESTMODSEQ` visible; `CHANGEDSINCE`-based flag deltas are a follow-up to the
wholesale refresh above. `account.sync.folder_allowlist` limits which folders are
synchronized.

Bodies are fetched with `BODY.PEEK[]` when a message is opened and cached; a body
is never fetched twice. The full MIME body includes all parts, so decoded
attachment bytes are stored alongside the headers in the same transaction.
Inline `cid:` parts keep their normalized content id for the HTML viewer.
Attachment metadata is listed immediately after a header sync; if an old row
predates migration 0002 and has no bytes yet, opening the message or saving the
attachment triggers one `refresh_body` fetch that fills them in.

Reconnects use exponential backoff (5 s to 5 min) and keep at most one
connection per account. `Engine::stop` interrupts IDLE and shuts the runtime
down with a short grace period.

### Sending

Compose builds an RFC 5322 message in the core (`compose` module), stores it in
`outbox`, and the sender worker:

1. claims the oldest queued entry and marks it `sending`,
2. submits it over SMTP (implicit TLS or STARTTLS, `lettre` with rustls),
3. marks the entry `sent` as soon as the server accepted it,
4. appends a copy to the Sent folder as a best-effort follow-up, so a server
   without a usable Sent folder cannot cause a second submission.

`Bcc` recipients come from the stored envelope JSON, not from the headers, so a
`Bcc` header never leaks to the other recipients.

The outbox survives restarts, so a crash or network outage cannot lose a message
that the user already "sent". Failed entries retry up to five times.

## Threading and UI integration

```
tokio runtime (core workers)          UI main loop
        │                                  │
        ├── Store (mutex) ─────────────────┤ reads
        │                                  │
        └── broadcast::Sender<Event> ──> marshal to main loop ──> widgets
```

- **GTK:** a `glib::timeout` source drains the event receiver every 200 ms on
  the main loop; tokio runs on its own runtime. UI code never blocks on network
  work. `NewMessages` events from background Inbox syncs become
  `GNotification`s; the initial population of a mailbox is suppressed, and a
  click activates `app.present` to raise the window.
- **WinUI:** an `async_channel` bridge posts to the `DispatcherQueue`; all
  widget mutation happens on the STA UI thread.

## GTK front-end (milestones 3–4)

`mailin-gtk` is a thin shell around `mailin-core`:

| Module | Responsibility |
|---|---|
| `app` | opens `Paths`/`Config`, applies the theme, starts `Engine`, owns the application |
| `window` | three-pane layout, event pump, folder/message selection, reader, notifications |
| `account_dialog` | add-account wizard and pure form validation |
| `autoconfig` | provider table and `imap.<domain>` / `smtp.<domain>` fallback |
| `compose` | compose window, attachment picker, reply/reply-all/forward drafts |
| `html` | CSP document builder, remote-content detection, `cid:`/`data:` rewriting, WebView setup |
| `format` | relative dates, snippets, sender labels, file sizes |

The window is an `adw::OverlaySplitView` (folders) with a `gtk::Paned` (message
list and reader). With no accounts the content area is a `StatusPage` with a
single "Add Account" action; after the first account the mail view replaces it.

State is read straight from `Store` on the main thread: listing a folder is a
small indexed query. Network work stays in the engine. The event pump drains the
broadcast receiver and coalesces events: account or folder changes rebuild the
sidebar, message changes reload the visible page, and `BodyReady` re-renders the
reader. A lagged receiver triggers all three refreshes, so a missed advisory
event cannot leave stale widgets behind.

The reader shows the decoded plain-text body by default and keeps a per-message
HTML toggle. `mail-parser` derives a text preview even for HTML-only mail, so
every message has something to display.

### HTML rendering and remote content

HTML is never loaded as a remote page. `html::document()` wraps the fragment in
a complete document with a restrictive `Content-Security-Policy` meta tag,
styles that respect the system light/dark preference, and no scripts. The
WebKit view disables JavaScript, plugins, file access and cross-site
navigation; only explicit user actions may open links in the system browser.

Remote images stay blocked unless the account preference allows them or the user
clicks "Load remote content" for the open message. `html::has_remote_content()`
detects `http(s)`/protocol-relative sources to decide whether the banner is
shown. Inline `cid:` images are rewritten to `data:` URLs from the cached
attachment bytes, so they render without network access.

### Attachments, compose and notifications

Non-inline attachments appear below the reader with a Save button. Saving uses
the cached bytes; older messages fetch the body once first, then reopen the save
dialog. Compose supports new messages, reply, reply-all and forward, file
attachments up to 50 MB, `Ctrl+Enter` to send, and queues the built message
through the core outbox. The window prefills recipients and quotes the original
body without hiding the composer behind a blocking dialog.

New-mail notifications use `GNotification` (`gio::Notification`) and respect the
`ui.notifications` preference. They are emitted only for a first Inbox sync with
new rows, so reconnect churn and Sent-folder appends do not spam the desktop.

### Flatpak

`packaging/flatpak/` contains the manifest, desktop entry, AppStream metainfo,
scalable icon and a generated `cargo-sources.json`. The manifest targets the
GNOME 50 runtime plus the Rust SDK extension, builds the workspace offline from
vendored crates, and grants only network, display, GPU, Secret Service and
notification access. CI builds a bundle on every push and pull request.

The account wizard validates the form before it touches the store, writes the
IMAP/SMTP settings to `accounts.settings_json`, and stores the password through
`CredentialsStore`. It prefills server fields from a small provider table and
falls back to the conventional `imap.<domain>` / `smtp.<domain>` names; a field
the user edited is never overwritten. Saving an account triggers
`Engine::sync_account`, so folders appear as soon as the first connection
succeeds.

Keyboard shortcuts: `Ctrl+F` search, `F5` synchronize, `Ctrl+Shift+A` add an
account. Search is debounced and runs against the FTS index; clearing it returns
to the selected folder.

## Windows / WinUI 3 constraints

These are verified constraints, not preferences:

- `windows` 0.62 exposes **no** XAML namespaces: the Windows App SDK metadata is
  not part of the Windows SDK, so `Microsoft.UI.Xaml` bindings must be generated
  separately. `mailin-winui-sys` contains that projection. It is generated by
  `cargo xtask winui-bindgen` from the pinned
  `Microsoft.WindowsAppSDK` 1.8, `Microsoft.Web.WebView2` and
  `Microsoft.Windows.SDK.Contracts` NuGet packages and committed to the
  repository.
- The projection is deliberately narrow. `crates/mailin-winui-sys/filter.txt`
  lists the application, window, layout, text and input controls, common
  collections, dispatcher timer and WebView2 types the client needs.
  Generating the whole `Microsoft.UI.Xaml` namespace produces roughly 43 MB of
  Rust; the filtered projection is a small fraction of that. `--minimal` keeps
  the static helpers but drops instance class methods, so instance members are
  called through their interfaces. Classes without an activation factory
  (`TextBlock`, `Border`, `ScrollViewer`, `PasswordBox`, `RowDefinition`,
  `ColumnDefinition`, `XamlControlsResources`) are created from small
  `XamlReader::Load` fragments instead of C#-style constructors.
- A pure Cargo build has **no XAML compiler**, so there are no `.xaml` files in
  the build. UI is composed in Rust; small declarative fragments can be parsed
  at runtime with `XamlReader::Load`.
- WinUI's `Application` is a composable runtime class. `MailinApplication`
  implements `IApplicationOverrides` with `#[implement]`; `Application::Start`
  drives `OnLaunched`, which creates the window. The generator is invoked with
  `--compose Microsoft.UI.Xaml.Application`, so the projection contains the
  aggregation helper `Application::compose`, so `app.rs` does not hand-roll
  the factory call.
- The App SDK runtime must be deployed: either installed system wide or copied
  next to the executable (self-contained, ~60 MB). The spike uses the framework
  package: `bootstrap.rs` loads `Microsoft.WindowsAppRuntime.Bootstrap.dll` and
  calls `MddBootstrapInitialize2(0x0001_0008, ...)`. `cargo xtask winui-runtime`
  downloads the bootstrapper; `build.rs` copies it next to the executable and
  falls back to `target/winui-runtime/`. CI installs the matching 1.8 runtime
  before building the release artifact.
- WinUI requires an STA thread with a `DispatcherQueue`. `app.rs` initialises
  the thread with `CoInitializeEx(COINIT_APARTMENTTHREADED)` before
  `Application::Start`; everything that touches XAML objects is confined to it.
- Consequence: Windows builds and the release artifact are verified in CI
  (`windows-latest`); the Linux development machine can only `cargo check` the
  Windows target. Runtime rendering (including WebView2) still has to be
  observed on a Windows desktop.

### Milestone 5 go/no-go

**Go.** The spike proves that the shared core, a generated App SDK projection,
the App SDK bootstrapper, a composed XAML application and a WebView2 control can
live in one Cargo workspace without a XAML compiler or a C# project. That
milestone deliberately stopped at infrastructure; the product UI landed in
milestone 6.

If the projection had proved unusable, the fallback was a WinUI 3 shell in C#
over the same core through a C ABI, or a native Win32 shell. That fallback is
not needed.

### Milestone 6: WinUI front-end parity

The WinUI client now has the same shape as the GTK client, still without a C#
project or a XAML compiler:

- `window.rs` builds the three-pane shell from Rust: toolbar with account
  picker, search and sync; folder sidebar with unread counts and role markers;
  message list with sender, subject, snippet, time, star and attachment
  markers; reader with headers, plain/HTML toggle, remote-content prompt,
  attachments and reply/reply-all/forward actions.
- `account_dialog.rs` mirrors the GTK wizard: identity, IMAP and SMTP fields,
  provider suggestions that never overwrite edited fields, the same validation,
  and a stable account id across retry attempts.
- `compose.rs` builds new messages, replies, replies-all and forwards, attaches
  files up to 50 MB and queues them through `Engine::queue_and_send`.
- `platform.rs` uses the Win32 common open/save dialogs because the WinRT
  pickers need an `HWND` interop object; attachment MIME guessing and size
  limits match the GTK client.
- `format.rs` produces the same relative times, full local dates, sender and
  subject fallbacks and file sizes. Win32 `FileTimeToLocalFileTime` and
  `FileTimeToSystemTime` replace GLib date formatting.
- `ui.rs` is the control layer: XAML fragments, text/button/panel/grid helpers,
  visibility and sizing, and event subscriptions. `EventRevoker`s are stored on
  the window and cleared when lists are rebuilt or a dialog closes, so handlers
  are not revoked while their controls are still alive.
- The core event broadcast is drained by a `DispatcherQueueTimer` on the UI
  thread; account, folder, message and body-change events refresh only the
  affected panes, and `Lagged` receivers fall back to a full refresh.
- HTML mail uses the shared `mailin_core::html` document, CSP, `cid:` rewriting
  and remote-content detector. The GTK-specific WebKit policy and the
  WinUI-specific WebView2 host stay in their own crates.
- Pure `autoconfig` and HTML policy helpers moved to `mailin-core`, so both
  front-ends test the same provider table and security policy.

The projection grew accordingly: layout panels, text/input controls,
`IItemsControl`/`ISelector`, `ICommand`, dispatcher timers and WebView2. It is
still generated from the pinned metadata; `filter.txt` is the source of truth.

Windows runtime behaviour (window sizing, WebView2 rendering and the common
file dialogs) still needs a Windows desktop or the `windows-latest` CI job; the
Linux development machine cross-checks the crate with the MSVC target.

## Testing strategy

| Layer | Approach |
|---|---|
| Store / search / outbox | In-memory and temp-file SQLite, real SQL, no mocks |
| Config / credentials | Temp dirs, permission assertions, in-memory store |
| Protocols | GreenMail container (IMAP + SMTP), ignored by default, run in CI |
| Front-ends | Pure logic (formatting, autoconfig, form validation, compose prefill, HTML policy) is unit tested; an ignored GTK test renders the account wizard on a desktop |
| Flatpak | `appstreamcli`/`desktop-file-validate` for metadata, full bundle build in CI |
| Windows | `windows-latest` CI: clippy, tests, release build, artifact upload |

## Decisions

| Decision | Rationale |
|---|---|
| SQLite via `rusqlite` with `bundled` | Same engine and version on every platform, FTS5 guaranteed |
| `async-imap` with the `runtime-tokio` feature | IDLE, CONDSTORE and UID search without writing a protocol stack |
| `rustls` + platform verifier | No OpenSSL dependency, OS trust store on both platforms |
| Accounts in the database | Single source of truth, atomic edits |
| Tokio broadcast for change events | One mechanism for local edits and network updates |
| Decoded attachment bytes in SQLite | The body fetch already downloads every MIME part; a second round trip per attachment would be slower and fragile offline |
| Locked-down WebKit/WebView2 + shared CSP + opt-in remote content | HTML mail is hostile input; privacy is the default on both front-ends |
| `GNotification` instead of a notification crate | Uses the platform notification channel and the Flatpak portal without another dependency |
| No `Content-Transfer-Encoding` shortcuts | Bodies are parsed with `mail-parser` in M2; never store raw MIME as text |
| `windows-bindgen --minimal` with a checked-in filter | The full XAML projection is ~43 MB; the client needs a small set of controls, collections and WebView2 types |
| XAML composed in Rust, no `.xaml` files | Cargo has no XAML compiler; a generated projection keeps the build self-contained |
| Load `MddBootstrapInitialize2` with `LoadLibraryW` | The App SDK bootstrapper is a C export, not WinRT metadata, so bindgen cannot project it |
| Bootstrap the framework package, not self-contained deployment | Fastest way to prove the runtime path; self-contained packaging belongs to M7 |

## Milestone 7: Packaging, Hardening, Accessibility, Performance

### Packaging Strategy

Windows artifacts are produced by `cargo xtask package-win` and the `release.yml` workflow:

1. **Self-contained zip** — `mailin-winui-<version>-x86_64.zip`
   - Contains the executable, the App SDK bootstrap DLL (`Microsoft.WindowsAppRuntime.Bootstrap.dll`), and the Windows App SDK 1.8 runtime installer (`WindowsAppRuntimeInstall-x64.exe`).
   - The bootstrap DLL must be next to the executable at runtime; the framework package is attached via `MddBootstrapInitialize2`.
   - Intended for portable use or environments where sideloading/installers are not an option.

2. **Unsigned MSIX** — `mailin-<version>-x86_64.msix`
   - Built from `packaging/windows/AppxManifest.xml` using `makeappx`.
   - Declares `runFullTrust` capability (required for the App SDK bootstrapper) and network capabilities.
   - Installed via `Add-AppxPackage`; SmartScreen warns because the package is unsigned.

3. **Inno Setup installer** — `mailin-setup-<version>.exe`
   - Script at `packaging/windows/setup.iss`.
   - Installs to `%LOCALAPPDATA%\Mailin`, creates Start Menu entry, optional desktop shortcut.
   - Runs the Windows App SDK runtime installer silently on first install.

All artifacts are attached to GitHub Releases with SHA-256 checksums. Binary size is reported as a CI guard.

### Hardening

- **cargo-deny** (`deny.toml`): RustSec advisory scanning, license allowlist (MIT/Apache/BSD/ISC/MPL-2.0), duplicate detection.
- **Secrets audit**: Passwords are `SecretString` end-to-end (`secrecy` crate). The `CredentialsStore` trait uses `SecretString`; `KeyringCredentials` converts to/from plain text only at the OS keyring boundary. Raw `String` handling eliminated in credential paths.
- **HTML policy edge-case tests**: `data:`/`cid:` rewriting with angle brackets, whitespace, case insensitivity; remote-content detector covers `srcset`, `<link>`, `<video>/<audio>`, `<iframe>`, `<form>`, CSS `url()` with quotes, protocol-relative URLs; CSP document invariants (nonce uniqueness, `form-action 'none'`, `frame-src 'none'`, `base-uri 'none'`, `style-src 'unsafe-inline'`, `font-src data:`).

### Accessibility

**GTK** (`mailin-gtk/src/window.rs`):
- Icon-only buttons get `accessible_name` via `gtk::AccessibleExt::set_accessible_name`.
- Message list keyboard navigation: `Up`/`Down` arrows move selection via `EventControllerKey`.
- `GtkShortcutsWindow` with `ShortcutsSection` + `ShortcutsShortcut` entries for all global shortcuts (F1 / Ctrl+? to open).
- Wizard focus handling: initial focus on the first field; tab order follows visual layout.

**WinUI** (`mailin-winui/src/window.rs`):
- `KeyboardAccelerator` objects on the main window for global shortcuts:
  - `Ctrl+F` → search
  - `F5` → synchronize
  - `Ctrl+Shift+A` → add account
  - `Ctrl+Enter` → send (in composer)
- Accelerators use `VirtualKey` + `VirtualKeyModifiers` and invoke the same handlers as the toolbar buttons.

### Sync Deltas (CONDSTORE CHANGEDSINCE)

Incremental sync now uses `UID SEARCH CHANGEDSINCE <modseq>` when the server advertises CONDSTORE and we have a stored `highest_modseq` for the folder. This returns only UIDs whose flags changed since the last sync, avoiding a full `UID FETCH 1:* (FLAGS)` on large folders.

- `ImapSession::fetch_flags_for_uids` fetches `(UID, FLAGS)` for a specific UID set.
- On CHANGEDSINCE failure (server doesn't support the extension despite advertising CONDSTORE, or modseq stale), the code falls back to the existing full flag refresh path.
- The `highest_modseq` is persisted in `FolderSyncState` and updated on every successful sync.
