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
- **Data retention.** No email content (subject, body, sender) is ever
  persisted to disk — only an opaque JMAP sync cursor (`state`) is stored,
  which identifies a point in the server's change log, not any message
  content. `/logout` deletes the stored account synchronously and
  durably; nothing is soft-deleted or retained.

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
