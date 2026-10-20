use std::fmt;
use std::net::{IpAddr, Ipv4Addr};

use jmap_client::DataType;
use jmap_client::client::{Client, Credentials};
use jmap_client::core::response::EmailGetResponse;
use jmap_client::email::Property;
use jmap_client::email::query as email_query;
use jmap_client::mailbox::{self, Role};

/// How many results `search` returns at most, so a broad query (e.g. a
/// single common word) can't pull an unbounded number of full Email/get
/// lookups and produce an unusably long Telegram reply.
const SEARCH_RESULT_LIMIT: usize = 10;

/// Result type for every public function in this module.
type Result<T> = std::result::Result<T, JmapError>;

/// The fixed English message behind [`JmapError::Unreachable`]'s `Display`.
/// The raw transport error, by contrast, fingerprints exactly one port
/// state ("connection refused" vs "timed out" vs a TLS/HTTP error, each
/// rendered by reqwest with the full URL and OS-level TCP cause), so it must
/// never travel past the server-side log (see [`JmapError::Unreachable`]).
/// The chat-facing (localized) wording lives in [`JmapError::localized`].
const UNREACHABLE_SERVER_MSG: &str = "JMAP server unreachable or invalid (details in server logs)";

/// Typed, user-facing JMAP failure categories.
///
/// The `Display` impl is the English, log-facing rendering - greppable in
/// one language no matter who's chatting - while [`JmapError::localized`]
/// produces the chat rendering in the reader's own locale. Every variant is
/// deliberately coarse: none carries transport detail (URL, OS error, port
/// state), so an error returned to a chat can never become a
/// reachability oracle (see `UNREACHABLE_SERVER_MSG` and SECURITY.md).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JmapError {
    /// The server URL didn't start with `https://`.
    InsecureUrl,
    /// The server URL couldn't be parsed as an absolute URL.
    InvalidUrl,
    /// The parsed URL had no host component.
    NoHost,
    /// The host resolved to a private/loopback/link-local address and the
    /// SSRF guard refused it (`ALLOW_PRIVATE_JMAP_HOSTS` off).
    PrivateHost { host: String, ip: String },
    /// Any network-level failure (DNS, connect, TLS, autodiscovery, session
    /// negotiation, authentication) - with no transport detail attached.
    Unreachable,
    /// A JMAP protocol call failed; `operation` names it for the logs only.
    Protocol { operation: &'static str },
    /// The message no longer exists server-side.
    MessageNotFound,
    /// The account exposes no mailbox with the Archive role.
    NoArchiveMailbox,
    /// The account exposes no mailbox with the Junk role.
    NoJunkMailbox,
    /// The account exposes no mailbox with the Trash role.
    NoTrashMailbox,
}

impl fmt::Display for JmapError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            JmapError::InsecureUrl => write!(f, "the JMAP server URL must start with https://"),
            JmapError::InvalidUrl => write!(f, "invalid JMAP server URL"),
            JmapError::NoHost => write!(f, "JMAP server URL without a host name"),
            JmapError::PrivateHost { host, ip } => {
                write!(
                    f,
                    "refused non-public address {ip} for host '{host}' (SSRF guard)"
                )
            }
            JmapError::Unreachable => write!(f, "{UNREACHABLE_SERVER_MSG}"),
            JmapError::Protocol { operation } => write!(f, "JMAP call failed: {operation}"),
            JmapError::MessageNotFound => write!(f, "message not found (maybe already deleted)"),
            JmapError::NoArchiveMailbox => write!(f, "this account has no JMAP Archive folder"),
            JmapError::NoJunkMailbox => write!(f, "this account has no JMAP Spam/Junk folder"),
            JmapError::NoTrashMailbox => write!(f, "this account has no JMAP Trash folder"),
        }
    }
}

impl std::error::Error for JmapError {}

impl JmapError {
    /// The chat-facing rendering, in the reader's locale.
    pub fn localized(&self, lang: &str) -> String {
        match self {
            JmapError::Unreachable => t!("jmap.unreachable", locale = lang).to_string(),
            JmapError::InsecureUrl => t!("jmap.insecure_url", locale = lang).to_string(),
            JmapError::InvalidUrl => t!("jmap.invalid_url", locale = lang).to_string(),
            JmapError::NoHost => t!("jmap.no_host", locale = lang).to_string(),
            JmapError::PrivateHost { host, ip } => {
                // The host/ip here describe the input the user themselves
                // supplied and the address it resolved to - not a probe
                // result - so they're safe to show (unlike a transport error).
                t!("jmap.private_host", locale = lang, host = host, ip = ip).to_string()
            }
            // The operation name is an internal tag for the logs; the chat
            // only needs to know the server refused the call.
            JmapError::Protocol { .. } => t!("jmap.protocol", locale = lang).to_string(),
            JmapError::MessageNotFound => t!("jmap.message_not_found", locale = lang).to_string(),
            JmapError::NoArchiveMailbox => t!("jmap.no_archive", locale = lang).to_string(),
            JmapError::NoJunkMailbox => t!("jmap.no_junk", locale = lang).to_string(),
            JmapError::NoTrashMailbox => t!("jmap.no_trash", locale = lang).to_string(),
        }
    }
}

/// Renders an [`anyhow::Error`] for the chat: the localized [`JmapError`]
/// when one is at the top of the chain (the common case, since every
/// `jmap::*` call returns one), otherwise the English chain as-is.
pub fn localized_error(err: &anyhow::Error, lang: &str) -> String {
    match err.downcast_ref::<JmapError>() {
        Some(e) => e.localized(lang),
        None => err.to_string(),
    }
}

pub struct EmailSummary {
    pub id: String,
    pub subject: String,
    pub from_name: Option<String>,
    pub from_addr: Option<String>,
    pub preview: String,
    pub received_at: Option<i64>,
    /// Attachment (name, size in bytes) pairs. Never downloaded or held in
    /// memory - this is metadata only, so the notification can at least
    /// say "there's a 4 MB file here" without silently hiding it.
    pub attachments: Vec<(String, usize)>,
}

fn attachments_of(email: &jmap_client::email::Email) -> Vec<(String, usize)> {
    email
        .attachments()
        .unwrap_or_default()
        .iter()
        .map(|a| (a.name().unwrap_or_default().to_string(), a.size()))
        .collect()
}

