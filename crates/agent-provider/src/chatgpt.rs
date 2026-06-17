//! Adapter `OpenAiChatGpt` — abonnement ChatGPT via la Responses API sur le
//! backend ChatGPT/Codex (ADR-10). Implémente `agent_core::Provider`.
//!
//! **SSE stateless** : `server_side_state = false` → pas de `previous_response_id`,
//! contexte complet reconstruit côté client à chaque tour → mappe proprement le
//! canonique (PROVIDERS §4.1, le piège WebSocket+state est explicitement évité).
//!
//! ⚠️ Risque #1 à valider au premier run live (non testable ici, pas de token) :
//! le backend est un modèle à raisonnement et reçoit `include:
//! ["reasoning.encrypted_content"]`. Le MVP **n'réinjecte pas** les reasoning
//! items aux tours suivants. Si le backend rejette (`400` « reasoning item
//! required ») un `function_call` non précédé de son reasoning item, le fix est
//! borné : porter le `thinkingSignature` de Pi (capturer l'item reasoning à
//! `output_item.done`, le stocker dans le transcript, le réémettre dans
//! `input[]`). Voir `docs/openai-subscription-auth.md` §1.b.

use std::time::Duration;

use agent_auth::OAuthCredential;
use agent_core::message::ContentBlock;
use agent_core::provider::{
    AuthError, CanonicalRequest, CanonicalResponse, Capabilities, ErrorClass, Provider,
    ProviderError, ProviderKind, StopReason, StreamEvent, TokenUsage,
};
use async_trait::async_trait;
use eventsource_stream::Eventsource;
use futures_util::Stream;
use futures_util::StreamExt;
use futures_util::stream::BoxStream;

use crate::chatgpt_events::CodexEventMapper;
use crate::chatgpt_request::{build_responses_body, inject_cache_key};
use crate::credential::CredentialManager;

/// Clé keyring de la credential abonnement ChatGPT (refresh rotatif réécrit ici).
pub const KEYRING_ACCOUNT: &str = "oauth:openai_chatgpt";

/// Fenêtre de contexte par défaut (modèles GPT-5.x du backend Codex). **Valeur
/// volatile/à ajuster** : n'affecte QUE les seuils de compaction ; un dépassement
/// réel déclenche la compaction réactive (413, withholding). Conservatrice.
pub const DEFAULT_MAX_CONTEXT: u32 = 256_000;

/// Effort de raisonnement par défaut (Codex CLI ≈ "medium").
pub const DEFAULT_REASONING_EFFORT: &str = "medium";

/// Slug de modèle par défaut. Le backend Codex (abonnement ChatGPT) impose une
/// liste blanche de slugs VERSIONNÉS qu'il fait évoluer (retraits fréquents) : le
/// slug générique `gpt-5` est rejeté en 400 ("not supported when using Codex with
/// a ChatGPT account"). **Valeur volatile** — surchargeable via `--model` ou la
/// commande `/models` en session (voir `agent_tui::MODELS`).
pub const DEFAULT_MODEL: &str = "gpt-5.5";

/// Borne du corps d'erreur HTTP capturé (évite un message géant en log).
const MAX_ERR_BODY: usize = 2000;

/// Timeout d'ÉTABLISSEMENT de connexion (US-022). Un backend qui n'établit jamais
/// la connexion (proxy d'entreprise, DNS noir) échoue ici plutôt que de geler ;
/// l'erreur `reqwest` est mappée `Transport` → classifiée `Retryable`. Pi : 20 s.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);

/// Idle timeout per-event par défaut (US-022). Un stream SSE OUVERT qui n'émet
/// plus aucun event (backend silencieux, queue) est annulé après ce délai →
/// `Stream("idle timeout")` (Retryable). Configurable par session (`with_idle_timeout`,
/// env `NUMEN_IDLE_TIMEOUT_SECS`). Pi : 20 s (header) ; Codex CLI : 300 s/event.
pub const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

