use std::net::{IpAddr, Ipv4Addr};

use anyhow::{Context, Result, anyhow, bail};
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

/// The single fixed message the chat gets for *any* network-level JMAP
/// failure (DNS resolution, TCP/TLS connect, autodiscovery, session
/// negotiation, authentication). The raw transport error, by contrast,
/// fingerprints exactly one port state ("connection refused" vs "timed
/// out" vs a TLS/HTTP error, each rendered by reqwest with the full URL
/// and OS-level TCP cause), so echoing it back to the chat would turn
/// `/login` against an attacker-chosen hostname - or a watcher reconnect
/// after a DNS-rebounding TOCTOU - into an internal-port-reachability
/// oracle (see SECURITY.md). The real cause is logged server-side instead;
/// callers must never attach it back to the error they return, or the
/// chain becomes one `{:?}` format away from the same leak.
const UNREACHABLE_SERVER_MSG: &str =
    "serveur JMAP injoignable ou invalide (détail dans les logs serveur)";

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
        .map(|a| (a.name().unwrap_or("(sans nom)").to_string(), a.size()))
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
        bail!("l'URL du serveur JMAP doit commencer par https://");
    }

    let host = url::Url::parse(server_url)
        .context("URL de serveur JMAP invalide")?
        .host_str()
        .context("URL de serveur JMAP sans nom d'hôte")?
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
            // generic error - bare, so the dropped cause can't be
            // resurrected from the error chain either.
            tracing::warn!(
                host = %host,
                error = %e,
                "JMAP session negotiation failed (details kept out of the chat)"
            );
            anyhow!(UNREACHABLE_SERVER_MSG)
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
    let parsed = url::Url::parse(server_url).context("URL de serveur JMAP invalide")?;
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
            bail!(UNREACHABLE_SERVER_MSG);
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
            bail!(
                "le serveur JMAP '{host}' se résout vers une adresse non publique ({}) : \
                 refusé pour éviter une attaque SSRF contre le réseau interne du déploiement. \
                 Si c'est un serveur auto-hébergé volontairement sur un réseau privé, active \
                 ALLOW_PRIVATE_JMAP_HOSTS=1.",
                addr.ip()
            );
        }
    }
    if !resolved_any {
        tracing::warn!(
            host = %host,
            "DNS resolution of the JMAP server returned no addresses (details kept out of the chat)"
        );
        bail!(UNREACHABLE_SERVER_MSG);
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
        .context("échec de l'initialisation de l'état JMAP")?;
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
        .context("Email/changes a échoué")?;
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
            .context("Email/get a échoué")?
        {
            let (from_name, from_addr) = email
                .from()
                .and_then(|addrs| addrs.first())
                .map(|a| (a.name().map(str::to_string), Some(a.email().to_string())))
                .unwrap_or((None, None));

            summaries.push(EmailSummary {
                id,
                subject: email.subject().unwrap_or("(sans objet)").to_string(),
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
        .context("Email/query a échoué")?;
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
        .context("Email/get (recherche) a échoué")?;

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
                subject: email.subject().unwrap_or("(sans objet)").to_string(),
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
        .context("Email/get (corps complet) a échoué")?;

    let Some(email) = response.take_list().pop() else {
        bail!("message introuvable (peut-être déjà supprimé)");
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
        text = email.preview().unwrap_or("(message vide)").to_string();
    }
    Ok(text)
}

async fn mailbox_with_role(client: &Client, role: Role) -> Result<Option<String>> {
    let response = client
        .mailbox_query(Some(mailbox::query::Filter::role(role)), None::<Vec<_>>)
        .await
        .context("Mailbox/query a échoué")?;
    Ok(response.ids().first().cloned())
}

pub async fn mark_read(client: &Client, id: &str) -> Result<()> {
    client
        .email_set_keyword(id, "$seen", true)
        .await
        .context("échec du marquage comme lu")?;
    Ok(())
}

/// The mailbox ids a message currently sits in, taken right before a
/// triage action moves it elsewhere - the snapshot `/undo` restores.
pub async fn mailbox_ids_of(client: &Client, id: &str) -> Result<Vec<String>> {
    let email = client
        .email_get(id, Some([Property::MailboxIds]))
        .await
        .context("Email/get (snapshot pour /undo) a échoué")?
        .context("message introuvable (peut-être déjà supprimé)")?;
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
        .context("échec de la restauration")?;
    Ok(())
}

pub async fn archive(client: &Client, id: &str) -> Result<()> {
    let Some(archive_id) = mailbox_with_role(client, Role::Archive).await? else {
        bail!("ce compte n'a pas de dossier Archive JMAP");
    };
    client
        .email_set_mailboxes(id, [archive_id])
        .await
        .context("échec de l'archivage")?;
    Ok(())
}

pub async fn junk(client: &Client, id: &str) -> Result<()> {
    let Some(junk_id) = mailbox_with_role(client, Role::Junk).await? else {
        bail!("ce compte n'a pas de dossier Spam/Junk JMAP");
    };
    client
        .email_set_mailboxes(id, [junk_id])
        .await
        .context("échec du marquage comme spam")?;
    Ok(())
}

/// Moves a message to Trash rather than permanently destroying it via
/// Email/destroy: "Supprimer" in a mail client conventionally means
/// recoverable-for-now, not gone-forever-with-no-undo, and a permanent
/// destroy would make /undo meaningless for this action specifically.
pub async fn delete(client: &Client, id: &str) -> Result<()> {
    let Some(trash_id) = mailbox_with_role(client, Role::Trash).await? else {
        bail!("ce compte n'a pas de dossier Corbeille JMAP");
    };
    client
        .email_set_mailboxes(id, [trash_id])
        .await
        .context("échec de la mise à la corbeille")?;
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
        assert!(err.to_string().contains("non publique"));
    }

    #[tokio::test]
    async fn connect_rejects_private_lan_host_by_default() {
        let err = connect("https://10.0.0.5", "tok", false)
            .await
            .err()
            .expect("RFC1918 address must be rejected by default");
        assert!(err.to_string().contains("non publique"));
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
        assert!(!err.to_string().contains("non publique"));
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
            msg.contains("injoignable") && msg.contains("logs serveur"),
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
        let chain: Vec<String> = err.chain().map(|e| e.to_string()).collect();
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
}
