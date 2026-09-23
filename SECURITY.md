# Security

## Threat model

**In scope / defended against:**

- **Unauthorized Telegram users.** `AUTHORIZED_CHAT_IDS` is a hard
  allowlist checked before any command is processed, any data is read, or
  any log line beyond the chat id is written. Callback queries (inline
  button taps) re-check the allowlist independently of the message
  handler.
- **Credential exposure at rest.** JMAP server URL and bearer token are
  AES-256-GCM encrypted before being written to disk. The master key and
  ciphertext files are created with `0600` permissions.
- **Credential exposure in transit.** All outbound connections (Telegram
  Bot API, JMAP server) use TLS (`rustls`, no OpenSSL/native-tls in the
  dependency tree at all — see `Cargo.toml`).
- **Credential exposure in the chat itself.** The `/login` message
  containing the bearer token is deleted from the chat immediately after
  the bot reads it (Telegram allows bots to delete incoming messages in
  private chats within 48 hours).
- **Formatting/markup injection.** Email subjects, sender names, and
  preview text are attacker-controlled (they're whatever showed up in the
  mailbox) and are fully escaped before being interpolated into a
  Telegram `MarkdownV2` message, so a crafted email can't inject
  formatting or break message rendering.
- **Malformed/oversized JMAP ids.** Inline button `callback_data` is
  capped by Telegram at 64 bytes; if a JMAP id would exceed that, the bot
  drops the action buttons for that notification rather than sending a
  button Telegram would silently reject.
- **Oversized/malicious mail content.** Subject, sender name, and preview
  are all length-capped before being rendered, so a hostile sender can't
  push a notification past Telegram's 4096-char message limit and get it
  silently dropped (the JMAP sync cursor still advances either way).
- **Data retention.** No email content (subject, body, sender) is ever
  persisted to disk — only an opaque JMAP sync cursor (`state`) is stored,
  which identifies a point in the server's change log, not any message
  content. `/logout` deletes the stored account synchronously and
  durably; nothing is soft-deleted or retained.
- **Shared JMAP accounts (`/partages`).** A shared/delegated mailbox is
  only ever offered for opt-in if the connected token is already granted
  access to it by the JMAP server itself (`/partages` lists exactly what
  `session.accounts()` returns for that token) — the bot cannot expand
  access beyond what the server already granted. Opting in stores the
  same minimal shape as the primary account (account id, display name,
  sync cursor; no content), and opting out or `/logout` erases it the
  same way.
- **SSRF via `/login`.** `server_url` is attacker-controlled (it's
  whatever an authorized chat typed in), and `AUTHORIZED_CHAT_IDS` can
  list several mutually-untrusted chats. By default, any hostname that
  resolves to a private/loopback/link-local/multicast address (including
  the `169.254.169.254` cloud metadata endpoint) is refused before the
  bearer token is ever sent to it — otherwise `/login` would be a
  ready-made SSRF probe of the deployment's internal network. Self-hosters
  who deliberately run their JMAP server on an internal network can opt
  back in with `ALLOW_PRIVATE_JMAP_HOSTS=1`. Residual risk: this is a
  pre-connect DNS check, not a per-connection resolver, so a DNS answer
  that changes between the check and the actual request (rebinding) is
  not covered — treat `ALLOW_PRIVATE_JMAP_HOSTS` as "trust this
  deployment's authorized chats," not as a sandboxed boundary.
- **Group chats.** The bot only responds in private (1:1) chats. A group
  chat id in `AUTHORIZED_CHAT_IDS` would otherwise hand every current and
  future member of that group full control (read/archive/delete) over
  the group's one connected mailbox with no per-member opt-in; messages
  from non-private chats are silently ignored instead.
- **Leaked watcher/connection on repeated `/login`.** Reconnecting an
  already-connected chat tears down the previous background watcher and
  JMAP connection first, so it can't be used to leak tasks/connections
  and exhaust the (intentionally single-replica) deployment's memory.
- **Concurrent writes to the encrypted store.** Credential-store writes
  from different chats' watchers/handlers are serialized, so they can't
  race on the same temp file and corrupt or lose a write.

**Explicitly out of scope / accepted trade-offs:**

- **Root/filesystem access to the running container or its volume.**
  Anyone with that level of access can read both the master key and the
  encrypted state file. This is the accepted trade-off of a deployment
  that takes exactly two environment variables and no external key
  management service. If you need defense against that threat, mount
  `/data` from storage with its own encryption/access controls, or fork
  the master-key handling to pull from a KMS.