/// Connects to a JMAP server using a bearer token, the way most JMAP
/// providers (Fastmail, Stalwart, ...) expect for API access. This is the
/// direct functional equivalent of GmailBot's OAuth exchange, except the
/// whole exchange happens as a single message in the Telegram chat instead
/// of a redirect flow, since it needs no public callback URL.
///
/// `server_url` is the provider's server root (e.g. `https://jmap.fastmail.com`),
/// *not* a full session endpoint: `jmap-client` performs RFC 8620
/// autodiscovery by fetching `{server_url}/.well-known/jmap` itself.
///
/// `server_url` is attacker-controlled: it's whatever an authorized chat
/// typed into `/login`, and `AUTHORIZED_CHAT_IDS` can list several
/// mutually-untrusted chats. Unless `allow_private_hosts` is set, any
/// hostname that resolves to a private/loopback/link-local address is
/// refused before we ever send it the bearer token - otherwise this
/// would be a ready-made SSRF primitive against the deployment's internal
/// network (cluster services, cloud metadata endpoints, ...).
///
/// Error-wise, only the purely local checks (the `https://` scheme, URL
/// parsing, a missing host) and the SSRF guard's own refusal stay
/// detailed: they describe the input the user themselves supplied, not
/// the behavior of anything on the network. Every network-level failure
/// past them is deliberately collapsed into the single fixed
/// [`UNREACHABLE_SERVER_MSG`] above, with the cause logged server-side -
/// see that constant for why.
pub async fn connect(server_url: &str, token: &str, allow_private_hosts: bool) -> Result<Client> {
    if !server_url.starts_with("https://") {
        return Err(JmapError::InsecureUrl);
    }

    let host = url::Url::parse(server_url)
        .map_err(|_| JmapError::InvalidUrl)?
        .host_str()
        .ok_or(JmapError::NoHost)?
        .to_string();

    if !allow_private_hosts {
        assert_public_host(&host, server_url).await?;
    }

    // jmap-client only follows a redirect to a host in this explicit
    // allowlist (anything else is aborted); RFC 8620 autodiscovery
    // (`/.well-known/jmap`) commonly 30x-redirects to a same-host session
    // path (e.g. Stalwart redirects to `/jmap/session`), so without this
    // every real-world provider that does that would fail to connect.
    // Trusting only the host the user themselves supplied keeps this from
    // widening the SSRF surface checked above.
    let client = Client::new()
        .credentials(Credentials::bearer(token))
        .follow_redirects([host.clone()])
        .connect(server_url)
        .await
        .map_err(|e| {
            // Error-oracle neutralization: this handshake is the first
            // attacker-triggered request to a possibly attacker-chosen
            // server, and its raw transport error (which jmap-client
            // renders as "Transport error: <full URL + OS-level TCP
            // cause>") would read back as a port-state fingerprint of
            // whatever the hostname resolves to by the time *this*
            // request re-resolves it (see the residual TOCTOU in
            // SECURITY.md). Log the cause server-side and return a bare
            // generic variant - bare, so the dropped cause can't be
            // resurrected from the error chain either.
            tracing::warn!(
                host = %host,
                error = %e,
                "JMAP session negotiation failed (details kept out of the chat)"
            );
            JmapError::Unreachable
        })?;

    Ok(client)
}

/// Connects the same way as [`connect`], then switches the client's default
/// account to a specific JMAP account id (e.g. a delegated shared mailbox)
/// instead of the token's personal account. `jmap-client` only tracks one
/// default account per `Client` and every request method reads it, so a
/// shared account needs its own dedicated `Client` rather than reusing the
/// primary one with a mutated account id - the primary client is shared
/// (`Arc`) across concurrent tasks and mutating it out from under them
/// would race.
pub async fn connect_shared(
    server_url: &str,
    token: &str,
    allow_private_hosts: bool,
    account_id: &str,
) -> Result<Client> {
    let mut client = connect(server_url, token, allow_private_hosts).await?;
    client.set_default_account_id(account_id);
    Ok(client)
}

/// Lists every non-personal (shared/delegated) account visible in the
/// current session, as `(account_id, display_name)` pairs. Reflects
/// whatever the token is granted access to *right now* - the caller should
/// reconnect first if it wants a fresh view rather than a cached session
/// from an earlier connect.
pub fn list_shared_accounts(client: &Client) -> Vec<(String, String)> {
    let session = client.session();
    session
        .accounts()
        .filter_map(|id| {
            let account = session.account(id)?;
            if account.is_personal() {
                None
            } else {
                Some((id.clone(), account.name().to_string()))
            }
        })
        .collect()
}

async fn assert_public_host(host: &str, server_url: &str) -> Result<()> {
    let parsed = url::Url::parse(server_url).map_err(|_| JmapError::InvalidUrl)?;
    let port = parsed.port_or_known_default().unwrap_or(443);

    // Same error-oracle neutralization as the session handshake in
    // `connect`: *how* a name fails to resolve (NXDOMAIN vs resolver
    // timeout vs empty answer) distinguishes internal DNS states, and the
    // distinction used to travel to the chat. Only the fixed generic
    // message goes back now; the cause goes server-side.
    let addrs = match tokio::net::lookup_host((host, port)).await {
        Ok(addrs) => addrs,
        Err(e) => {
            tracing::warn!(
                host = %host,
                error = %e,
                "DNS resolution of the JMAP server failed (details kept out of the chat)"
            );
            return Err(JmapError::Unreachable);
        }
    };

    let mut resolved_any = false;
    for addr in addrs {
        resolved_any = true;
        if is_disallowed_host(addr.ip()) {
            // A deliberate policy refusal, not a network probe result:
            // it explains a decision about a name the user themselves
            // supplied (and how to opt in), and reveals nothing about
            // whether anything is listening anywhere - which is why,
            // unlike every transport error above and below, it stays
            // detailed enough to be actionable in the chat.
            return Err(JmapError::PrivateHost {
                host: host.to_string(),
                ip: addr.ip().to_string(),
            });
        }
    }
    if !resolved_any {
        tracing::warn!(
            host = %host,
            "DNS resolution of the JMAP server returned no addresses (details kept out of the chat)"
        );
        return Err(JmapError::Unreachable);
    }
    Ok(())
}

fn is_disallowed_host(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_disallowed_v4(v4),
        IpAddr::V6(v6) => {
            if let Some(mapped) = v6.to_ipv4_mapped() {
                return is_disallowed_v4(mapped);
            }
            let seg0 = v6.segments()[0];
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (seg0 & 0xfe00) == 0xfc00 // unique local fc00::/7
                || (seg0 & 0xffc0) == 0xfe80 // link-local unicast fe80::/10
        }
    }
}