pub struct OpenAiChatGptProvider {
    creds: CredentialManager,
    http: reqwest::Client,
    capabilities: Capabilities,
    reasoning_effort: Option<String>,
    /// Délai max sans event SSE avant annulation (US-022). Voir `DEFAULT_IDLE_TIMEOUT`.
    idle_timeout: Duration,
    /// Identifiant de session STABLE (UUID v4), envoyé en `prompt_cache_key` à
    /// chaque requête (US-029) → le backend réutilise son cache de préfixe.
    session_id: String,
    /// US-031 (P2, replay isolé) : réinjecter les reasoning items chiffrés ?
    /// **Défaut OFF** — jamais le chemin par défaut (risque 400, à valider en live).
    reasoning_replay: bool,
}

/// Génère un UUID v4 (RFC 4122) depuis 16 octets aléatoires. Évite la crate `uuid`
/// (réutilise `rand`, déjà au workspace).
fn new_session_id() -> String {
    use rand::RngCore;
    let mut b = [0u8; 16];
    rand::rng().fill_bytes(&mut b);
    b[6] = (b[6] & 0x0F) | 0x40; // version 4
    b[8] = (b[8] & 0x3F) | 0x80; // variant RFC 4122
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7], b[8], b[9], b[10], b[11], b[12], b[13], b[14],
        b[15]
    )
}

impl OpenAiChatGptProvider {
    /// Construit l'adapter depuis une credential OAuth déjà chargée (par la CLI,
    /// depuis le keyring). `max_context` pilote la compaction ; `reasoning_effort`
    /// = `None` omet le champ `reasoning`.
    pub fn new(cred: OAuthCredential, max_context: u32, reasoning_effort: Option<String>) -> Self {
        // US-022 : `connect_timeout` borne l'établissement TCP/TLS. Un échec de
        // `build()` (backend TLS indisponible) retombe sur le client par défaut —
        // jamais de panic (lint `panic = deny`).
        let http = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        let creds = CredentialManager::new(cred, http.clone(), KEYRING_ACCOUNT);
        Self {
            creds,
            http,
            capabilities: Capabilities {
                vision: true,
                tools: true,
                // caching implicite côté backend, non contrôlé explicitement.
                prompt_caching: false,
                reasoning: true,
                // CLÉ : SSE stateless → le canonique client-side mappe (PROVIDERS §4.1).
                server_side_state: false,
                max_context,
            },
            reasoning_effort,
            idle_timeout: DEFAULT_IDLE_TIMEOUT,
            session_id: new_session_id(),
            reasoning_replay: false,
        }
    }

    /// Constructeur de confort : défauts MVP (`DEFAULT_MAX_CONTEXT`, effort medium).
    pub fn from_credential(cred: OAuthCredential) -> Self {
        Self::new(
            cred,
            DEFAULT_MAX_CONTEXT,
            Some(DEFAULT_REASONING_EFFORT.to_string()),
        )
    }

    /// Surcharge l'idle timeout SSE (US-022). `Duration::ZERO` est ignoré (garde
    /// le défaut) pour qu'une valeur d'env aberrante ne désactive pas le watchdog.
    pub fn with_idle_timeout(mut self, idle: Duration) -> Self {
        if !idle.is_zero() {
            self.idle_timeout = idle;
        }
        self
    }

    /// Active/désactive le replay des reasoning items chiffrés (US-031, P2). OFF par
    /// défaut : activer expose au risque 400 (paire `rs`/`fc`), à valider en live.
    pub fn with_reasoning_replay(mut self, on: bool) -> Self {
        self.reasoning_replay = on;
        self
    }
}

/// Watchdog SSE (US-022) : enveloppe un flux d'events canoniques d'un timeout
/// per-event. Tant qu'un event arrive avant `idle`, il est relayé tel quel ; un
/// silence > `idle` (backend gelé) coupe le flux avec `Stream("idle timeout")`
/// (classifié `Retryable` → la boucle agent retry/abandonne, jamais de gel). Une
/// erreur amont est relayée puis termine le flux (parité avec le chemin direct).
fn idle_guarded<S>(
    mut inner: S,
    idle: Duration,
) -> impl Stream<Item = Result<StreamEvent, ProviderError>> + Send
where
    S: Stream<Item = Result<StreamEvent, ProviderError>> + Send + Unpin + 'static,
{
    async_stream::stream! {
        loop {
            match tokio::time::timeout(idle, inner.next()).await {
                // pas d'event depuis `idle` → backend silencieux.
                Err(_elapsed) => {
                    yield Err(ProviderError::Stream("idle timeout".to_string()));
                    return;
                }
                Ok(None) => break, // fin de flux normale.
                Ok(Some(item)) => {
                    let stop = item.is_err();
                    yield item;
                    if stop {
                        return;
                    }
                }
            }
        }
    }
}

