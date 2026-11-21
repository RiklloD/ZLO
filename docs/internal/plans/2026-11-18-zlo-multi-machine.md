# ZLO: several machines working together

Status: implemented on `feat/multi-machine` (2026-11-18). Follows
`2026-11-08-zlo-remote-environments.md` (wire contract v1), and copies
t3code's peer links, remote delegation, secret requests and route
supervisor, with Tailscale instead of T3 Connect.

## What landed

| Area | Where | Notes |
|---|---|---|
| Per-device scopes | `ironclaw_product_contracts::environment_access`, `ironclaw_identity`, `ironclaw_webui::device_scopes` | `zlo pair --scope …`; enforced on every device-token request by path family. Old devices keep every scope. |
| Peer links | `peer` contract module, identity `peer_links` records, `ironclaw_webui::peer_client` | `zlo pair --link operate` on the other machine, `zlo environment link <https url> --pairing-code … --access operate` here (or Settings → Computers → Linked environments). Token in the operator secret store; 30-day grant; https/loopback only. |
| Tools on another machine | `ironclaw_orchestrator::peer` | `environment` on every `t3_*` tool. Receiving side fenced by `min(link access, caller mode)`. |
| Remote delegation | `ironclaw_orchestrator::peer` | Local task + durable follower; idempotent launch key hashed per caller; Stop carried across restarts. |
| Link requests | `t3_environment_link` | Card in the thread and on the Computers page; pick a linked machine or paste address + code. |
| Access fence | `PeerReceiver`, `Store::thread_profile` | Below full: only link-started threads, no schedules, no further hops, children never on "Full access". |
| Secret requests | `t3_request_secret`, `ironclaw_composition::multi_machine` vault | Private card from any device; `secretref:` or 24-hour 0600 file; never in the chat or the table. |
| Secret references | `ironclaw_secrets::reference` | Any stored secret may be `op://vault/item/field`, read with `op read` when consumed (desktop sign-in, service account or Connect). |
| Learned routes | descriptor `addresses`, Rune `Registry::learn_routes` | Tailnet addresses of the private listener; used only when this Mac's tailnet lists the address as a peer (`check_learned_transport`). |
| Fail-back | Rune `remote::failback` | Earlier routes re-checked every minute (descriptor id + an authorized call); moves back quietly (`connect::refresh`). |
| Private listener | `[remote] private_port`, `serve_private_listener` | Every IPv4 interface; only loopback, RFC 1918 and tailnet peers. |
| SSH launch | Rune `zlo_client::ssh::{start_backend, stop_launched_backend, install_backend}` | Starts `zlo serve` when the tunnel finds nothing; pid file marks ownership; Remove stops only what ZLO launched; missing `zlo` is built with cargo there. |
| Load balancing | `t3_delegate environment=auto`, link preference | Lowest `pressure × weight`; `Manual only` excluded; all full → error naming the choice. |
| Outside agents | `POST /api/mcp` | MCP for terminal agents with a scoped device token, over the same fenced path. |

## Decisions

- **No plain http to 100.64/10 from the backend.** Other CGNAT VPNs share
  the range; Rune removed "Tailscale counts as private" earlier for the same
  reason. Rune accepts a learned http tailnet route only for an address its
  live peer list contains.
- **Peer calls are orchestrator tool calls**, not a second API: one route,
  one fence, the same tool semantics on both sides.
- **The receiving side reuses delegation.** A link is a hidden parent
  thread; its tasks are ordinary children with delivery skipped (the
  linking side reads the result), so depth limits, worktrees and profiles
  apply unchanged.

## Not done

- Load-balancing *new threads* from the composer: Rune works on one
  computer at a time; balancing is on delegation instead.
- Remote browser tabs, SSH device hosts, guest links and a phone app.
