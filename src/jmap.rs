use std::net::{IpAddr, Ipv4Addr};

use anyhow::{Context, Result, bail};
use jmap_client::DataType;
use jmap_client::client::{Client, Credentials};
use jmap_client::core::response::EmailGetResponse;
use jmap_client::email::Property;
use jmap_client::mailbox::{self, Role};

pub struct EmailSummary {
    pub id: String,
    pub subject: String,
    pub from_name: Option<String>,
    pub from_addr: Option<String>,
    pub preview: String,
    pub received_at: Option<i64>,
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
/// refused before we ever send it the bearer token — otherwise this
/// would be a ready-made SSRF primitive against the deployment's internal
/// network (cluster services, cloud metadata endpoints, ...).
pub async fn connect(server_url: &str, token: &str, allow_private_hosts: bool) -> Result<Client> {
    if !server_url.starts_with("https://") {
        bail!("l'URL du serveur JMAP doit commencer par https://");
    }

    if !allow_private_hosts {
        assert_public_host(server_url).await?;
    }

    let client = Client::new()
        .credentials(Credentials::bearer(token))
        .connect(server_url)
        .await
        .context("connexion/authentification JMAP échouée")?;

    Ok(client)
}

async fn assert_public_host(server_url: &str) -> Result<()> {
    let parsed = url::Url::parse(server_url).context("URL de serveur JMAP invalide")?;
    let host = parsed
        .host_str()
        .context("URL de serveur JMAP sans nom d'hôte")?
        .to_string();
    let port = parsed.port_or_known_default().unwrap_or(443);

    let addrs = tokio::net::lookup_host((host.as_str(), port))
        .await
        .with_context(|| format!("résolution DNS impossible pour {host}"))?;

    let mut resolved_any = false;
    for addr in addrs {
        resolved_any = true;
        if is_disallowed_host(addr.ip()) {
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
        bail!("la résolution DNS de '{host}' n'a retourné aucune adresse");
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
            });
        }
    }

    Ok((summaries, new_state))
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

pub async fn delete(client: &Client, id: &str) -> Result<()> {
    client
        .email_destroy(id)
        .await
        .context("échec de la suppression")?;
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
}
