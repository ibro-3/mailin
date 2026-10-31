-- PGP/GPG key storage for encryption and signing.
-- Public keys cached per-recipient; secret keys stored in OS keyring via CredentialsStore.

CREATE TABLE pgp_keys (
    id              INTEGER PRIMARY KEY,
    account_id      TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    fingerprint     TEXT NOT NULL,
    user_ids        TEXT NOT NULL DEFAULT '',  -- JSON array of user IDs
    public_key      TEXT NOT NULL,             -- Armored public key
    created_at      INTEGER NOT NULL,
    UNIQUE (account_id, fingerprint)
) STRICT;

CREATE INDEX idx_pgp_keys_account ON pgp_keys(account_id);
CREATE INDEX idx_pgp_keys_fingerprint ON pgp_keys(fingerprint);