/// Borne la phase HEADERS d'une requête (US-022 durcissement). `reqwest::send()`
/// se résout à la réception des headers de réponse → ce timeout NE coupe PAS le
/// long stream SSE qui suit (couvert séparément par `idle_guarded`). Un dépassement
/// (`Elapsed`) devient `Stream("header timeout")` → classifié `Retryable`, parité
/// avec l'idle timeout. Une erreur réseau de `send()` reste `Transport` (Retryable).
async fn send_with_header_timeout(
    rb: reqwest::RequestBuilder,
    timeout: Duration,
) -> Result<reqwest::Response, ProviderError> {
    tokio::time::timeout(timeout, rb.send())
        .await
        .map_err(|_elapsed| ProviderError::Stream("header timeout".to_string()))?
        .map_err(|e| ProviderError::Transport(e.to_string()))
}

/// Marqueurs d'un 429 TERMINAL (quota d'abonnement épuisé), dérivés de Pi
/// (`isTerminalRateLimitError`). Un 429 portant l'un d'eux ne doit JAMAIS être
/// retryé : la session ne grille pas ses tentatives ni ne harcèle un compte bloqué
/// (US-023, edge case #2). Comparaison ASCII lowercase (le corps est du JSON
/// d'erreur backend).
///
/// On garde uniquement les signaux NON-AMBIGUS : les sous-chaînes nues `"billing"`
/// et `"available balance"` de Pi sont écartées (un 429 transitoire « rate limited;
/// see billing dashboard » serait droppé à tort). Biais assumé vers le faux-négatif :
/// un terminal raté retombe en `RateLimited` → retry BORNÉ (`max_retries` + cap 60 s),
/// dégradation sûre ; un faux-positif tuerait la session sans recours.
const TERMINAL_RATE_LIMIT_MARKERS: &[&str] = &[
    "gousagelimiterror",
    "freeusagelimiterror",
    "monthly usage limit reached",
    "insufficient_quota",
    "out of budget",
    "quota exceeded",
];

/// Vrai si le corps d'un 429 dénote un quota épuisé (terminal), pas une surcharge
/// transitoire. Voir `TERMINAL_RATE_LIMIT_MARKERS`.
pub fn is_terminal_rate_limit(body: &str) -> bool {
    let lower = body.to_ascii_lowercase();
    TERMINAL_RATE_LIMIT_MARKERS
        .iter()
        .any(|m| lower.contains(m))
}

/// Parse le délai serveur d'un `Retry-After` en millisecondes (US-023), dans
/// l'ordre de priorité de Pi : `retry-after-ms` (ms exactes) > `Retry-After`
/// (secondes entières) > `Retry-After` (date HTTP IMF-fixdate, delta vs `now_ms`).
/// `now_ms` est injecté pour la testabilité. `None` si aucun en-tête exploitable.
///
/// NB : la valeur RENVOYÉE n'est PAS plafonnée ici (elle reflète le serveur brut) ;
/// le plafond `MAX_RETRY_AFTER_MS` est appliqué par le consommateur (`retry_delay`,
/// agent-core). Tout futur consommateur de `retry_after_ms` doit re-borner.
fn parse_retry_after_ms(headers: &reqwest::header::HeaderMap, now_ms: u64) -> Option<u64> {
    // 1) `retry-after-ms` : millisecondes exactes, prioritaire.
    if let Some(ms) = headers
        .get("retry-after-ms")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<f64>().ok())
        .filter(|ms| ms.is_finite())
    {
        return Some(ms.max(0.0) as u64);
    }
    // 2) `Retry-After` : secondes entières, sinon date HTTP.
    let raw = headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim();
    if let Some(secs) = raw.parse::<f64>().ok().filter(|s| s.is_finite()) {
        return Some((secs.max(0.0) * 1000.0) as u64);
    }
    // 3) date HTTP absolue → delta positif jusqu'à l'échéance.
    let target_ms = parse_imf_fixdate_ms(raw)?;
    Some(target_ms.saturating_sub(now_ms))
}