fn is_disallowed_v4(v4: Ipv4Addr) -> bool {
    v4.is_private()
        || v4.is_loopback()
        // Covers 169.254.0.0/16, which includes the 169.254.169.254 cloud
        // metadata endpoint commonly targeted by SSRF exploits.
        || v4.is_link_local()
        || v4.is_unspecified()
        || v4.is_multicast()
        || v4.is_broadcast()
        || v4.is_documentation()
        // RFC 6598 Carrier-Grade NAT / shared address space (100.64.0.0/10):
        // not covered by is_private(), but used as real internal-network
        // space by some deployments (it's Tailscale's default overlay
        // range, among others). Ipv4Addr::is_shared() would cover this but
        // is still unstable, so check the range manually.
        || (v4.octets()[0] == 100 && (v4.octets()[1] & 0b1100_0000) == 0b0100_0000)
}

pub fn account_email(client: &Client) -> String {
    client.session().username().to_string()
}

/// Establishes the baseline Email sync cursor without fetching or
/// notifying about any existing mail. Used right after /login so the
/// user only gets notified about mail that arrives from now on.
pub async fn current_email_state(client: &Client) -> Result<String> {
    let mut request = client.build();
    request.get_email().ids(Vec::<String>::new());
    let response = request
        .send_single::<EmailGetResponse>()
        .await
        .map_err(|_| JmapError::Protocol {
            operation: "Email/get (state baseline)",
        })?;
    Ok(response.state().to_string())
}

/// Fetches emails created since `since_state` and returns them along with
/// the new cursor to persist. The JMAP `state` string is the single source
/// of truth for what has already been seen; EventSource pushes are only a
/// wake-up signal, never trusted on their own.
pub async fn fetch_changed_emails(
    client: &Client,
    since_state: &str,
) -> Result<(Vec<EmailSummary>, String)> {
    let changes = client
        .email_changes(since_state, Some(50))
        .await
        .map_err(|_| JmapError::Protocol {
            operation: "Email/changes",
        })?;
    let new_state = changes.new_state().to_string();
    let created = changes.created().to_vec();

    let mut summaries = Vec::with_capacity(created.len());
    for id in created {
        if let Some(email) = client
            .email_get(
                &id,
                Some([
                    Property::Subject,
                    Property::Preview,
                    Property::From,
                    Property::ReceivedAt,
                    Property::Attachments,
                ]),
            )
            .await
            .map_err(|_| JmapError::Protocol {
                operation: "Email/get",
            })?
        {
            let (from_name, from_addr) = email
                .from()
                .and_then(|addrs| addrs.first())
                .map(|a| (a.name().map(str::to_string), Some(a.email().to_string())))
                .unwrap_or((None, None));

            summaries.push(EmailSummary {
                id,
                subject: email.subject().unwrap_or_default().to_string(),
                from_name,
                from_addr,
                preview: email.preview().unwrap_or_default().to_string(),
                received_at: email.received_at(),
                attachments: attachments_of(&email),
            });
        }
    }

    Ok((summaries, new_state))
}

/// Full-text search across the account (`Email/query` with a `text`
/// filter, matching subject/body/from/to per RFC 8620 §5.5), newest first,
/// capped at `SEARCH_RESULT_LIMIT` results. Read-only - unlike every other
/// action in this module, it never advances the sync cursor or changes
/// anything server-side, so it carries none of the risk a write action
/// would.
pub async fn search(client: &Client, query: &str) -> Result<Vec<EmailSummary>> {
    let mut request = client.build();
    request
        .query_email()
        .filter(email_query::Filter::text(query))
        .sort([email_query::Comparator::received_at().descending()])
        .limit(SEARCH_RESULT_LIMIT);
    let query_response = request
        .send_query_email()
        .await
        .map_err(|_| JmapError::Protocol {
            operation: "Email/query",
        })?;
    let ids = query_response.ids().to_vec();
    if ids.is_empty() {
        return Ok(Vec::new());
    }

    let mut request = client.build();
    request.get_email().ids(ids).properties([
        Property::Subject,
        Property::Preview,
        Property::From,
        Property::ReceivedAt,
        Property::Attachments,
    ]);
    let mut response = request
        .send_get_email()
        .await
        .map_err(|_| JmapError::Protocol {
            operation: "Email/get (search)",
        })?;

    Ok(response
        .take_list()
        .into_iter()
        .filter_map(|email| {
            let id = email.id()?.to_string();
            let (from_name, from_addr) = email
                .from()
                .and_then(|addrs| addrs.first())
                .map(|a| (a.name().map(str::to_string), Some(a.email().to_string())))
                .unwrap_or((None, None));
            let attachments = attachments_of(&email);
            Some(EmailSummary {
                id,
                subject: email.subject().unwrap_or_default().to_string(),
                from_name,
                from_addr,
                preview: email.preview().unwrap_or_default().to_string(),
                attachments,
                received_at: email.received_at(),
            })
        })
        .collect())
}

/// Fetches the full plain-text body of a message on demand (never cached
/// to disk, only held in memory long enough to render the Telegram
/// message).
pub async fn fetch_full_text(client: &Client, id: &str) -> Result<String> {
    let mut request = client.build();
    let get_req = request.get_email();
    get_req
        .ids([id])
        .properties([Property::TextBody, Property::Preview]);
    get_req
        .arguments()
        .fetch_text_body_values(true)
        .max_body_value_bytes(8000);

    let mut response = request
        .send_single::<EmailGetResponse>()
        .await
        .map_err(|_| JmapError::Protocol {
            operation: "Email/get (full body)",
        })?;

    let Some(email) = response.take_list().pop() else {
        return Err(JmapError::MessageNotFound);
    };

    let mut text = String::new();
    if let Some(parts) = email.text_body() {
        for part in parts {
            if let Some(part_id) = part.part_id()
                && let Some(value) = email.body_value(part_id)
            {
                text.push_str(value.value());
            }
        }
    }
    if text.trim().is_empty() {
        // Left empty on purpose: the placeholder for "nothing to show" is
        // localized at the presentation layer (the bot), not here.
        text = email.preview().unwrap_or_default().to_string();
    }
    Ok(text)
}

async fn mailbox_with_role(client: &Client, role: Role) -> Result<Option<String>> {
    let response = client
        .mailbox_query(Some(mailbox::query::Filter::role(role)), None::<Vec<_>>)
        .await
        .map_err(|_| JmapError::Protocol {
            operation: "Mailbox/query",
        })?;
    Ok(response.ids().first().cloned())
}

pub async fn mark_read(client: &Client, id: &str) -> Result<()> {
    client
        .email_set_keyword(id, "$seen", true)
        .await
        .map_err(|_| JmapError::Protocol {
            operation: "Email/set keyword",
        })?;
    Ok(())
}

