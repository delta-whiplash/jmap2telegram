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

## Reporting a vulnerability

Please open a private security advisory on this repository (**Security**
tab → **Report a vulnerability**) rather than a public issue. Include
reproduction steps and, if applicable, which of the guarantees above is
violated.
