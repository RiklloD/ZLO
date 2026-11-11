# Secrets encryption-at-rest: the `EncryptedBackend` contract question

Status: open, needs a decision before implementation (2026-11-11).

## Context

`crates/substrates/ironclaw_filesystem/CONTRACT.md` invariant 6 says
encryption-at-rest is a backend decorator: `EncryptedBackend` wraps an inner
backend, encrypts `Entry::body` plus any `IndexValue::Bytes` projection, and
`SecretStore` writes plaintext. Today `SecretStore` and `CredentialBroker`
(`crates/substrates/ironclaw_secrets/src/secret_store.rs`) encrypt inline with
`SecretsCrypto` (AES-256-GCM, a per-record HKDF salt, AAD from
`filesystem_secret_aad` / `credential_account_aad` / `credential_session_aad`).

## Why it cannot be built "within the contract" today

1. **AAD source.** Existing ciphertext is bound to an AAD built from the
   *domain tuple*: domain tag plus length-prefixed raw `tenant_id`, `user_id`,
   `agent_id`, `project_id` (and mission/thread/invocation for sessions) plus
   handle/id. A decorator only sees a `VirtualPath`. The path does not carry
   the raw ids: the reserved system tenant `"\x1fSYSTEM\x1f"` maps to the path
   segment `__system__`, and the mount view (not the record) decides the
   `/tenants/<t>/users/<u>` prefix. So a path-derived AAD cannot reproduce
   any existing AAD, and old ciphertext cannot be decrypted by a generic
   decorator.
2. **Body shape.** Only fields are encrypted (`StoredSecret.encrypted_value`,
   `StoredAccount.encrypted_payload`, `StoredSession.encrypted_payload`).
   Scope, handle, status, expiry and `uses` are plaintext JSON in the same
   body. A body-encrypting decorator produces a different on-disk format, so
   "existing bytes stay compatible" is impossible without the decorator
   understanding these secret-specific schemas, which the filesystem crate
   must not own.
3. **Key custody and dependency direction.** The master key, HKDF and the
   keychain live in `ironclaw_secrets`, which depends on `ironclaw_filesystem`.
   The decorator cannot call `SecretsCrypto` without a reverse edge (a cycle),
   and the filesystem crate's charter limits its workspace dependencies.

## Questions to decide

1. **AAD binding.** Should decorator ciphertext be bound to the resolved
   `VirtualPath` (plus a mount/domain tag)? Or must the decorator accept a
   per-write AAD/context from the consumer, for example a new `Entry` field
   or a `ScopedFilesystem` hook, which keeps `(scope, handle)` bindings but
   widens the trait surface?
2. **Legacy data.** Pick one:
   (a) a one-way, explicit re-encryption migration to the decorator format.
   This breaks rollback: old binaries cannot read migrated rows.
   (b) a versioned envelope: the decorator passes untagged legacy bodies
   through and the store keeps a read-only legacy decrypt path
   indefinitely. That contradicts "stores never own encryption code".
   (c) dual-read for a fixed window, then (a).
3. **Cipher seam.** Should `ironclaw_filesystem` define a `BodyCipher`-style
   trait that `ironclaw_secrets` implements and composition injects, keeping
   crypto dependencies out of the filesystem crate? Or does the decorator
   own AES-GCM/HKDF itself, with the key passed in?
4. **Plaintext metadata.** Should `status`, `uses`, expiry and scope stay
   readable without the key? (Today they do: lease bodies are plaintext, and
   secret metadata listing reads them.) Under whole-body encryption, every
   metadata listing decrypts every row.
5. **Scope of the mount.** Does the decorator wrap all of `/secrets`, which
   includes leases (no secret material today) and migration markers, or only
   the record families that hold material?

## Recommendation (for the deciders)

Take 1 = consumer-supplied AAD context, 2 = (b) with a versioned envelope tag,
and 3 = a cipher trait implemented in `ironclaw_secrets`. That is the only
combination that keeps existing ciphertext decryptable and rollback safe
without the filesystem crate learning secret schemas. It still leaves a legacy
decrypt path in the store until a later explicit migration retires it.