/// The mailbox ids a message currently sits in, taken right before a
/// triage action moves it elsewhere - the snapshot `/undo` restores.
pub async fn mailbox_ids_of(client: &Client, id: &str) -> Result<Vec<String>> {
    let email = client
        .email_get(id, Some([Property::MailboxIds]))
        .await
        .map_err(|_| JmapError::Protocol {
            operation: "Email/get (undo snapshot)",
        })?
        .ok_or(JmapError::MessageNotFound)?;
    Ok(email
        .mailbox_ids()
        .into_iter()
        .map(str::to_string)
        .collect())
}

/// Puts a message back in a specific set of mailboxes - the undo half of
/// archive/junk/delete, restoring whatever `mailbox_ids_of` snapshotted
/// beforehand rather than assuming "back to Inbox" (a message filed into
/// more than one mailbox should come back exactly as it was).
pub async fn restore_mailboxes(client: &Client, id: &str, mailbox_ids: Vec<String>) -> Result<()> {
    client
        .email_set_mailboxes(id, mailbox_ids)
        .await
        .map_err(|_| JmapError::Protocol {
            operation: "Email/set mailboxes (undo)",
        })?;
    Ok(())
}

pub async fn archive(client: &Client, id: &str) -> Result<()> {
    let Some(archive_id) = mailbox_with_role(client, Role::Archive).await? else {
        return Err(JmapError::NoArchiveMailbox);
    };
    client
        .email_set_mailboxes(id, [archive_id])
        .await
        .map_err(|_| JmapError::Protocol {
            operation: "Email/set mailboxes (archive)",
        })?;
    Ok(())
}

pub async fn junk(client: &Client, id: &str) -> Result<()> {
    let Some(junk_id) = mailbox_with_role(client, Role::Junk).await? else {
        return Err(JmapError::NoJunkMailbox);
    };
    client
        .email_set_mailboxes(id, [junk_id])
        .await
        .map_err(|_| JmapError::Protocol {
            operation: "Email/set mailboxes (junk)",
        })?;
    Ok(())
}

/// Moves a message to Trash rather than permanently destroying it via
/// Email/destroy: "Supprimer" in a mail client conventionally means
/// recoverable-for-now, not gone-forever-with-no-undo, and a permanent
/// destroy would make /undo meaningless for this action specifically.
pub async fn delete(client: &Client, id: &str) -> Result<()> {
    let Some(trash_id) = mailbox_with_role(client, Role::Trash).await? else {
        return Err(JmapError::NoTrashMailbox);
    };
    client
        .email_set_mailboxes(id, [trash_id])
        .await
        .map_err(|_| JmapError::Protocol {
            operation: "Email/set mailboxes (trash)",
        })?;
    Ok(())
}

pub const WATCHED_TYPES: [DataType; 1] = [DataType::Email];

#[cfg(test)]
mod tests {
    use super::*;
    use jmap_client::client::Credentials;
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn connect_rejects_non_https_url() {
        let err = connect("http://jmap.example.org/session", "tok", false)
            .await
            .err()
            .expect("non-https URL must be rejected");
        assert!(err.to_string().contains("https://"));
    }

    #[test]
    fn jmap_errors_render_english_for_logs_and_localized_for_the_chat() {
        let err = JmapError::NoArchiveMailbox;
        // Log-facing Display stays English whichever language the chat uses.
        assert_eq!(err.to_string(), "this account has no JMAP Archive folder");
        assert_eq!(
            err.localized("en"),
            "this account has no JMAP Archive folder"
        );
        assert_eq!(
            err.localized("fr"),
            "ce compte n'a pas de dossier Archive JMAP"
        );

        // The network failure stays generic in both locales (no transport
        // detail) while still being translated.
        let unreachable = JmapError::Unreachable;
        assert!(unreachable.localized("en").contains("unreachable"));
        assert!(unreachable.localized("fr").contains("injoignable"));
    }

    #[test]
    fn private_host_error_names_the_input_and_the_opt_in() {
        let err = JmapError::PrivateHost {
            host: "mail.internal".to_string(),
            ip: "10.0.0.5".to_string(),
        };
        let fr = err.localized("fr");
        // Actionable on purpose: it describes the user's own input, not a
        // probe result, so host/ip and the escape hatch are worth showing.
        assert!(fr.contains("mail.internal"));
        assert!(fr.contains("10.0.0.5"));
        assert!(fr.contains("ALLOW_PRIVATE_JMAP_HOSTS"));
    }

    #[test]
    fn localized_error_recovers_the_typed_error_through_an_anyhow_wrapper() {
        // The presentation path wraps `jmap::*` errors in `anyhow` (e.g.
        // `state.client_for`); the localized rendering must still apply and
        // stay generic - a transport leak must not reappear via the
        // fallback `Display`.
        let wrapped: anyhow::Error = JmapError::Unreachable.into();
        assert_eq!(
            localized_error(&wrapped, "fr"),
            "serveur JMAP injoignable ou invalide (détail dans les logs serveur)"
        );

        // A non-`JmapError` anyhow error falls back to its English message.
        let other = anyhow::anyhow!("store unavailable");
        assert_eq!(localized_error(&other, "fr"), "store unavailable");
    }

    #[test]
    fn every_jmap_error_variant_renders_in_english_and_french() {
        // Covers the whole `Display`/`localized` match surface: every
        // variant must have a stable English rendering (for logs) and a
        // translated one for chat, with no raw catalog key leaking out.
        let variants = [
            JmapError::InsecureUrl,
            JmapError::InvalidUrl,
            JmapError::NoHost,
            JmapError::PrivateHost {
                host: "mail.internal".to_string(),
                ip: "10.0.0.5".to_string(),
            },
            JmapError::Unreachable,
            JmapError::Protocol {
                operation: "Email/get",
            },
            JmapError::MessageNotFound,
            JmapError::NoArchiveMailbox,
            JmapError::NoJunkMailbox,
            JmapError::NoTrashMailbox,
        ];

        for err in &variants {
            assert!(
                !err.to_string().is_empty(),
                "no English Display for {err:?}"
            );
            let en = err.localized("en");
            let fr = err.localized("fr");
            assert!(
                !en.is_empty() && !fr.is_empty(),
                "empty rendering for {err:?}"
            );
            assert!(!en.starts_with("jmap."), "raw key leaked: {en}");
            assert!(!fr.starts_with("jmap."), "raw key leaked: {fr}");
            // The two locales are genuinely different catalog entries, never
            // a silent English fallback.
            assert_ne!(en, fr, "untranslated variant: {err:?}");
        }
    }