/// Parse une date HTTP IMF-fixdate (`"Tue, 15 Nov 1994 08:12:31 GMT"`, RFC 7231)
/// en ms epoch. Format unique émis par les backends OpenAI ; les variantes legacy
/// (RFC 850 / asctime) ne sont pas gérées (renvoie `None`). Évite une dépendance
/// date externe (offline-safe).
fn parse_imf_fixdate_ms(s: &str) -> Option<u64> {
    let rest = s.trim().split_once(", ")?.1; // "15 Nov 1994 08:12:31 GMT"
    let mut it = rest.split(' ');
    let day: i64 = it.next()?.parse().ok()?;
    let month: i64 = match it.next()? {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        _ => return None,
    };
    let year: i64 = it.next()?.parse().ok()?;
    let mut hms = it.next()?.split(':');
    let h: i64 = hms.next()?.parse().ok()?;
    let mi: i64 = hms.next()?.parse().ok()?;
    let se: i64 = hms.next()?.parse().ok()?;
    let secs = days_from_civil(year, month, day) * 86_400 + h * 3_600 + mi * 60 + se;
    if secs < 0 {
        return None;
    }
    Some((secs as u64) * 1000)
}

/// Jours depuis 1970-01-01 (algorithme de Howard Hinnant, domaine public). Exact
/// sur toute la plage grégorienne proleptique.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[async_trait]
impl Provider for OpenAiChatGptProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::OpenAiChatGpt
    }

    fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    async fn stream(
        &self,
        req: CanonicalRequest,
    ) -> Result<BoxStream<'static, Result<StreamEvent, ProviderError>>, ProviderError> {
        // 1. credential fraîche (refresh + keyring si besoin) → URL + en-têtes.
        let spec = self.creds.request_spec().await?;
        // 2. corps Responses (SSE stateless).
        let mut body = build_responses_body(&req, self.reasoning_effort.as_deref());
        // US-029 : clé de cache stable par session → réutilisation du cache backend.
        inject_cache_key(&mut body, &self.session_id);

        // 3. POST. `.json()` pose content-type ; on ajoute les en-têtes propriétaires
        //    (Authorization, chatgpt-account-id, originator, OpenAI-Beta, accept).
        let mut rb = self.http.post(&spec.url).json(&body);
        for (k, v) in &spec.headers {
            if !k.eq_ignore_ascii_case("content-type") {
                rb = rb.header(k, v);
            }
        }
        // US-022 (durcissement) : borne la phase HEADERS. `connect_timeout` couvre
        // l'établissement TCP/TLS et `idle_guarded` le stream OUVERT, mais entre les
        // deux `send()` attend les headers de réponse sans borne : un backend qui
        // handshake puis retient ses headers (proxy bloqué, queue) gèlerait la boucle
        // sans signal. `send()` se résout à la réception des headers → ce timeout NE
        // coupe PAS le long stream SSE qui suit.
        let resp = send_with_header_timeout(rb, CONNECT_TIMEOUT).await?;

        // 4. statut. 413 → erreur de contexte (withholding/compaction réactive).
        let status = resp.status();
        if !status.is_success() {
            let code = status.as_u16();
            if code == 413 {
                return Err(ProviderError::ContextLengthExceeded);
            }
            // US-023 : capter `Retry-After` AVANT de consommer le corps (les
            // headers sont droppés sinon). `now_ms` local (le provider fait des
            // I/O réelles, pas besoin de l'horloge injectée du cœur).
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            let retry_after_ms = parse_retry_after_ms(resp.headers(), now_ms);
            let mut text = resp.text().await.unwrap_or_default();
            text.truncate(MAX_ERR_BODY);
            return Err(ProviderError::Http {
                status: code,
                message: text,
                retry_after_ms,
            });
        }

        // 5. flux SSE → StreamEvent canoniques (jamais d'ANSI, jamais de panic).
        //    Mapping stateful (un event SSE → 0..n StreamEvent) dans un async_stream,
        //    puis watchdog `idle_guarded` : le timeout enveloppe `inner.next()`, donc
        //    un `es.next()` qui stalle (backend muet) déclenche l'idle timeout, sans
        //    couper pendant le drain d'events déjà bufferisés (US-022).
        let mut es = resp.bytes_stream().eventsource();
        let replay = self.reasoning_replay; // Copy → capturé dans le stream 'static.
        let mapped = async_stream::stream! {
            let mut mapper = CodexEventMapper::with_replay(replay);
            while let Some(ev) = es.next().await {
                match ev {
                    Ok(event) => match mapper.ingest(&event.data) {
                        Ok(events) => {
                            for e in events {
                                yield Ok(e);
                            }
                        }
                        Err(e) => {
                            yield Err(e);
                            return;
                        }
                    },
                    Err(e) => {
                        yield Err(ProviderError::Stream(e.to_string()));
                        return;
                    }
                }
            }
        };
        Ok(idle_guarded(mapped.boxed(), self.idle_timeout).boxed())
    }

    async fn complete(&self, req: CanonicalRequest) -> Result<CanonicalResponse, ProviderError> {
        // Réutilise le chemin stream et agrège (titres / résumés de compaction).
        let stream = self.stream(req).await?;
        futures_util::pin_mut!(stream);
        let mut text = String::new();
        let mut usage = TokenUsage::default();
        let mut stop = StopReason::EndTurn;
        while let Some(ev) = stream.next().await {
            match ev? {
                StreamEvent::TextDelta { text: t } => text.push_str(&t),
                StreamEvent::Usage { usage: u } => usage = u,
                StreamEvent::Done { stop: s } => stop = s,
                _ => {}
            }
        }
        Ok(CanonicalResponse {
            content: vec![ContentBlock::Text { text }],
            usage,
            stop,
        })
    }

    fn classify_error(&self, err: &ProviderError) -> ErrorClass {
        match err {
            ProviderError::Http {
                status, message, ..
            } => match *status {
                401 | 403 => ErrorClass::Auth(AuthError::Invalid),
                // 429 quota épuisé (corps GoUsageLimitError/billing/…) → TERMINAL :
                // jamais retryé (US-023). Un 429 transitoire reste `RateLimited`.
                429 if is_terminal_rate_limit(message) => ErrorClass::InvalidRequest,
                429 => ErrorClass::RateLimited,
                529 => ErrorClass::Overloaded(529),
                s if s >= 500 => ErrorClass::Retryable,
                _ => ErrorClass::InvalidRequest,
            },
            // Transitoires : transport, flux coupé, chunk garbled → retry transverse.
            ProviderError::Transport(_) | ProviderError::Stream(_) | ProviderError::Decode(_) => {
                ErrorClass::Retryable
            }
            // N'atteint pas classify (is_context_error géré en amont) ; fail-safe.
            ProviderError::ContextLengthExceeded => ErrorClass::InvalidRequest,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider() -> OpenAiChatGptProvider {
        OpenAiChatGptProvider::new(
            OAuthCredential {
                provider: agent_auth::ProviderId::OpenAiChatGpt,
                access: agent_auth::Secret::new("AT"),
                refresh: agent_auth::Secret::new("RT"),
                expires_at: u64::MAX,
                account_id: Some("acct".into()),
            },
            DEFAULT_MAX_CONTEXT,
            None,
        )
    }

    #[test]
    fn capabilities_are_sse_stateless() {
        let p = provider();
        let c = p.capabilities();
        assert!(!c.server_side_state, "SSE stateless → mappe le canonique");
        assert!(c.tools && c.reasoning);
        assert_eq!(p.kind(), ProviderKind::OpenAiChatGpt);
    }

    // US-029 : session_id = UUID v4 bien formé, stable par instance, unique.
    #[test]
    fn session_id_is_uuid_v4_shaped() {
        let id = new_session_id();
        assert_eq!(id.len(), 36, "UUID canonique 8-4-4-4-12");
        let lens: Vec<usize> = id.split('-').map(str::len).collect();
        assert_eq!(lens, vec![8, 4, 4, 4, 12]);
        assert_eq!(id.as_bytes()[14], b'4', "nibble de version 4");
        assert!(
            matches!(id.as_bytes()[19], b'8' | b'9' | b'a' | b'b'),
            "variant RFC 4122"
        );
        assert_ne!(new_session_id(), new_session_id(), "deux UUID diffèrent");
        // un provider porte un session_id UUID stocké à la construction.
        assert_eq!(provider().session_id.len(), 36);
    }

    #[test]
    fn classify_error_taxonomy() {
        let p = provider();
        let http = |s| ProviderError::Http {
            status: s,
            message: String::new(),
            retry_after_ms: None,
        };
        assert!(matches!(
            p.classify_error(&http(401)),
            ErrorClass::Auth(AuthError::Invalid)
        ));
        assert!(matches!(
            p.classify_error(&http(429)),
            ErrorClass::RateLimited
        ));
        assert!(matches!(
            p.classify_error(&http(529)),
            ErrorClass::Overloaded(529)
        ));
        assert!(matches!(
            p.classify_error(&http(503)),
            ErrorClass::Retryable
        ));
        assert!(matches!(
            p.classify_error(&http(400)),
            ErrorClass::InvalidRequest
        ));
        assert!(matches!(
            p.classify_error(&ProviderError::Transport("x".into())),
            ErrorClass::Retryable
        ));
    }

    // US-023 : un 429 « quota épuisé » (corps GoUsageLimitError/billing) est
    // TERMINAL (InvalidRequest, jamais retryé) ; un 429 transitoire reste
    // RateLimited (retryé).
    #[test]
    fn terminal_429_is_not_retried() {
        let p = provider();
        let terminal = |body: &str| ProviderError::Http {
            status: 429,
            message: body.to_string(),
            retry_after_ms: None,
        };
        for body in [
            "{\"error\":{\"type\":\"GoUsageLimitError\"}}",
            "FreeUsageLimitError: monthly usage limit reached",
            "{\"detail\":\"insufficient_quota\"}",
            "billing: out of budget",
        ] {
            assert!(
                matches!(p.classify_error(&terminal(body)), ErrorClass::InvalidRequest),
                "429 terminal attendu pour: {body}"
            );
        }
        // surcharge transitoire : pas de marqueur → retryable.
        assert!(matches!(
            p.classify_error(&terminal("Too Many Requests, slow down")),
            ErrorClass::RateLimited
        ));
        // régression : un 429 transitoire mentionnant « billing » NE doit PAS être
        // classé terminal (sous-chaîne nue écartée — biais faux-négatif sûr).
        assert!(matches!(
            p.classify_error(&terminal("rate limited; see your billing dashboard for limits")),
            ErrorClass::RateLimited
        ));
    }

    #[test]
    fn terminal_rate_limit_markers_are_case_insensitive() {
        assert!(is_terminal_rate_limit("GOUSAGELIMITERROR"));
        assert!(is_terminal_rate_limit("Quota Exceeded"));
        assert!(!is_terminal_rate_limit("transient overload, retry"));
    }

    // US-023 : parsing `Retry-After` dans les 3 formats (ms exactes, secondes
    // entières, date HTTP), `retry-after-ms` prioritaire.
    #[test]
    fn parse_retry_after_all_formats() {
        use reqwest::header::{HeaderMap, HeaderValue, RETRY_AFTER};

        // ms exactes prioritaires (même si `Retry-After` secondes est présent).
        let mut h = HeaderMap::new();
        h.insert("retry-after-ms", HeaderValue::from_static("1500"));
        h.insert(RETRY_AFTER, HeaderValue::from_static("9"));
        assert_eq!(parse_retry_after_ms(&h, 0), Some(1500));

        // secondes entières → ms.
        let mut h = HeaderMap::new();
        h.insert(RETRY_AFTER, HeaderValue::from_static("2"));
        assert_eq!(parse_retry_after_ms(&h, 0), Some(2000));

        // date HTTP absolue → delta vs now. 30 s epoch, now 20 s → 10 s restantes.
        let mut h = HeaderMap::new();
        h.insert(
            RETRY_AFTER,
            HeaderValue::from_static("Thu, 01 Jan 1970 00:00:30 GMT"),
        );
        assert_eq!(parse_retry_after_ms(&h, 20_000), Some(10_000));

        // échéance déjà passée → 0 (saturating), jamais négatif.
        assert_eq!(parse_retry_after_ms(&h, 40_000), Some(0));

        // aucun en-tête → None.
        assert_eq!(parse_retry_after_ms(&HeaderMap::new(), 0), None);
    }

    #[test]
    fn imf_fixdate_epoch_anchor() {
        // ancre : 1970-01-01T00:00:00Z = 0 ms.
        assert_eq!(
            parse_imf_fixdate_ms("Thu, 01 Jan 1970 00:00:00 GMT"),
            Some(0)
        );
        // un jour plus tard = 86_400_000 ms.
        assert_eq!(
            parse_imf_fixdate_ms("Fri, 02 Jan 1970 00:00:00 GMT"),
            Some(86_400_000)
        );
        // format invalide → None (pas de panic).
        assert_eq!(parse_imf_fixdate_ms("pas une date"), None);
        assert_eq!(days_from_civil(1970, 1, 1), 0);
    }

    // US-022 : un stream OUVERT mais silencieux déclenche l'idle timeout (Retryable),
    // sans gel. Timeout court (réel) → test rapide et déterministe.
    #[tokio::test]
    async fn idle_timeout_fires_on_silent_stream() {
        let silent =
            futures_util::stream::pending::<Result<StreamEvent, ProviderError>>().boxed();
        let guarded = idle_guarded(silent, Duration::from_millis(40));
        futures_util::pin_mut!(guarded);
        let first = guarded.next().await;
        assert!(
            matches!(&first, Some(Err(ProviderError::Stream(m))) if m == "idle timeout"),
            "idle timeout attendu, reçu: {first:?}"
        );
    }

    // US-022 (durcissement) : un backend qui ACCEPTE la connexion puis RETIENT ses
    // headers (proxy bloqué, queue) doit déclencher le header timeout, pas geler la
    // boucle. Serveur local qui accepte puis dort sans répondre ; timeout court (réel).
    #[tokio::test]
    async fn header_timeout_fires_when_backend_withholds_response() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind localhost");
        let addr = listener.local_addr().expect("local addr");
        // accepte la socket et la garde ouverte SANS jamais écrire de réponse.
        tokio::spawn(async move {
            if let Ok((sock, _)) = listener.accept().await {
                tokio::time::sleep(Duration::from_secs(30)).await;
                drop(sock);
            }
        });
        let rb = reqwest::Client::new().post(format!("http://{addr}/"));
        let res = send_with_header_timeout(rb, Duration::from_millis(150)).await;
        assert!(
            matches!(&res, Err(ProviderError::Stream(m)) if m == "header timeout"),
            "header timeout attendu, reçu: {res:?}"
        );
    }

    // US-022 : un flux qui émet des events sous le délai les relaie intacts, et la
    // fin de flux (None) termine proprement.
    #[tokio::test]
    async fn idle_guard_passes_events_through() {
        let inner = futures_util::stream::iter(vec![
            Ok(StreamEvent::TextDelta { text: "a".into() }),
            Ok(StreamEvent::Done {
                stop: StopReason::EndTurn,
            }),
        ])
        .boxed();
        let guarded = idle_guarded(inner, Duration::from_secs(5));
        let collected: Vec<_> = guarded.collect().await;
        assert_eq!(collected.len(), 2);
        assert!(matches!(collected[0], Ok(StreamEvent::TextDelta { .. })));
        assert!(matches!(
            collected[1],
            Ok(StreamEvent::Done {
                stop: StopReason::EndTurn
            })
        ));
    }

    // US-022 : une erreur amont est relayée puis termine le flux (parité avec le
    // chemin direct : `yield Err` puis `return`).
    #[tokio::test]
    async fn idle_guard_propagates_and_stops_on_error() {
        let inner = futures_util::stream::iter(vec![
            Ok(StreamEvent::TextDelta { text: "x".into() }),
            Err(ProviderError::Stream("boom".into())),
            Ok(StreamEvent::TextDelta { text: "jamais".into() }),
        ])
        .boxed();
        let guarded = idle_guarded(inner, Duration::from_secs(5));
        let collected: Vec<_> = guarded.collect().await;
        assert_eq!(collected.len(), 2, "doit s'arrêter après l'erreur");
        assert!(matches!(collected[1], Err(ProviderError::Stream(_))));
    }
}
