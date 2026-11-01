-- OpenPGP/MIME payloads extracted while parsing a message.
-- Keeping them lets the reader decrypt or verify on demand without re-fetching.
CREATE TABLE pgp_mime (
    message_id INTEGER PRIMARY KEY REFERENCES messages(id) ON DELETE CASCADE,
    -- 'encrypted' (armored ciphertext) or 'signed' (the signed MIME entity)
    kind       TEXT NOT NULL,
    -- Encrypted: armored ciphertext. Signed: the exact bytes covered by the signature.
    payload    TEXT NOT NULL,
    -- Armored detached signature for 'signed' payloads.
    signature  TEXT,
    fetched_at INTEGER NOT NULL
) STRICT;