    #[tokio::test]
    async fn connect_rejects_loopback_host_by_default() {
        let err = connect("https://127.0.0.1", "tok", false)
            .await
            .err()
            .expect("loopback address must be rejected by default (SSRF guard)");
        assert!(err.to_string().contains("SSRF") || err.to_string().contains("non publique"));
    }

    #[tokio::test]
    async fn connect_rejects_cloud_metadata_address_by_default() {
        // 169.254.169.254 is the cloud-provider metadata endpoint commonly
        // targeted by SSRF exploits; it falls under is_link_local().
        let err = connect("https://169.254.169.254", "tok", false)
            .await
            .err()
            .expect("link-local/metadata address must be rejected by default");
        assert!(err.to_string().contains("SSRF"));
    }

    #[tokio::test]
    async fn connect_rejects_private_lan_host_by_default() {
        let err = connect("https://10.0.0.5", "tok", false)
            .await
            .err()
            .expect("RFC1918 address must be rejected by default");
        assert!(err.to_string().contains("SSRF"));
    }

    #[tokio::test]
    async fn connect_allows_loopback_when_opted_in() {
        // With the guard disabled, the request should get past the SSRF
        // check and fail for a *different* reason (nothing listening),
        // proving the guard itself was bypassed as intended rather than
        // some other check silently blocking it too.
        let err = connect("https://127.0.0.1:1", "tok", true)
            .await
            .err()
            .expect("nothing listens on port 1, connection should still fail");
        assert!(!err.to_string().contains("SSRF"));
    }

    /// The chat-facing error for a network-level connect failure must be
    /// a single fixed generic message. reqwest's raw transport error -
    /// which jmap-client renders verbatim as "Transport error: …" -
    /// names the full URL and the OS-level TCP cause (e.g. "Connection
    /// refused (os error 111)" vs "timed out"), and that combination
    /// fingerprints exactly one port state on whatever the hostname
    /// resolved to for *this* request. Returning it to the chat would
    /// turn `/login` into an internal-port-reachability oracle (see
    /// SECURITY.md, "SSRF via /login"); the cause goes to the server-side
    /// log instead.
    #[tokio::test]
    async fn connect_failure_error_message_carries_no_transport_detail() {
        // Loopback opted into so the SSRF guard itself is out of the way
        // (its refusal is a deliberate policy notice, covered by its own
        // tests above) and the failure is pure transport: nothing listens
        // on port 1, so the TCP connect fails with ECONNREFUSED.
        let err = connect("https://127.0.0.1:1", "tok", true)
            .await
            .err()
            .expect("nothing listens on port 1, connect must fail");
        let msg = err.to_string();
        assert!(
            msg.contains("unreachable") && msg.contains("server logs"),
            "expected the fixed generic message, got: {msg}"
        );
        // Nothing that could fingerprint the host, port, URL or
        // transport-layer cause - including the whole error chain, not
        // just the outermost message: the cause is dropped, not attached.
        for fragment in [
            "127.0.0.1",
            "/.well-known/jmap",
            "error sending request",
            "Connection refused",
            "tcp connect",
            "os error",
            "Transport error",
            "https://127.0.0.1",
        ] {
            assert!(!msg.contains(fragment), "error-oracle leak in: {msg}");
        }
        let mut chain: Vec<String> = Vec::new();
        let mut current: Option<&dyn std::error::Error> = Some(&err);
        while let Some(e) = current {
            chain.push(e.to_string());
            current = std::error::Error::source(e);
        }
        for fragment in ["Connection refused", "Transport error", "os error"] {
            assert!(
                !chain.iter().any(|c| c.contains(fragment)),
                "error-oracle leak via cause chain: {chain:?}"
            );
        }
    }

    #[test]
    fn is_disallowed_host_flags_private_ranges() {
        assert!(is_disallowed_host("127.0.0.1".parse().unwrap()));
        assert!(is_disallowed_host("10.1.2.3".parse().unwrap()));
        assert!(is_disallowed_host("172.16.0.1".parse().unwrap()));
        assert!(is_disallowed_host("192.168.1.1".parse().unwrap()));
        assert!(is_disallowed_host("169.254.169.254".parse().unwrap()));
        assert!(is_disallowed_host("0.0.0.0".parse().unwrap()));
        assert!(is_disallowed_host("::1".parse().unwrap()));
        assert!(is_disallowed_host("fc00::1".parse().unwrap()));
        assert!(is_disallowed_host("fe80::1".parse().unwrap()));
        // IPv4-mapped IPv6 must not bypass the IPv4 checks.
        assert!(is_disallowed_host("::ffff:127.0.0.1".parse().unwrap()));
        // RFC 6598 CGNAT/shared address space (100.64.0.0/10) - not covered
        // by is_private(), used as internal-network space by some
        // deployments (e.g. Tailscale's default overlay range).
        assert!(is_disallowed_host("100.64.0.1".parse().unwrap()));
        assert!(is_disallowed_host("100.100.100.100".parse().unwrap()));
        assert!(is_disallowed_host("100.127.255.255".parse().unwrap()));
    }

    #[test]
    fn is_disallowed_host_does_not_over_block_adjacent_public_ranges() {
        // Bit-math sanity check: 100.63.x.x and 100.128.x.x sit just
        // outside 100.64.0.0/10 and must stay allowed.
        assert!(!is_disallowed_host("100.63.0.1".parse().unwrap()));
        assert!(!is_disallowed_host("100.128.0.1".parse().unwrap()));
    }

    #[test]
    fn is_disallowed_host_allows_public_ranges() {
        assert!(!is_disallowed_host("1.1.1.1".parse().unwrap()));
        assert!(!is_disallowed_host("8.8.8.8".parse().unwrap()));
        assert!(!is_disallowed_host("2606:4700:4700::1111".parse().unwrap()));
    }

    fn session_body(mock_uri: &str) -> serde_json::Value {
        json!({
            "capabilities": {
                "urn:ietf:params:jmap:core": {
                    "maxSizeUpload": 50000000,
                    "maxConcurrentUpload": 4,
                    "maxSizeRequest": 10000000,
                    "maxConcurrentRequests": 4,
                    "maxCallsInRequest": 16,
                    "maxObjectsInGet": 500,
                    "maxObjectsInSet": 500,
                    "collationAlgorithms": []
                }
            },
            "accounts": {
                "acc1": {
                    "name": "user@example.org",
                    "isPersonal": true,
                    "isReadOnly": false,
                    "accountCapabilities": {}
                }
            },
            "primaryAccounts": { "urn:ietf:params:jmap:core": "acc1" },
            "username": "user@example.org",
            "apiUrl": format!("{mock_uri}/jmap/api"),
            "downloadUrl": format!("{mock_uri}/jmap/download/{{accountId}}/{{blobId}}/{{name}}"),
            "uploadUrl": format!("{mock_uri}/jmap/upload/{{accountId}}"),
            "eventSourceUrl": format!("{mock_uri}/jmap/eventsource"),
            "state": "session-state-1"
        })
    }

