# Architecture

## Goals

Mailin is a native mail client, not a web view in a window. The same Rust core
powers two front-ends that use each platform's real toolkit:

| Platform | Toolkit | HTML rendering |
|---|---|---|
| Linux | GTK 4 via `gtk4-rs`, libadwaita for layout and styling | WebKitGTK 6 (`webkit6`) |
| Windows | WinUI 3 (Windows App SDK) | WebView2 (`webview2-com`) |

Both front-ends are thin: they render state, forward user intent, and never talk
to the network themselves. Everything protocol related lives in `mailin-core`.

## Crate graph

```
mailin-gtk ─┐
            ├──> mailin-core ──> IMAP / SMTP servers
mailin-winui┘        │
                     └──> SQLite (local cache) + OS keyring
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
   front-end will relax this per crate when it needs COM/WinRT interop.

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
  separately (planned: `windows-bindgen` against the App SDK winmd files, output
  committed as `mailin-winui-sys`).
- A pure Cargo build has **no XAML compiler**, so there are no `.xaml` files in
  the build. UI is composed in Rust; small declarative fragments can be parsed
  at runtime with `XamlReader::Load`.
- The App SDK runtime must be deployed: either installed system wide or copied
  next to the executable (self-contained, ~60 MB). The app manifest is embedded
  by `build.rs`, and `resources.pri` is owned by the App SDK.
- WinUI requires an STA thread with a `DispatcherQueue`. Everything that touches
  XAML objects is confined to it.
- Consequence: Windows builds, runtime loading and packaging are verified in CI
  (`windows-latest`); the Linux development machine can only `cargo check` the
  Windows target.

Milestone 5 is deliberately a short spike with a go/no-go decision. If the
generated projection proves unusable, the fallback is a WinUI 3 shell in C#
over the same core through a C ABI, or a native Win32 shell — the core and the
GTK client are unaffected either way.

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
| Locked-down WebKit + CSP + opt-in remote content | HTML mail is hostile input; privacy is the default |
| `GNotification` instead of a notification crate | Uses the platform notification channel and the Flatpak portal without another dependency |
| No `Content-Transfer-Encoding` shortcuts | Bodies are parsed with `mail-parser` in M2; never store raw MIME as text |
