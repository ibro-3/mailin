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
| `attachments` | MIME part metadata per message |
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

Per account there is one supervised worker; folder work is bounded (≤4 in
flight). For each folder:

1. `SELECT` the mailbox and read `UIDVALIDITY`, `UIDNEXT`, `HIGHESTMODSEQ`.
2. If `UIDVALIDITY` changed, discard the folder's cached messages and start over
   — UIDs are meaningless under a new validity.
3. Otherwise fetch `UID > last_uid` with `ENVELOPE`, `FLAGS`, `INTERNALDATE`,
   `RFC822.SIZE` and `BODYSTRUCTURE`, and persist the page in one transaction.
4. If the server advertises CONDSTORE, fetch flag changes with
   `CHANGEDSINCE <highest_modseq>` instead of refetching flags for every UID.
5. Watch the inbox and the open folder with `IDLE`; fall back to polling every
   `poll_interval_secs` when IDLE is unavailable or the connection drops.

Bodies are fetched with `BODY.PEEK[]` when a message is opened (or eagerly when
`sync.body_sync = "eager"`) and cached; a body is never fetched twice.

Reconnects use exponential backoff with jitter. Every network operation is
cancellable, so disabling an account or quitting the app does not wait for a
timeout.

### Sending

Compose builds an RFC 5322 message, stores it in `outbox`, and the sender worker:

1. claims the oldest queued entry,
2. submits it over SMTP (implicit TLS or STARTTLS, `lettre` with rustls),
3. appends a copy to the Sent folder,
4. marks the entry sent, or records the error and retries up to five times.

The outbox survives restarts, so a crash or network outage cannot lose a draft
that the user already "sent".

## Threading and UI integration

```
tokio runtime (core workers)          UI main loop
        │                                  │
        ├── Store (mutex) ─────────────────┤ reads
        │                                  │
        └── broadcast::Sender<Event> ──> marshal to main loop ──> widgets
```

- **GTK:** a `glib::MainContext` idle source drains the event receiver; tokio
  runs on its own runtime. UI code never blocks on network work.
- **WinUI:** an `async_channel` bridge posts to the `DispatcherQueue`; all
  widget mutation happens on the STA UI thread.

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
| Front-ends | Pure logic extracted into testable modules; UI smoke tests per platform |
| Windows | `windows-latest` CI: clippy, tests, release build, artifact upload |

## Decisions

| Decision | Rationale |
|---|---|
| SQLite via `rusqlite` with `bundled` | Same engine and version on every platform, FTS5 guaranteed |
| `async-imap` with the `runtime-tokio` feature | IDLE, CONDSTORE and UID search without writing a protocol stack |
| `rustls` + platform verifier | No OpenSSL dependency, OS trust store on both platforms |
| Accounts in the database | Single source of truth, atomic edits |
| Tokio broadcast for change events | One mechanism for local edits and network updates |
| No `Content-Transfer-Encoding` shortcuts | Bodies are parsed with `mail-parser` in M2; never store raw MIME as text |