    /// Exercises the real network boundary: a bearer-token-authenticated
    /// session negotiation against a mock JMAP server, followed by the
    /// Email/get(ids: []) call `current_email_state` builds to establish
    /// the sync baseline. This is the integration path every /login
    /// depends on.
    #[tokio::test]
    async fn session_bootstrap_and_state_query_against_mock_server() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/.well-known/jmap"))
            .respond_with(ResponseTemplate::new(200).set_body_json(session_body(&server.uri())))
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(path("/jmap/api"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "methodResponses": [[
                    "Email/get",
                    {
                        "accountId": "acc1",
                        "state": "email-state-1",
                        "list": [],
                        "notFound": []
                    },
                    "s0"
                ]],
                "sessionState": "session-state-1"
            })))
            .mount(&server)
            .await;

        // Bypasses this module's https-only guard on purpose: that guard
        // is a one-line policy check already covered by
        // `connect_rejects_non_https_url`, and fighting TLS certificates
        // for a local mock server would test wiremock, not our code.
        let client = Client::new()
            .credentials(Credentials::bearer("test-token-123"))
            .connect(&server.uri())
            .await
            .expect("mock session negotiation should succeed");

        assert_eq!(account_email(&client), "user@example.org");

        let state = current_email_state(&client)
            .await
            .expect("Email/get(ids: []) should succeed against the mock");
        assert_eq!(state, "email-state-1");
    }

    /// Regression test: Stalwart (and likely other JMAP servers) 30x-
    /// redirects `/.well-known/jmap` to a same-host session path (e.g.
    /// `/jmap/session`) instead of serving the session object directly.
    /// jmap-client's `connect()` only follows a redirect to a host in its
    /// explicit `follow_redirects` allowlist, aborting anything else - our
    /// `connect()` wrapper must populate that allowlist with the server's
    /// own host, or every provider that redirects like this fails to log
    /// in. Caught live against a real Stalwart deployment before this test
    /// existed.
    #[tokio::test]
    async fn connect_follows_same_host_redirect_from_well_known_jmap() {
        let server = MockServer::start().await;
        let host = url::Url::parse(&server.uri())
            .unwrap()
            .host_str()
            .unwrap()
            .to_string();

        Mock::given(method("GET"))
            .and(path("/.well-known/jmap"))
            .respond_with(ResponseTemplate::new(307).insert_header("Location", "/jmap/session"))
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/jmap/session"))
            .respond_with(ResponseTemplate::new(200).set_body_json(session_body(&server.uri())))
            .mount(&server)
            .await;

        // Mirrors this module's connect(): trusts only the server's own host.
        let client = Client::new()
            .credentials(Credentials::bearer("test-token-123"))
            .follow_redirects([host])
            .connect(&server.uri())
            .await
            .expect("same-host redirect from /.well-known/jmap must be followed");

        assert_eq!(account_email(&client), "user@example.org");
    }

    fn session_body_with_shared_accounts(mock_uri: &str) -> serde_json::Value {
        let mut body = session_body(mock_uri);
        body["accounts"]["shared1"] = json!({
            "name": "contact@delta-net.ovh",
            "isPersonal": false,
            "isReadOnly": false,
            "accountCapabilities": {}
        });
        body["accounts"]["shared2"] = json!({
            "name": "contact@cardinalcodes.com",
            "isPersonal": false,
            "isReadOnly": true,
            "accountCapabilities": {}
        });
        body
    }

    #[tokio::test]
    async fn list_shared_accounts_excludes_the_personal_account() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/.well-known/jmap"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(session_body_with_shared_accounts(&server.uri())),
            )
            .mount(&server)
            .await;

        let client = Client::new()
            .credentials(Credentials::bearer("test-token-123"))
            .connect(&server.uri())
            .await
            .expect("mock session negotiation should succeed");

        let mut shared = list_shared_accounts(&client);
        shared.sort();
        assert_eq!(
            shared,
            vec![
                ("shared1".to_string(), "contact@delta-net.ovh".to_string()),
                (
                    "shared2".to_string(),
                    "contact@cardinalcodes.com".to_string()
                ),
            ]
        );
        // The personal account ("acc1") must never show up as "shared".
        assert!(!shared.iter().any(|(id, _)| id == "acc1"));
    }

    /// `connect_shared` itself can't be exercised against a plain-http mock
    /// server (it enforces the same https-only guard as `connect`, already
    /// covered by `connect_rejects_non_https_url`); this verifies the
    /// underlying `set_default_account_id` mechanism it relies on.
    #[tokio::test]
    async fn set_default_account_id_switches_the_active_account() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/.well-known/jmap"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(session_body_with_shared_accounts(&server.uri())),
            )
            .mount(&server)
            .await;

        // Bypasses this module's https-only guard on purpose, same as the
        // other mock-server tests above.
        let mut client = Client::new()
            .credentials(Credentials::bearer("test-token-123"))
            .connect(&server.uri())
            .await
            .expect("mock session negotiation should succeed");
        assert_eq!(client.default_account_id(), "acc1");

        client.set_default_account_id("shared1");
        assert_eq!(client.default_account_id(), "shared1");
    }

    #[test]
    fn is_disallowed_v4_flags_every_internal_range_and_only_those() {
        // Each of these is a policy refusal the SSRF guard must make:
        // RFC1918 private, loopback, link-local (incl. the
        // 169.254.169.254 metadata endpoint), unspecified, multicast,
        // broadcast, documentation, and RFC6598 CGNAT space.
        for addr in [
            "10.0.0.1",
            "172.16.0.1",
            "192.168.1.1",
            "127.0.0.1",
            "169.254.169.254",
            "0.0.0.0",
            "224.0.0.1",
            "255.255.255.255",
            "192.0.2.1",
            "100.64.0.1",
            "100.127.255.254",
        ] {
            assert!(
                is_disallowed_v4(addr.parse().unwrap()),
                "{addr} must be refused"
            );
        }
        // ...and these are ordinary public addresses, allowed. The two
        // 100.x rows straddle the CGNAT range on purpose.
        for addr in [
            "8.8.8.8",
            "1.1.1.1",
            "93.184.216.34",
            "100.63.255.255",
            "100.128.0.1",
        ] {
            assert!(
                !is_disallowed_v4(addr.parse().unwrap()),
                "{addr} must be allowed"
            );
        }
    }

    #[test]
    fn is_disallowed_host_flags_every_internal_ipv6_range() {
        for addr in [
            "::1",              // loopback
            "::",               // unspecified
            "ff02::1",          // multicast
            "fd00::1",          // unique local fc00::/7
            "fe80::1",          // link-local unicast fe80::/10
            "::ffff:10.0.0.1",  // IPv4-mapped private
            "::ffff:127.0.0.1", // IPv4-mapped loopback
        ] {
            let ip: IpAddr = addr.parse().unwrap();
            assert!(is_disallowed_host(ip), "{addr} must be refused");
        }
        assert!(!is_disallowed_host("2606:4700:4700::1111".parse().unwrap()));
    }

    #[tokio::test]
    async fn assert_public_host_accepts_a_literal_public_address() {
        // A numeric host resolves without any DNS round-trip, so this
        // reaches the "resolved, and nothing disallowed" branch and
        // succeeds - a guard against it ever returning the wrong verdict
        // for a genuinely public, already-resolved host.
        assert_public_host("8.8.8.8", "https://8.8.8.8")
            .await
            .expect("a public literal address must pass the guard");
    }

    #[tokio::test]
    async fn assert_public_host_refuses_a_literal_private_address() {
        let err = assert_public_host("127.0.0.1", "https://127.0.0.1")
            .await
            .expect_err("a loopback literal must be refused");
        assert!(matches!(err, JmapError::PrivateHost { .. }), "got {err:?}");
    }

    /// Answers a JMAP API POST with a canned response per method name,
    /// echoing the request's call id back as the protocol requires.
    /// jmap-client sends one method per POST here, so keying on the method
    /// name (rather than on call ordering) is enough.
    struct JmapApi {
        by_method: serde_json::Map<String, serde_json::Value>,
    }

    impl wiremock::Respond for JmapApi {
        fn respond(&self, request: &wiremock::Request) -> ResponseTemplate {
            let body: serde_json::Value =
                serde_json::from_slice(&request.body).unwrap_or_else(|_| json!({}));
            let call = &body["methodCalls"][0];
            let method = call[0].as_str().unwrap_or_default();
            let call_id = call[2].clone();
            let args = self
                .by_method
                .get(method)
                .cloned()
                .unwrap_or_else(|| json!({}));
            // jmap-client's API envelope requires `sessionState`
            // (RFC 8620 §3.4) alongside `methodResponses`.
            ResponseTemplate::new(200).set_body_json(json!({
                "methodResponses": [[method, args, call_id]],
                "sessionState": "session-state-1"
            }))
        }
    }

    /// A client connected to `server`, whose API calls are answered from
    /// `responses` (a JSON object mapping method name to response
    /// arguments). Bypasses this module's https-only guard on purpose,
    /// same as the other mock-server tests.
    async fn mock_client(server: &MockServer, responses: serde_json::Value) -> Client {
        Mock::given(method("GET"))
            .and(path("/.well-known/jmap"))
            .respond_with(ResponseTemplate::new(200).set_body_json(session_body(&server.uri())))
            .mount(server)
            .await;

        let by_method = responses
            .as_object()
            .expect("responses must be a JSON object")
            .clone();
        Mock::given(method("POST"))
            .and(path("/jmap/api"))
            .respond_with(JmapApi { by_method })
            .mount(server)
            .await;

        Client::new()
            .credentials(Credentials::bearer("tok"))
            .connect(&server.uri())
            .await
            .expect("mock session should connect")
    }

    /// The arguments of the last `Email/set` call the server received.
    async fn last_email_set(server: &MockServer) -> serde_json::Value {
        server
            .received_requests()
            .await
            .expect("requests should be recorded")
            .into_iter()
            .filter_map(|r| serde_json::from_slice::<serde_json::Value>(&r.body).ok())
            .rfind(|b| b["methodCalls"][0][0] == "Email/set")
            .expect("an Email/set call should have been sent")["methodCalls"][0][1]
            .clone()
    }

    fn email_summary_json() -> serde_json::Value {
        json!({
            "id": "m1",
            "subject": "Invoice",
            "preview": "Please find attached...",
            "receivedAt": "2024-01-02T03:04:05Z",
            "from": [{"name": "Alice", "email": "alice@example.org"}],
            "attachments": [
                {"name": "invoice.pdf", "size": 12345},
                {"name": "notes.txt", "size": 67}
            ]
        })
    }

    #[tokio::test]
    async fn search_returns_summaries_with_their_attachments() {
        let server = MockServer::start().await;
        let client = mock_client(
            &server,
            json!({
                "Email/query": {
                    "accountId": "acc1", "queryState": "q1",
                    "canCalculateChanges": false, "position": 0,
                    "ids": ["m1"], "total": 1
                },
                "Email/get": {
                    "accountId": "acc1", "state": "s", "notFound": [],
                    "list": [email_summary_json()]
                }
            }),
        )
        .await;

        let results = search(&client, "invoice").await.unwrap();
        assert_eq!(results.len(), 1, "the query hit must come back");
        let summary = &results[0];
        assert_eq!(summary.id, "m1");
        assert_eq!(summary.subject, "Invoice");
        assert_eq!(summary.from_name.as_deref(), Some("Alice"));
        assert_eq!(summary.from_addr.as_deref(), Some("alice@example.org"));
        assert_eq!(
            summary.attachments,
            vec![
                ("invoice.pdf".to_string(), 12345),
                ("notes.txt".to_string(), 67)
            ],
            "attachment names and sizes must survive the mapping"
        );
    }

    #[tokio::test]
    async fn fetch_changed_emails_returns_summaries_and_the_new_cursor() {
        let server = MockServer::start().await;
        let client = mock_client(
            &server,
            json!({
                "Email/changes": {
                    "accountId": "acc1", "oldState": "s1", "newState": "s2",
                    "hasMoreChanges": false, "created": ["m1"],
                    "updated": [], "destroyed": []
                },
                "Email/get": {
                    "accountId": "acc1", "state": "s2", "notFound": [],
                    "list": [email_summary_json()]
                }
            }),
        )
        .await;

        let (summaries, new_state) = fetch_changed_emails(&client, "s1").await.unwrap();
        assert_eq!(new_state, "s2", "the new cursor must be persisted as-is");
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].id, "m1");
        assert_eq!(summaries[0].subject, "Invoice");
        assert_eq!(summaries[0].attachments.len(), 2);
    }

    #[tokio::test]
    async fn fetch_full_text_reads_the_body_then_falls_back_to_the_preview() {
        let server = MockServer::start().await;
        let client = mock_client(
            &server,
            json!({
                "Email/get": {
                    "accountId": "acc1", "state": "s", "notFound": [],
                    "list": [{
                        "id": "m1",
                        "preview": "snippet only",
                        "textBody": [{"partId": "1"}],
                        "bodyValues": {"1": {"value": "Hello, world."}}
                    }]
                }
            }),
        )
        .await;
        assert_eq!(
            fetch_full_text(&client, "m1").await.unwrap(),
            "Hello, world."
        );

        // No body parts at all: the preview is the fallback.
        let preview_server = MockServer::start().await;
        let preview_client = mock_client(
            &preview_server,
            json!({
                "Email/get": {
                    "accountId": "acc1", "state": "s", "notFound": [],
                    "list": [{"id": "m1", "preview": "snippet only"}]
                }
            }),
        )
        .await;
        assert_eq!(
            fetch_full_text(&preview_client, "m1").await.unwrap(),
            "snippet only"
        );

        // An empty list means the message is gone.
        let empty_server = MockServer::start().await;
        let empty_client = mock_client(
            &empty_server,
            json!({
                "Email/get": {
                    "accountId": "acc1", "state": "s", "notFound": [], "list": []
                }
            }),
        )
        .await;
        assert!(matches!(
            fetch_full_text(&empty_client, "m1").await.unwrap_err(),
            JmapError::MessageNotFound
        ));
    }

    #[tokio::test]
    async fn archive_moves_the_message_to_the_archive_mailbox() {
        let server = MockServer::start().await;
        let client = mock_client(
            &server,
            json!({
                "Mailbox/query": {
                    "accountId": "acc1", "queryState": "q1",
                    "canCalculateChanges": false, "position": 0,
                    "ids": ["arch"], "total": 1
                },
                "Email/set": {
                    "accountId": "acc1", "oldState": "s", "newState": "s2",
                    "updated": {"m1": null}
                }
            }),
        )
        .await;

        archive(&client, "m1").await.unwrap();
        let set = last_email_set(&server).await;
        assert_eq!(
            set["update"]["m1"]["mailboxIds"],
            json!({"arch": true}),
            "the message must land in the resolved Archive mailbox"
        );
    }

    #[tokio::test]
    async fn junk_and_delete_resolve_their_own_role_mailboxes() {
        for (role_id, verb) in [("junkbox", "junk"), ("trashbox", "delete")] {
            let server = MockServer::start().await;
            let client = mock_client(
                &server,
                json!({
                    "Mailbox/query": {
                        "accountId": "acc1", "queryState": "q1",
                        "canCalculateChanges": false, "position": 0,
                        "ids": [role_id], "total": 1
                    },
                    "Email/set": {
                        "accountId": "acc1", "oldState": "s", "newState": "s2",
                        "updated": {"m1": null}
                    }
                }),
            )
            .await;

            match verb {
                "junk" => junk(&client, "m1").await.unwrap(),
                _ => delete(&client, "m1").await.unwrap(),
            }
            let set = last_email_set(&server).await;
            assert_eq!(
                set["update"]["m1"]["mailboxIds"],
                json!({ role_id: true }),
                "{verb} must use its resolved mailbox"
            );
        }
    }

    #[tokio::test]
    async fn a_missing_role_mailbox_is_reported_rather_than_silently_ignored() {
        // No Mailbox/query hit for the role: the verb must surface its
        // typed "no such folder" error instead of appearing to succeed.
        for responses in [
            ("Mailbox/query", Role::Archive, JmapError::NoArchiveMailbox),
            ("Mailbox/query", Role::Junk, JmapError::NoJunkMailbox),
            ("Mailbox/query", Role::Trash, JmapError::NoTrashMailbox),
        ] {
            let (_method, role, expected) = responses;
            let server = MockServer::start().await;
            let client = mock_client(
                &server,
                json!({
                    "Mailbox/query": {
                        "accountId": "acc1", "queryState": "q1",
                        "canCalculateChanges": false, "position": 0,
                        "ids": [], "total": 0
                    }
                }),
            )
            .await;

            let err = match role {
                Role::Archive => archive(&client, "m1").await.unwrap_err(),
                Role::Junk => junk(&client, "m1").await.unwrap_err(),
                _ => delete(&client, "m1").await.unwrap_err(),
            };
            assert_eq!(
                std::mem::discriminant(&err),
                std::mem::discriminant(&expected),
                "wrong error for {role:?}: {err:?}"
            );
        }
    }

    #[tokio::test]
    async fn mark_read_sets_the_seen_keyword() {
        let server = MockServer::start().await;
        let client = mock_client(
            &server,
            json!({
                "Email/set": {
                    "accountId": "acc1", "oldState": "s", "newState": "s2",
                    "updated": {"m1": null}
                }
            }),
        )
        .await;

        mark_read(&client, "m1").await.unwrap();
        let update = serde_json::to_string(&last_email_set(&server).await["update"]).unwrap();
        assert!(
            update.contains("$seen"),
            "the seen keyword must be set: {update}"
        );
    }

    #[tokio::test]
    async fn mailbox_ids_of_snapshots_every_mailbox_the_message_is_in() {
        let server = MockServer::start().await;
        let client = mock_client(
            &server,
            json!({
                "Email/get": {
                    "accountId": "acc1", "state": "s", "notFound": [],
                    "list": [{
                        "id": "m1",
                        "mailboxIds": {"inbox": true, "archive": true, "hidden": false}
                    }]
                }
            }),
        )
        .await;

        let mut ids = mailbox_ids_of(&client, "m1").await.unwrap();
        ids.sort();
        assert_eq!(ids, vec!["archive".to_string(), "inbox".to_string()]);
    }

    #[tokio::test]
    async fn restore_mailboxes_puts_the_snapshot_back_exactly() {
        let server = MockServer::start().await;
        let client = mock_client(
            &server,
            json!({
                "Email/set": {
                    "accountId": "acc1", "oldState": "s", "newState": "s2",
                    "updated": {"m1": null}
                }
            }),
        )
        .await;

        restore_mailboxes(
            &client,
            "m1",
            vec!["inbox".to_string(), "archive".to_string()],
        )
        .await
        .unwrap();
        let set = last_email_set(&server).await;
        assert_eq!(
            set["update"]["m1"]["mailboxIds"],
            json!({"inbox": true, "archive": true}),
            "undo must restore the exact mailbox set"
        );
    }
}