- **A compromised JMAP server or Telegram account.** The bot trusts both
  endpoints it talks to, as any client must.
- **Multi-account/shared-mailbox isolation beyond the Telegram chat id.**
  Authorization is per Telegram chat id; anyone who controls an
  authorized chat controls that chat's connected mailbox.

## Regulatory & framework alignment

This is self-hosted software, not a certified product or a managed
service: **certification against any of these frameworks is an
organizational process** (a formal audit, a risk register, documented
policies, signed-off procedures) that the *deployer* carries out, not
something a codebase can claim on its own. What follows is an honest
mapping of the technical controls actually implemented here to the
requirements each framework cares about, so a deployer's own compliance
work starts from an accurate picture rather than from zero — not a claim
of certification.

### GDPR

- **Art. 5(1)(c) data minimization** — the only data ever persisted is
  what `/login` needs to keep working: JMAP server URL, bearer token,
  the account's email address, and an opaque sync cursor. No message
  content (subject, body, sender) ever touches disk.
- **Art. 5(1)(f) / Art. 32 integrity, confidentiality, security of
  processing** — AES-256-GCM at rest, TLS (rustls) in transit, a hard
  per-chat access allowlist, and (see above) a documented threat model
  covering SSRF, injection, and resource-exhaustion classes of risk.
- **Art. 17 right to erasure** — `/logout` deletes the chat's stored
  record synchronously and durably; nothing is soft-deleted, queued, or
  retained past that call.
- **Art. 25 data protection by design and by default** — the "two env
  vars, everything else live in chat" design *is* the privacy posture:
  there's no config file, CI secret, or support ticket that could ever
  contain a user's JMAP credentials except the encrypted on-disk store
  itself.
- **Art. 33/34 breach notification** — this is an operational duty of
  whoever deploys and operates the bot (the data controller), not
  something the software can discharge for them. What the software gives
  them going in: a breach of the encrypted store exposes JMAP tokens and
  the deployment key, but never historical email content, since none is
  stored.
- **Controller/processor roles** — a self-hosting deployer is the data
  controller for their users' data; Telegram (message transport) and the
  chosen JMAP provider (mail hosting) act as processors/sub-processors
  under their own terms. Confirming that arrangement is suitable is the
  deployer's call, not this software's.

### ISO/IEC 27001 (Annex A, 2022)

Controls this codebase directly supports: **A.8.24** (cryptography —
AES-256-GCM, TLS), **A.8.9** (configuration management — the Helm chart's
explicit `securityContext`, non-root, read-only rootfs), **A.8.3 / A.8.2**
(access restriction — the chat allowlist, private-chat-only enforcement),
**A.8.12** (data leakage prevention — data minimization, no content
persistence), **A.8.28 / A.8.29** (secure coding and security testing —
`clippy -D warnings`, `cargo audit`, the CI test suite, and two rounds of
independent adversarial review whose findings are fixed and recorded in
git history), **A.5.7 / A.8.8** (vulnerability management — Dependabot +
`cargo audit` gating every CI run), and **A.5.24–5.28** (incident
handling — the reporting process below). Building an ISMS around this
(risk register, management review, internal audit) is the deployer's
work; the technical controls above are what that ISMS would find in
place.

### NIS2 (Directive (EU) 2022/2555, Art. 21 risk-management measures)

NIS2 obligations attach to "essential"/"important" entities, not to a
piece of software — but its Art. 21 measures map cleanly to what's here:
supply-chain security (**Dependabot** across cargo/Docker/GitHub Actions,
plus `cargo audit` in CI), vulnerability handling and disclosure (this
file's reporting process), cryptography (TLS + AES-256-GCM), access
control (the chat allowlist), and secure development (CI gates + the
adversarial-review history). Out of this software's scope, because
they're organizational: business continuity/disaster-recovery planning
for the deployment, staff cyber-hygiene training, and multi-factor
authentication for *operator* access to the deployment itself (Telegram's
own auth is the identity boundary for *end users*, not something this
software layers MFA on top of).

### PCI-DSS

**Not applicable.** This application never processes, stores, or
transmits payment card data (PAN, CVV, expiry) in any form — there is no
cardholder data environment here to bring into PCI-DSS scope.

## Reporting a vulnerability

Please open a private security advisory on this repository (**Security**
tab → **Report a vulnerability**) rather than a public issue. Include
reproduction steps and, if applicable, which of the guarantees above is
violated.
