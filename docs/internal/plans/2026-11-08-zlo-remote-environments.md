# ZLO remote environments: t3code's architecture on Tailscale

Status (2026-11-08): phases 0–5, 7 and 8 (SSH route, Light bundle) are
built, uncommitted on `rune-port`. Phase 6 is covered only partly (every
connect reloads from the backend; per-connection tags drop stale replies;
chat streams are closed on a switch rather than tagged). "Update all
machines" is built (phase 8, "Self-update" in the wire contract); not yet run
against a second physical machine. Remote file `@`-mentions and the backend's computer-use readiness are
built (phase 7 rows below). Remote git and terminal are built (phase 7,
"Git and console on the backend" below). Not built: supervisors for inactive
computers, the `tailscale whois` cross-check, a terminal QR.

Live check on the dev Mac (2026-11-08, throwaway backend, real Tailscale,
`--https-port 8443`): `ironclaw remote enable` served
`https://<mac>.ts.net:8443 → 127.0.0.1:3001`; ZLO's own client code
(`zlo_client::pairing`) paired in one click over the tailnet, minted a code
with the device token, paired a second device with it, got `invalid_code`
on reuse, revoked it (401), and the revocation held across a backend
restart. A forged `Tailscale-User-Login` on the main listener got 404;
an unauthenticated tailnet request got 401. `remote disable` left
`tailscale serve status` empty. Not yet run: a second physical machine,
and the SSH route against a real host.

## Goal

Any machine can play any role:

| Role | Runs | Example |
|---|---|---|
| Backend only | `ironclaw serve` as a login service, no app | Mac mini, Linux box, VPS |
| Both | ZLO plus the backend it starts | your main Mac (today's setup) |
| App only | ZLO with no local backend, connected to a paired machine | laptop |

Phones are out of scope here: they get a dedicated app later, built on the
same pairing and device-token contract.

The design copies T3 Code's remote-environment architecture as closely as
possible (reference checkout: `~/.hermes/dev/t3-reference`; the server
`auth/` code and `infra/relay` are only in its git HEAD, not the working
tree). The one deliberate change: T3 Connect, its paid Cloudflare relay
(Workers, Durable Objects, Clerk, Postgres, one Cloudflare Tunnel per
environment), is replaced by Tailscale. Everything must be free to run and
stay light: Rust on both ends, HTTP + SSE as today, no new hosted service.

## What we copy from T3 Code, and what replaces the relay

Kept as T3 Code does it (paths are in the T3 reference checkout):

- **Environment** = one backend installation with a permanent id, not an
  address (`apps/server/src/environment/ServerEnvironment.ts`). One
  environment has several **routes** (`packages/client-runtime/src/connection/catalog.ts`).
- **Public descriptor** (`/.well-known/t3/environment`) read before pairing:
  id, name, protocol version, capabilities. A wrong machine is rejected
  before the one-time code is spent (`connection/onboarding.ts:111-154`).
- **One-time pairing code**, 5 minutes, consumed atomically
  (`auth/PairingGrantStore.ts`), exchanged for a **per-device token** that is
  revocable and survives restarts (`auth/SessionStore.ts`).
- **Connection supervisor** per environment: transient failures retry with
  jittered backoff, blocked ones (auth, version) do not
  (`connection/supervisor.ts`, `connection/model.ts`).
- **Protocol check** with "update the server" vs "update the app", and an
  outdated host stays saved (`connection/compatibility.ts`,
  `connection/outdatedHostUpdate.ts`).
- **Snapshot, then changes after it**, refetched after every reconnect;
  "connected" and "caught up" are different states (`state/threads.ts`,
  `state/shell.ts`).
- **Environment-scoped keys** for every thread, project, draft and cache
  (`environment/scoped.ts`).
- **Local-only features become backend operations**: attachments upload as
  bytes, files and previews are read through the backend, terminals and git
  run on the backend, "open in editor" becomes an SSH link.

Replaced:

| T3 Connect job | T3 Code (paid) | Ours (free) |
|---|---|---|
| Reach the backend with no open ports | Cloudflare Tunnel + `cloudflared` | `tailscale serve`, backend stays on loopback |
| Hostname per machine | relay-reserved subdomain | MagicDNS `machine.tailnet.ts.net`, with a real HTTPS certificate |
| Account owns machines | Clerk + signed environment link | the user's tailnet |
| List my machines | relay environment list | tailnet peer list, then probe each peer's descriptor |
| Sign in without a code | relay brokers a 2-minute credential | backend sees the caller's tailnet login; owner match issues a device token |
| Offline state | relay health + `tunnel_released` | peer `Online` flag in the tailnet status |
| Confidentiality | Cloudflare decrypts at its edge | WireGuard end to end |
| Cost | Workers, DOs, Postgres, tunnels | none (free personal plan, or self-hosted Headscale) |

Deliberately not copied: WebSocket RPC (we keep HTTP + SSE with bearer
headers, so there are no replayable WS tickets), tokens in IndexedDB (we use
the macOS Keychain), raw pairing codes in the database (we store a hash),
push notifications through a relay (later, if ever).

## What exists today

- ZLO pairs one remote computer from a link `http(s)://host:port#token=…`,
  saved in `~/Library/Application Support/ZLO/gateway.json` (0600)
  (`tools/ironclaw_rune/app/src/onboarding/gateway.rs`).
- The link carries the backend's **master `webui-token`**: full operator
  authority, no expiry, no per-device revocation.
- The client (`tools/ironclaw_rune/client`) already speaks bearer HTTP to any
  base URL, and its SSE stream resumes with `Last-Event-ID`, backs off and
  detects dead connections (`client/src/stream.rs`).
- The backend has signed session tokens (`SignedTokenSessionStore`), but
  their revocation list is in memory and clears on restart
  (`crates/product/ironclaw_webui/src/signed_session_login.rs`).
- `ironclaw serve` refuses a non-loopback listener when the runtime gives
  the agent full access to the host
  (`crates/app/ironclaw_cli/src/commands/serve.rs`,
  `reject_non_loopback_privileged_local_runtime`). This stays.
- `ironclaw service` installs launchd/systemd units
  (`crates/app/ironclaw_cli/src/commands/service/`).
- Backend file endpoints already exist: project files, filesystem browse and
  read (`crates/product/ironclaw_webui/src/webui_v2/handlers.rs`,
  `list_project_files`, `browse_fs_dir`, `read_fs_file`).

Known problems in ZLO when the backend is remote:

- `service.rs` starts the local backend unless `IRONCLAW_DESKTOP_URL` is
  set; a paired computer does not stop it.
- An unreadable `gateway.json` silently falls back to This Mac.
- Path attachments send a client path that the backend opens on its own disk
  (`client/src/attachments.rs`).
- Project tree, console, git, preview and open run on the local Mac
  (`app/src/chat/side/{project,console,git,preview}.rs`,
  `app/src/chat/actions.rs`).
- A computer is keyed by its address; async results and drafts are not
  scoped to a computer (`app/src/jobs.rs`, `app/src/drafts.rs`).

## Wire contract v1 (binding for backend and ZLO)

Everything below is JSON over the existing WebUI listener. Times are
RFC 3339 UTC strings. Error bodies are `{"error": "<code>", "message": "<text>"}`.

### Descriptor (public, no auth)

`GET /.well-known/ironclaw/environment` → 200

```json
{
  "environment_id": "0f6c2d0e-3b0a-4a51-9b7e-6b1c8f1f2a10",
  "label": "Studio",
  "server_version": "0.41.0",
  "protocol_version": 1,
  "pairing": ["one_time_code", "tailnet_identity"],
  "tailnet_url": "https://studio.tail9915dd.ts.net",
  "build_commit": "94172fa26c1f0e5d4b3a2918f7e6d5c4b3a29180",
  "self_update": true
}
```

- `environment_id`: UUID created on first start, never changes.
- `label`: host name by default.
- `pairing`: `one_time_code` always; `tailnet_identity` only when
  `[remote] enabled` and one-click pairing is allowed (no other local
  accounts, or `trust_local_users`).
- `tailnet_url`: the `tailscale serve` URL when remote is enabled, else
  `null`.
- `build_commit`: the full commit the backend binary was built from
  (stamped by `crates/app/ironclaw_cli/build.rs`), `null` when the build
  had no git checkout. Additive: absent from older backends.
- `self_update`: `[update] method = "source"` is set up there. Additive:
  `false` when absent.
- Nothing else (no paths, no user names). Rate-limited like other public
  routes.

### Pairing codes (operator)

`POST /api/auth/pairing-codes` (bearer: master token or a device token;
both are operator) → 200

```json
{ "code": "7KQ2-M9XD-4TRA", "expires_at": "2026-11-08T21:05:00Z" }
```

- 12 Crockford base32 characters from a CSPRNG, shown as three groups of
  four. Redemption accepts any case, with or without dashes.
- Valid 5 minutes, single use. Stored as SHA-256 only.

### Redeem a code (public)

`POST /api/auth/pair`

```json
{ "code": "7KQ2-M9XD-4TRA",
  "client": { "label": "Lorik's MacBook Air", "os": "macOS", "app_version": "0.1.0" } }
```

→ 200 `{"environment_id": "…", "device_id": "…", "token": "icd_<device_id>_<secret>"}`

→ 400 `invalid_code` for unknown, expired, used or malformed codes (one code
for all, no oracle). → 429 when rate-limited (strict: a handful per minute
per peer).

- `device_id`: 32 lowercase hex. `secret`: 32 random bytes, base64url, no
  padding. Only SHA-256 of the whole token is stored.
- Two concurrent redemptions of one code: exactly one succeeds.

### One-click tailnet pairing (public, identity listener only)

`POST /api/auth/pair/tailnet` with the same `client` object (no code) →
200 same body as `/api/auth/pair`.

- Honored only on the identity listener (the `tailscale serve` target).
  The caller's login is the `Tailscale-User-Login` header, which only the
  identity listener reads; the normal listener strips all `Tailscale-*`
  headers.
- Login in `[remote] owner_logins` → token, `paired_by: tailnet_identity`.
- Otherwise 403 `not_owner` (ask the owner for a code), or 403
  `tailnet_pairing_disabled` when one-click is off. On the normal
  listener: 404.

### Devices (operator)

`GET /api/auth/devices` → 200

```json
{ "devices": [ {
  "device_id": "…", "label": "Lorik's MacBook Air", "os": "macOS",
  "app_version": "0.1.0", "paired_by": "one_time_code",
  "created_at": "…", "last_seen_at": "…", "revoked_at": null,
  "current": true } ] }
```

`paired_by`: `one_time_code` | `tailnet_identity`. `current`: the request's
own device token. Revoked devices stay listed with `revoked_at` set.

`DELETE /api/auth/devices/{device_id}` → 204, idempotent. A revoked token
gets 401 on every route, including after a backend restart.

### Device token authority

`Authorization: Bearer icd_…` authenticates as the operator, exactly like
the master token (decision 1), with `device_id` recorded for audit and
`last_seen_at` (updated at most once a minute). The master token keeps
working for the local ZLO and the CLI.

### Self-update (operator)

`[update]` in the backend's `config.toml` (default `method = "none"`):

```toml
[update]
method = "source"
repo = "/Users/me/ironclaw"   # absolute; the checkout this binary is built from
branch = "rune-port"          # optional; default: the checkout's current branch
```

`GET /api/operator/update` → 200

```json
{ "method": "source",
  "current": { "version": "1.2.0", "commit": "aaaa…" },
  "available": { "commit": "bbbb…", "behind": 3,
                 "subjects": ["…", "…", "…"], "checked_at": "…" },
  "last_run": { "state": "succeeded", "started_at": "…", "finished_at": "…",
                "message": "Updated to bbbbbbbbb; IronClaw restarted.",
                "log_tail": "…" } }
```

- `available`: from the last check; `null` before one, or when the running
  build is the upstream tip. At most 20 subjects, 200 characters each.
- `last_run.state`: `idle | running | succeeded | failed`. A `running`
  record whose job no longer holds the lock (30 s after it started) reads
  as `failed`. `log_tail`: the last 40 lines of the job log.

`POST /api/operator/update/check` → 200 same body after `git fetch`;
`409 not_configured`; `502 check_failed` with the reason in `message`.

`POST /api/operator/update` → 202 same body, `last_run.state = running`;
`409 update_running` while a job holds the lock; `409 not_configured`.

- All three are operator-only and take **no body** (`413` if one is sent):
  repository, branch and build steps come only from local config.
- Only fast-forwards: a dirty checkout (tracked changes) or one with commits
  the upstream lacks is refused with a message; the configured branch must
  be the one checked out.
- The job runs detached from the backend: the backend copies its binary to
  `<reborn_home>/update/ironclaw-updater` and starts
  `ironclaw-updater update --run-job --install-path <its own path>` in a new
  process group (Linux under systemd: its own `systemd-run --user` unit),
  output appended to `update.log`. It holds `update.lock` (an OS file lock)
  for its life and writes `run.json`, so the restarted backend reports the
  outcome.
- Rebuild by install: the `zlo-backend` inside a `*.app` runs the repo's
  `tools/ironclaw_rune/bundle.sh` (installs ZLO.app, reopens it, ZLO starts
  the new backend); any other binary is rebuilt with
  `cargo build --release -p ironclaw`, replaced atomically, and restarted
  through `ironclaw service` when installed (else the message says to
  restart `ironclaw serve`).
- CLI: `ironclaw update [--check] [--json]` runs the same check / job in the
  foreground.

### `[remote]` config and the identity listener

```toml
[remote]
enabled = true
identity_port = 3001          # loopback port that `tailscale serve` targets
owner_logins = ["rikllodal@gmail.com"]
trust_local_users = false
tailnet_url = "https://loriks-macbook-pro.tail9915dd.ts.net"
https_port = 443
```

When `enabled`, `ironclaw serve` also binds `127.0.0.1:<identity_port>`
with the same router and middleware, plus the identity extension. The
loopback-only rule for the main listener is unchanged.

### CLI

```
ironclaw remote enable [--https-port 443] [--trust-local-users] [--json]
ironclaw remote status [--json]
ironclaw remote disable
ironclaw pair [--json]              # prints code, expiry, link, QR
ironclaw devices [--json]
ironclaw devices revoke <device_id|label> | --all
```

- `remote enable`: find Tailscale (`tailscale` on `PATH`, else
  `/Applications/Tailscale.app/Contents/MacOS/Tailscale`), require
  `BackendState == "Running"`, refuse if Funnel is on for the chosen port
  or the port is served by something else, record `owner_logins` from
  `Self.UserID` → `User[id].LoginName`, detect other local login accounts,
  write `[remote]`, run
  `tailscale serve --bg --https=<https_port> http://127.0.0.1:<identity_port>`.
  `--json` prints `{"tailnet_url", "one_click": bool, "restart_required": bool}`.
- `remote disable`: `tailscale serve --https=<https_port> off` (never
  `serve reset`), set `enabled = false`.
- `pair`: link is `<tailnet_url or listener origin>/pair#code=<code>`.
  `--json` prints `{"code", "expires_at", "link"}`.
- These talk to the running backend over loopback with the master token
  where they need it (`pair`, `devices`), or edit config + run Tailscale
  (`remote`).

### ZLO side

- Saved computers in `~/Library/Application Support/ZLO/environments.json`
  (no secrets): `{"run_local_backend": true, "active": "<environment_id|this-mac>",
  "environments": [{"environment_id", "label", "routes": [{"kind": "tailnet"|"direct", "url"}]}]}`.
- Device tokens in the macOS Keychain: service `ZLO`, account
  `<environment_id>`.
- Pairing links accepted: `https://host/pair#code=XXXX-XXXX-XXXX`, a bare
  code with a chosen computer, and the legacy `…#token=` form for one
  release (saved as-is, with a "re-pair for a device token" note).
- Every connect reads the descriptor first: id must match the saved one;
  `protocol_version != 1` blocks with "update ZLO" or "update IronClaw on
  <label>".

## Phases

Each phase ships on its own and leaves the product working. Order matters:
phase 1 is app-only and unblocks the laptop case right away; phase 3 removes
the master token from the wire.

### Phase 0: verify the Tailscale assumptions (done 2026-11-08)

Run on the dev Mac: Tailscale 1.104.1, the sandboxed macOS app
(`com.apple.security.app-sandbox`, system extension
`io.tailscale.ipn.macsys.network-extension` running as root). Only this
node was online, so a second machine has not been tested yet.

1. **Unix socket target: does not work on macOS.** `tailscale serve
   unix:/path` is accepted and listed, but every request returns 502, both
   for a socket under `~/.ironclaw/reborn/` and under `/tmp`; the socket
   itself answers a direct `curl --unix-socket`. The sandboxed extension
   cannot reach it. (Linux `tailscaled` likely can; not tested.) **Decision:
   the serve target is a dedicated loopback TCP port.**
2. **Identity headers: present and not forgeable through the proxy.** A
   request through `tailscale serve` to a loopback port arrives with
   `Tailscale-User-Login`, `Tailscale-User-Name`,
   `Tailscale-User-Profile-Pic`, `Tailscale-Headers-Info`,
   `X-Forwarded-For` (the caller's tailnet IP), `X-Forwarded-Host`,
   `X-Forwarded-Proto: https`, and `Host` = the MagicDNS name and port. A
   client that sends its own `Tailscale-User-Login: evil@example.com`
   gets it replaced with the real login. Still to test with a second node:
   tagged and shared-node callers (expected: no user headers).
3. **`tailscale whois <ip>` works** and returns machine name, node id,
   addresses and user login. Usable as a cross-check of `X-Forwarded-For`.
4. **CLI path**: not on `PATH` on macOS; it is
   `/Applications/Tailscale.app/Contents/MacOS/Tailscale` (the app's own
   binary acts as the CLI). Look up `tailscale` on `PATH` first, then that
   path. `status --json` gives `Self.{DNSName, HostName, OS, Online,
   UserID, TailscaleIPs}`, `Peer{}.{DNSName, OS, Online, UserID}` (DNS
   names end with a dot), `User{}.LoginName`, `MagicDNSSuffix`,
   `CertDomains`. `serve status` / `funnel status` print `No serve config`
   when empty; `serve reset` clears everything (so `remote disable` must
   remove only our own port, `serve --https=<port> off`, never `reset`).
5. **Proxied connections are invisible to the backend's user.** `lsof` run
   as the backend's user does not see the peer of a connection coming from
   the root-owned extension, while it does see a same-user local client.
   That cannot tell the extension apart from another OS user's process, so
   it is not a usable check (see the multi-user rule in phase 4).

Open for a second node: a real remote caller's headers and
`X-Forwarded-For`, latency through the tailnet, and the Linux socket
target.

### Phase 1: app-only mode in ZLO (Rune only)

Goal: a ZLO pointed at a remote never touches the local machine as if it
were the backend.

- Settings → Computers gets **Run IronClaw on this Mac** (on by default).
  Off, or a remote selected: `service.rs` never starts, restarts or stops a
  backend. The decision reads the selected computer before `service::start()`
  in `app/src/main.rs` and the retry path in `app/src/workspace.rs`.
- An unreadable or missing saved remote shows a connection problem; never a
  silent fallback to This Mac (`onboarding/gateway.rs`).
- While remote: console, git stage/discard/restore, path attachments,
  local preview and "open" are disabled with a one-line reason. Screenshots
  and voice stay (they are client-side by nature).
- `bundle.sh` gains a Light bundle without `zlo-backend` (packaging only,
  no cargo feature); Light starts with the switch off and hidden.

Tests: crate-tier tests in the Rune app for the start decision and for the
disabled actions; a fixture for the remote state.

Exit: launching and retrying against a remote never spawns a process;
nothing local is mutated from a remote thread.

### Phase 2: environment identity and descriptor (backend)

Status (2026-11-08): backend built. Environment id + label live in
`ironclaw_identity::environment_access` (`/tenant-shared/reborn-environment/
environment.json` on the control-plane mount, created on first `serve`);
`EnvironmentId` is a validated newtype in
`ironclaw_product_contracts::environment_access` rather than
`ironclaw_host_api` (one owner, no new host_api vocabulary). The descriptor
ships the wire-contract-v1 shape (no `capabilities` object yet). Route:
`ironclaw_webui::environment_access_route_mount`. ZLO side pending.

- Persistent `EnvironmentId` (UUID) created on first start and kept through
  `RootFilesystem` in the operator's mount, plus a display name (default:
  the host name, editable).
  - Type: `ironclaw_host_api` (neutral id).
  - Descriptor contract: `ironclaw_product_contracts` (new `environment` module).
  - Service: `ironclaw_operator` (or the crate that owns installation
    metadata; confirm with `crates/AGENTS.md`).
- `GET /.well-known/ironclaw/environment`, public, minimal:

  ```json
  {
    "environment_id": "6f1c…",
    "label": "Studio",
    "server_version": "0.x.y",
    "protocol_version": 1,
    "pairing": ["one_time_code", "tailnet_identity"],
    "capabilities": { "files": true, "git": false, "terminal": false, "self_update": false }
  }
  ```

  Machine details beyond this only after auth. Route descriptor, body and
  rate limits through the existing `webui_v2` descriptor machinery.
- ZLO reads the descriptor before pairing and on every connect: wrong id →
  "this address reaches a different machine"; protocol mismatch → blocked,
  with which side to update.

Tests: integration test through the harness for the endpoint (public,
rate-limited, no secrets in the body); client unit tests for the
compatibility decision.

### Phase 3: device pairing and device tokens (backend + CLI + ZLO)

Status (2026-11-08): backend + CLI built. Records and the single-use CAS in
`ironclaw_identity::environment_access` behind the
`EnvironmentAccessService` port (`ironclaw_product_contracts`), opened by
`RebornRuntime::open_environment_access`; routes and
`DeviceTokenAuthenticator` in `ironclaw_webui`; `ironclaw pair` /
`ironclaw devices [revoke]` in the CLI (no QR: no QR crate in the tree).
`POST /api/auth/pairing-codes` returns `{code, expires_at}`; the CLI builds
the link. `local_operator` pairing is not implemented. ZLO side pending.

Backend (`ironclaw_webui` owns host auth; durable records via a host-auth
port implemented in composition, stored with `RootFilesystem` and the
bounded CAS helper; amend `ironclaw_webui/CONTRACT.md` and the identity
contract if records live there; run `ironclaw_architecture_tests`):

- **Pairing code**: 12 characters, Crockford base32, CSPRNG with rejection
  sampling, 5-minute expiry, stored as SHA-256 only. Consumption is one CAS
  write that checks expiry, revocation and prior use.
- **Device grant**:

  ```
  device_id, label, os, app_version,
  token_hash, created_at, last_seen_at, expires_at?, revoked_at?,
  paired_by: one_time_code | tailnet_identity | local_operator,
  tailnet_login?
  ```

  Token format `icd_<device_id>_<secret>`: lookup by id, constant-time
  compare of the hash. Revocation sets `revoked_at` and survives restarts.
- **Authority** (decided): a device token maps to the operator caller, as
  the master token does today, with `device_id` carried in the caller's
  provenance for audit and `last_seen_at`. Narrower per-device scopes
  (T3's `orchestration:*`, `terminal:*`, `filesystem:*`) can come later
  behind the same record.
- **Endpoints** (descriptors, body and rate limits as usual):
  - `POST /api/auth/pairing-codes` (operator) → `{code, expires_at, link}`
  - `POST /api/auth/pair` (public, strict rate limit) `{code, client}` →
    `{environment_id, device_id, token}`
  - `GET /api/auth/devices`, `DELETE /api/auth/devices/{id}` (operator)
- `DeviceTokenAuthenticator` sits beside `EnvBearerAuthenticator`; the
  master token keeps working for the local ZLO and the CLI.

CLI (`crates/app/ironclaw_cli`):

```
ironclaw pair [--json]          # code + link + terminal QR, 5 minutes
ironclaw devices                # list
ironclaw devices revoke <name|id>
ironclaw devices revoke --all
```

ZLO:

- Pairing link becomes `https://studio.tailnet.ts.net/pair#code=…`; the old
  `#token=` form is still accepted for one release, with a notice.
- Token stored in the Keychain; `gateway.json` keeps only non-secret data.
  Existing `gateway.json` tokens are migrated once, then removed.
- Settings → Computers → this computer → **Paired devices**, with revoke
  (calls the backend; deleting locally is not a revocation).

Tests: integration tests for redeem races (one winner), expiry, revoked
device rejected after a backend restart, rate limit on `/api/auth/pair`;
ZLO tests for link parsing and Keychain migration.

Exit: no client needs the master token; one device can be revoked without
rotating anything else.

### Phase 4: the Tailnet route (replaces T3 Connect)

Status (2026-11-08): backend + CLI built, except the `tailscale whois`
cross-check of `X-Forwarded-For` (not done; the identity header is trusted
on the loopback identity listener only, per phase 0). `[remote]` in
`ironclaw_config` (no `serve_target` key: the target is always
`http://127.0.0.1:<identity_port>`); `ironclaw remote enable|status|disable`
with the Tailscale CLI in `crates/app/ironclaw_cli/src/tailscale.rs`; the
other-local-accounts check runs on every `serve` start as well as at
`enable`. Not yet run against a real tailnet (tested with a fake CLI). ZLO
side pending.

Backend side (`ironclaw_cli` for the commands, `ironclaw_config` for
`[remote]`, `ironclaw_webui` for the identity listener; external calls to
the Tailscale daemon go through a small adapter, not ad-hoc in handlers):

```
ironclaw remote enable     # check Tailscale, start the identity listener,
                           # run `tailscale serve --bg --https=443 <target>`,
                           # record the owner's tailnet login
ironclaw remote status     # URL, owner, paired devices, serve state
ironclaw remote disable    # `tailscale serve --https=<port> off` (never `reset`), stop the listener
```

- `[remote]` in `config.toml`: `enabled`, `owner_logins`, `serve_target`.
  `enable` is the explicit opt-in that makes the host reachable from the
  tailnet; the loopback-only rule in `serve.rs` stays untouched.
- **Never Funnel.** `enable` refuses if Funnel is on for our port.
- **Identity listener**: a second loopback TCP listener (Unix sockets do
  not work with the sandboxed macOS Tailscale, phase 0) used only as the
  `tailscale serve` target, serving the same router. Only on this listener
  are the Tailscale identity headers read; the normal listener strips them.
  `X-Forwarded-For` is cross-checked with `tailscale whois` (same login).
- **`POST /api/auth/pair/tailnet`** (identity listener only): caller's
  tailnet login in `owner_logins` → device token issued directly
  (`paired_by: tailnet_identity`). Anyone else, including other people on
  the same tailnet, gets "ask the owner for a pairing code" (decided: they
  pair with a code from the owner, or run their own IronClaw). Mirrors T3's
  relay-brokered bootstrap, with the tailnet as the broker.
- **Multi-user hosts**: a process of another OS user on the backend machine
  could call the loopback identity port directly and forge the headers,
  and phase 0 showed the backend cannot tell it apart from the Tailscale
  extension. So `remote enable` checks for other login accounts
  (`dscl . list /Users UniqueID`, uid ≥ 500 on macOS; `/etc/passwd`
  login shells on Linux). If there are any, one-click tailnet pairing is
  off and codes are required; `--trust-local-users` turns it on explicitly.

ZLO side (`client/src/connection.rs` + a new `tailnet.rs`):

- **Discover**: run `tailscale status --json`, take online peers, probe
  `https://<DNSName>/.well-known/ironclaw/environment` in parallel (1.5 s
  timeout each). Show "Found on your tailnet: Studio".
- **Connect**: one click → `/api/auth/pair/tailnet`; fallback to the code
  form.
- **Offline**: the peer's `Online: false` shows the machine as asleep
  without probing.
- Transport rule, one function used for links, saved profiles and
  `IRONCLAW_DESKTOP_URL`: HTTPS with normal certificate checks, or plain
  HTTP to loopback only. The current "100.x / .ts.net means private" rule
  goes away, since `tailscale serve` gives HTTPS.

Tests: integration test with a fake identity-header ingress (owner match,
non-owner refused, headers on the normal listener ignored); ZLO tests for
status parsing and discovery against fixtures.

Exit: on a fresh laptop with Tailscale, ZLO finds the Mac mini and connects
in one click with no copy-paste.

### Phase 5: environment registry and supervisor in ZLO

- Replace `Computer::{ThisMac, Remote(url)}` with T3's split:
  - `Environment { id, label }`
  - `Route { Local | Tailnet { dns_name } | Direct { url } | Ssh { host } }`,
    several per environment, in preference order
  - `Credential` (a Keychain reference)
  - `Connection` (live state: available, connecting, connected,
    synchronizing, backoff, blocked, offline)
- One supervisor per saved environment (one active at a time in the UI,
  others listed with their state; switching keeps the old screen until the
  new one answers, as in Hermes).
- Every async completion carries `(environment_id, connection_generation)`;
  stale ones are dropped (`app/src/jobs.rs`).
- Drafts, selected thread, project caches and page caches are keyed by
  environment (`app/src/drafts.rs`, `state.rs`, `workspace.rs`); the global
  `"new"` draft key becomes per environment.

Tests: switching while requests are outstanding never applies a result to
the wrong environment.

### Phase 6: resync contract

- After a new connection (not a resumed SSE stream), refetch the timeline
  and inbox, then subscribe after that snapshot's cursor; drop events at or
  below it.
- Expired or unknown cursor → full refresh, not an error loop.
- Show "connected" and "caught up" separately in the connection indicator.
- Mutations carry a stable action id across retries so a resend after a
  reconnect is not applied twice (backend dedup in the owning product
  handlers).

Owners: `client/src/stream.rs`, `app/src/chat.rs`; backend cursor semantics
in `ironclaw_webui` and the event/projection contracts.

Tests: reconnect mid-run, backend restart mid-run, duplicate replay.

### Phase 7: local features become backend features

| Feature | Change | Owners |
|---|---|---|
| Attachments | client reads bounded bytes and uploads; a client path is never sent as a backend path | `client/src/attachments.rs`, `ironclaw_product_contracts` inbound requests, `ironclaw_attachments` |
| Project tree, previews | use `list_project_files` / `browse_fs_dir` / `read_fs_file` | `app/src/chat/side/{project,preview}.rs` |
| Git (read) | diff and status through a backend view — **built** | `ProductSurface` view + `ironclaw_webui` route |
| Git (write), terminal | backend operations through `ProductSurface`; not a raw shell route — **built** | `ironclaw_orchestrator` (`repo`, `console`) behind `ironclaw_assistant`'s orchestration decorator |
| Open in editor | `vscode://vscode-remote/ssh-remote+<host><path>` (or the editor's equivalent) when the environment has an SSH route; disabled otherwise | `app/src/chat/actions.rs` |
| Computer use | permissions read from the backend's status, not this Mac's | `app/src/chat/computer.rs`, `app/src/permissions.rs` |
| File `@`-mentions | backend index of the thread workspace; mentions name `/workspace/...`, resolved on the backend | `ProductSurface` view + `ironclaw_webui` route, `orchestration::mentions`, `app/src/chat/menu.rs` |
| Logs | fetched from the backend | `app/src/logs.rs` |
| Agent installs (onboarding) | run on the backend through its own setup surface | `app/src/onboarding/agents.rs` |

Each row is its own change; phase 1's "disabled while remote" flags come off
one by one.

#### Git and console on the backend (built 2026-11-09)

- **Routes** (operator-only: classified in
  `is_webui_v2_operator_webui_config_route_id`, re-checked by the product
  surface): `GET|POST /api/webchat/v2/threads/{id}/git` and
  `GET|POST /api/webchat/v2/threads/{id}/console`; wire types in
  `ironclaw_product_contracts::thread_repo`.
- **Which repository**: the thread's own on the backend
  (`Engine::thread_cwd`: its worktree, profile folder or runtime folder, then
  `git rev-parse --show-toplevel`). No request names a folder; file paths are
  repository-relative, plain components only, passed as literal pathspecs
  after `--`. Untracked files are deleted with `git clean` (never follows a
  symlink).
- **Git**: worktree isolation's hardened runner (`worktree::git::Git`:
  hooks and fsmonitor off, no network, no credential helper) plus
  `--no-ext-diff --no-textconv`; outputs streamed and capped (patch 2 MiB,
  2 000 status entries, 500 commits). Changes answer the status read back
  after them. The last turn's checkpoint (`git stash create`) is kept in the
  engine's memory per thread. No commit, push or pull remotely (yet).
- **Console**: one `sh -c` per thread in its folder, a 4 000-line ring the
  client polls with a cursor, 30-minute limit, Stop/timeout/shutdown kill the
  process group. Runs through the host process port
  (`HostProcessPort::spawn_streaming`, scrubbed environment) and is wired only
  when the runtime policy gives the agent a local host process port
  (`process_backend = LocalHost`, i.e. a single-user local deployment);
  elsewhere the view says `available: false` and a start is refused with
  `console_unavailable`.
- **ZLO**: when the backend is remote, Changes/History/Console call these
  routes (`zlo_client::repo_calls`); an older backend (404/405) or one with no
  console keeps the "not available" line (`app/src/chat/side/remote_repo.rs`).
- Not yet run against a second physical machine.

Built (2026-11-08):

- **File `@`-mentions.** `GET /api/webchat/v2/threads/{thread_id}/files/index`
  (`project_fs_index`) answers `{root: "/workspace", entries: [{path, kind}],
  complete}`: paths relative to the workspace, hidden names and
  `node_modules`/`target`/`dist`/`build`/`__pycache__` skipped, at most
  5,000 entries, 600 folders, 12 levels, 3 s. The orchestration surface
  builds it from the thread's own `project_fs_list` listings, so ownership,
  `/workspace` confinement and the sensitive-name filter are that view's.
  `.gitignore` is not read (it is repository-controlled). ZLO's `@` picker
  uses it when the backend is remote, cached per (computer, thread) with the
  local index's 20 s refresh; a picked file mentions `/workspace/<path>` and
  no `mention_roots` are sent. The new-thread screen offers no files while
  remote (no thread workspace yet), and never walks this Mac for a remote.
  **Reconciliation, in one place:** composition resolves the caller's
  `/workspace` through the same `WorkspaceMountPolicy` the file routes read
  (`<workspace_root>` shared, or `<workspace_root>/tenants/<t>/users/<u>`
  per caller) and hands it to `orchestration::mentions` as one more root
  whose *sent* name is `/workspace`; `mentions::fs` maps the path and opens
  the rest below that folder's descriptor with the usual no-follow,
  sensitive-name and hard-link checks. The loop is shown the workspace path;
  a CLI engine the real one. Git-diff mentions stay refused.
- **Computer-use readiness.** `GET /api/webchat/v2/computer-use/readiness`
  (operator only, 403 otherwise): `{platform, engine, accessibility,
  screen_recording, ready}` from `ironclaw_computer_use::HostComputerUseReadiness`
  — engine present / missing / disabled, and `AXIsProcessTrusted` /
  `CGPreflightScreenCaptureAccess` in the backend process (which macOS
  charges for the in-process engine); no prompt, `unknown` when undetermined,
  `unsupported` off macOS. ZLO, when remote and the backend's computer use is
  on, shows one line where the local permission card is — "Computer use on
  <computer>: ready" (until the agent starts driving apps, or Hide) or
  "Computer use on <computer>: needs Screen Recording — grant it on
  <computer>" — and re-asks every 30 s while not ready. This Mac's
  permissions are never checked or prompted for a remote backend.

### Phase 8: SSH route, updates, packaging

- **SSH route** (for people without Tailscale, from Hermes): ZLO runs
  `ssh -N -L <local>:127.0.0.1:<port> user@host`, then
  `ssh user@host ironclaw pair --json` to get a code over the user's own key,
  and redeems it through the tunnel. Tunnel opened on demand, closed with the
  connection.
- **Update all machines** (from Hermes): each remote updates itself through
  its own backend; ZLO last.
- **Light bundle** published next to the full app.

Shipped in ZLO (2026-11-08):

- SSH route: `{"kind": "ssh", "url": "ssh://user@host:22", "remote_port"?}`
  in `environments.json` (`remote_port` only when not 3000; older files
  read unchanged). Settings → Computers → Add a computer has an "Over SSH"
  field (`user@host[:port]`). ZLO runs `/usr/bin/ssh` with
  `BatchMode=yes, ConnectTimeout=8`, arguments as a list after `--`, the
  target checked first (no whitespace, leading `-` or shell characters):
  `ironclaw pair --json`, then `~/.ironclaw/bin/ironclaw pair --json`;
  then `ssh -N -o ExitOnForwardFailure=yes -L 127.0.0.1:<free>:127.0.0.1:<port>`
  and the code is redeemed through the tunnel. The backend port comes from
  the `pair` link when it names the loopback listener, else 3000. Saved
  routes: the descriptor's tailnet URL (if any), then SSH; the tunnel's
  loopback address is never saved. Connect tries tailnet/direct first,
  SSH last. One tunnel at a time (`app/src/remote/tunnel.rs`): opened on
  demand, restarted with backoff while wanted, closed on switching away
  and on quit. Unknown host keys are refused (trust them with `ssh` once
  in Terminal). Client code: `client/src/ssh.rs`.
- Light bundle: `bundle.sh --light` (or `ZLO_LIGHT=1`) builds
  `ZLO Light.app` (bundle id `com.numoh.desktop.light`, so it never
  replaces `ZLO.app`) without `zlo-backend` or the computer-use engine.
  Running from an app bundle with no `zlo-backend` beside the executable
  makes ZLO app-only (`service::light_bundle`; `cargo run` never is): no
  local backend, This Mac hidden in Settings → Computers and setup, and
  with no computer saved the window is "Add a computer".
- Update all machines (2026-11-08): Settings → Computers → Updates has
  "Check for updates" (every saved computer and This Mac in parallel:
  `POST /api/operator/update/check`) and "Update all machines" (the other
  computers in parallel: `POST /api/operator/update`, then `GET` every 3 s
  until their job ends, riding out the backend going away and finding it
  again through its descriptor, 45-minute limit; then This Mac, whose job
  reinstalls and reopens ZLO). Each row says up to date / N commits behind /
  updating… / updated / failed: <reason> / not set up (method none), with
  the `[update]` lines to add. Computers reached only over SSH update while
  they are the one in use (the one tunnel belongs to that connection). Rows
  live in app state: they survive the window, not an app restart. Code:
  `app/src/remote/updates.rs`, `client/src/update_calls.rs`; backend:
  `crates/app/ironclaw_cli/src/commands/update/`.

## Security invariants (checked in every phase)

- Backend stays on loopback; remote reach only through `ironclaw remote
  enable` (Tailscale) or SSH. Never Funnel, never `0.0.0.0` with full host
  access.
- Pairing gives identity, not extra power: approvals, capability checks and
  `ProductSurface` still apply to every request.
- No master token on the wire after phase 3; device tokens hashed at rest
  on the backend, in the Keychain on the client.
- Tailscale identity headers trusted only on the identity listener.
- Revocation is durable; a rollback must not bring revoked devices back.
- Provider credentials and secrets never leave the backend.
- Removing a computer from ZLO deletes local credentials and caches only;
  backend conversation data is never deleted.

## Decisions (2026-11-08)

1. Device tokens carry full operator authority.
2. Phones are left out; a dedicated phone app comes later on the same
   pairing contract.
3. Other people on the same tailnet pair with a code from the owner, or run
   their own IronClaw. Only `owner_logins` get one-click tailnet pairing.
4. Phase 0 done: loopback TCP serve target, identity headers trusted only
   on the identity listener (results above).

## Open questions

1. Phase 0 items that need a second online node (real remote caller,
   tagged/shared callers, Linux socket target).
2. Where ZLO (`tools/ironclaw_rune`) moves when it is promoted into the
   `crates/` workspace; phase 5 may be the right moment.

## Sequencing summary

| Phase | Side | Depends on | Unblocks |
|---|---|---|---|
| 0 spike (done) | — | — | 4 |
| 1 app-only mode | ZLO | — | laptop use today |
| 2 identity | backend + ZLO | — | 3, 5 |
| 3 device pairing | backend + CLI + ZLO | 2 | 4, the future phone app |
| 4 Tailnet route | backend + CLI + ZLO | 0, 3 | one-click connect |
| 5 registry | ZLO | 2 | several machines |
| 6 resync | both | 5 | reliable remote use |
| 7 local features | both | 1 | full parity |
| 8 SSH, updates, packaging | both | 3, 4 | everything else |
