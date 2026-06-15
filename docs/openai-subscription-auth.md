# Auth abonnement ChatGPT — extraction Pi → implémentation Numen

> **Statut : référence d'implémentation + input de décision (pas une des 4 sources de vérité).** Ne contredit pas `ARCHITECTURE/PROVIDERS/DECISIONS/ROADMAP` ; il les complète sur un point que la préférence d'Arthur (« utiliser mon abonnement OpenAI plutôt que l'API ») impose.
>
> Source extraite : repo `pi` (TypeScript, `/home/arthur/dev/pi`), packages `ai` + `coding-agent`. **Constantes vérifiées adversarialement : 45/45 claims confirmés contre le code réel** (3 corrections de précision intégrées ci-dessous, marquées ⚠️). Toutes les valeurs sont littérales (recopiables verbatim en Rust). Croisé avec `docs/PROVIDERS.md` et `tasks/prd-numen.md` (US-015/016/017/018, EP-004).
>
> **Décision induite à acter en ADR-10** : l'auth abonnement ChatGPT force la **Responses API sur le backend ChatGPT** (pas Chat Completions) → un `ProviderKind::OpenAiChatGpt` distinct, gated, SSE stateless. Voir §2.

---

## 1. Comment Pi authentifie via l'abonnement ChatGPT (mécanique réelle)

Pi se fait passer pour le **Codex CLI officiel d'OpenAI** : il réutilise verbatim le `client_id` du paquet OSS `openai/codex` (`openai-codex.ts:31`). Ce n'est pas un client OAuth propre à Pi — c'est le point pivot de toute la mécanique, et la source du risque ToS (§4).

### 1.a — Obtention du token

**Constantes (`packages/ai/src/utils/oauth/openai-codex.ts:31-44`)**

| Constante | Valeur littérale |
|---|---|
| `CLIENT_ID` | `app_EMoamEEZ73f0CkXaXp7hrann` |
| `AUTH_BASE_URL` | `https://auth.openai.com` |
| `AUTHORIZE_URL` | `https://auth.openai.com/oauth/authorize` |
| `TOKEN_URL` | `https://auth.openai.com/oauth/token` |
| `REDIRECT_URI` (browser) | `http://localhost:1455/auth/callback` |
| `DEVICE_USER_CODE_URL` | `https://auth.openai.com/api/accounts/deviceauth/usercode` |
| `DEVICE_TOKEN_URL` | `https://auth.openai.com/api/accounts/deviceauth/token` |
| `DEVICE_VERIFICATION_URI` (affichée user) | `https://auth.openai.com/codex/device` |
| `DEVICE_REDIRECT_URI` (échange device) | `https://auth.openai.com/deviceauth/callback` |
| `DEVICE_CODE_TIMEOUT_SECONDS` | `900` (15 min) |
| `SCOPE` | `openid profile email offline_access` |
| `JWT_CLAIM_PATH` | `https://api.openai.com/auth` |
| Port callback local | `1455`, host `PI_OAUTH_CALLBACK_HOST || 127.0.0.1` (`:50`, `:374`) |
| PKCE method | `S256` (`:309`) |

**PKCE** (`pkce.ts:21-33`) : `verifier = base64url(32 octets aléatoires)`, **sans padding** (`=` supprimés, `+`→`-`, `/`→`_`). ⚠️ **Correction load-bearing** : le `challenge` est `base64url(SHA-256(UTF-8 bytes de la STRING verifier))`, **PAS** le hash des 32 octets aléatoires bruts. En Rust : `Sha256::digest(verifier.as_bytes())` où `verifier` est déjà la string base64url — surtout pas `Sha256::digest(&random_32_bytes)`. Les deux produisent des challenges différents → un misread casse silencieusement le flow. `state = randomBytes(16).hex()` (32 hex chars, `:75`).

**FLOW A — Browser (PKCE + serveur callback local)**
1. `generatePKCE()` + `createState()` localement.
2. Ouvre dans le navigateur (pas un fetch) `GET https://auth.openai.com/oauth/authorize` avec les query params (`:303-315`) :
   ```
   response_type=code
   client_id=app_EMoamEEZ73f0CkXaXp7hrann
   redirect_uri=http://localhost:1455/auth/callback
   scope=openid profile email offline_access
   code_challenge=<base64url_sha256_de_la_string_verifier>
   code_challenge_method=S256
   state=<32_hex>
   id_token_add_organizations=true      # ← non-standard, à inclure verbatim
   codex_cli_simplified_flow=true       # ← non-standard, à inclure verbatim
   originator=pi                        # ← Numen mettra "numen" (cf. §1.c + §4)
   ```
3. Serveur HTTP local sur `127.0.0.1:1455` (`:324,:374`). OpenAI redirige vers `…/auth/callback?code=…&state=…`. Le serveur valide `path==/auth/callback`, le `state`, extrait `code` (`:341-364`).
4. Fallback si callback manqué : paste manuel d'URL/code (`onManualCodeInput`) ou `onPrompt` (`:478-544`).
5. Échange `code→token` (`:160-173`) :
   ```
   POST https://auth.openai.com/oauth/token
   Content-Type: application/x-www-form-urlencoded
   grant_type=authorization_code&client_id=app_EMoamEEZ73f0CkXaXp7hrann
     &code=<code>&code_verifier=<verifier>
     &redirect_uri=http://localhost:1455/auth/callback
   ```
   **Pas de client_secret, pas de header Authorization** — flow public PKCE pur.

**FLOW B — Device-code (headless)**
1. `POST .../deviceauth/usercode`, body **JSON** `{"client_id":"app_EMoamEEZ73f0CkXaXp7hrann"}` → réponse `{device_auth_id, user_code, interval}` (`:196-236`).
2. Affiche à l'user : `user_code` + `https://auth.openai.com/codex/device` (`:438-443`).
3. Poll loop toutes les `interval` s (min garanti 1000 ms, max 900 s) : `POST .../deviceauth/token`, body JSON `{"device_auth_id","user_code"}` (`:244-253`).
   - `403`/`404` **ou** `errorCode=deviceauth_authorization_pending` → pending ;
   - `errorCode=slow_down` → +5 s à l'intervalle ;
   - `200` → `{authorization_code, code_verifier}` (`:255-293`).
   **Subtilité majeure** : en device flow le `code_verifier` n'est **pas** généré localement — il est **renvoyé par le serveur** dans la réponse du poll (`:257-266`).
4. Échange final identique à A.5 **mais avec `redirect_uri=https://auth.openai.com/deviceauth/callback`** (`:444-450`). L'échange échoue si la `redirect_uri` ne matche pas celle de l'autorisation.

### 1.b — Décodage de l'id_token / dérivation de l'account_id

La réponse token est `{access_token, refresh_token, expires_in}` (`:138-151`). **Pas d'`id_token` distinct** dans le code, bien que `openid` soit demandé. L'`account_id` est extrait du **JWT access_token lui-même** :
- split sur `.`, base64url-decode de la payload, lecture de `payload['https://api.openai.com/auth'].chatgpt_account_id` (`openai-codex.ts:400-404`, et côté provider `openai-codex-responses.ts:1416-1427`).
- C'est un **claim custom** dans le namespace `https://api.openai.com/auth`, pas un claim JWT standard. Échec de lecture → exception immédiate.
- `OAuthCredentials = {access, refresh, expires: Date.now()+expires_in*1000, accountId}` (`:407-419`).

### 1.c — Usage du token pour appeler le modèle

**C'est ici que ça devient critique pour Numen.** L'inférence ne tape **pas** `api.openai.com/v1/chat/completions`, ni même `api.openai.com/v1/responses`. Elle tape le **backend ChatGPT** via la **Responses API** (`openai-codex-responses.ts`) :

| Constante | Valeur (`openai-codex-responses.ts`) |
|---|---|
| `DEFAULT_CODEX_BASE_URL` | `https://chatgpt.com/backend-api` (`:51`) |
| SSE endpoint résolu | `https://chatgpt.com/backend-api/codex/responses` (`:522-528`) |
| WebSocket endpoint résolu | `wss://chatgpt.com/backend-api/codex/responses` (`:530-535`) |
| `OPENAI_BETA` (WS) | `responses_websockets=2026-02-06` (`:689`) |
| `OPENAI_BETA` (SSE) | `responses=experimental` (`:1462`) |
| `originator` header | `pi` (`:1448`) |

**Surface d'API : Responses API, pas Chat Completions.** Provider api `openai-codex-responses`, provider id `openai-codex`. Transport `auto` = WebSocket en premier, repli SSE si WS échoue **avant** de streamer (si WS échoue **après** début de stream, l'erreur est propagée, pas de repli).

**Requête SSE (fallback / forcé)** — `POST https://chatgpt.com/backend-api/codex/responses` :

Headers :
```
Authorization: Bearer <access_token JWT>
chatgpt-account-id: <chatgpt_account_id extrait du JWT>   # ← propriétaire ChatGPT, requis pour router
originator: pi                                            # ← propriétaire, sur TOUTES les requêtes
User-Agent: pi (<os.platform> <os.release>; <os.arch>)
OpenAI-Beta: responses=experimental
accept: text/event-stream
content-type: application/json
session-id: <sessionId>            (si présent)
x-client-request-id: <sessionId>  (si présent)
+ model.headers / options.headers
```

⚠️ **Correction load-bearing** : `originator` est **hardcodé** à `"pi"` côté provider (`:1448`), ce n'est PAS un knob runtime. L'adapter Numen hardcodera sa propre valeur (`"numen"`). **Risque ouvert** : le backend ChatGPT *peut* valider l'`originator` contre une liste connue (Codex utilise `codex_cli_rs`) — si un `originator` inconnu est rejeté, il faudra soit demander l'inscription, soit (zone grise totale) emprunter une valeur connue. À tester en live.

Body (forme) :
```json
{
  "model": "gpt-5.4",
  "store": false,                              // ← le backend REJETTE store:true ("Store must be set to false", :1318)
  "stream": true,
  "instructions": "<systemPrompt>",            // ← PAS un message role:system
  "input": [ /* messages au format Responses API */ ],  // ← PAS messages[]
  "text": { "verbosity": "low" },
  "include": ["reasoning.encrypted_content"],  // ← raisonnement chiffré, non documenté en public
  "prompt_cache_key": "<sessionId clamped>",
  "tool_choice": "auto",
  "parallel_tool_calls": true,
  "tools": [ ... ],
  "reasoning": { "effort": "xhigh", "summary": "auto" }
}
```

**Requête WebSocket** (transport par défaut) — upgrade `wss://chatgpt.com/backend-api/codex/responses` (`:1341` send, `:1474-1490` headers). Mêmes headers **sauf** : `OpenAI-Beta: responses_websockets=2026-02-06`, et **`accept` + `content-type` supprimés** (`:1482-1486`). Frame envoyée : `{"type":"response.create", ...même body...}`. Continuation sur connexion réutilisée : body réduit au delta d'input + `previous_response_id` (state **server-side, connection-scoped**), `store` reste `false` (`:1261-1278`).

⚠️ **Correction de précision** — event mapping (`mapCodexEvents`, `:604`) : `response.done` **ET** `response.completed` **ET** `response.incomplete` sont tous traités comme terminaux et normalisés vers `response.completed`. C'est `response.incomplete` qui est le seul réellement spécifique au backend Codex (absent de la Responses API publique), pas `response.done`. `type='error'` / `response.failed` arrive comme event de stream → levé en `CodexApiError` (pas une erreur HTTP).

### 1.d — Storage + refresh

**Storage** (`packages/coding-agent/src/core/auth-storage.ts`) :
- Fichier `~/.pi/agent/auth.json`, **JSON en clair**, mode `0o600`, dossier parent `0o700` (`:48,:65`). **Pas de keyring OS, pas de chiffrement** — sécurité = permissions FS uniquement.
- Map `providerId → credential`. Entrée OAuth : `{type:'oauth', access, refresh, expires}` où `expires` = timestamp ms absolu. **Mono-compte par provider**.
- Locking via `proper-lockfile` (lockfile physique). Refresh sous lock async (backoff 100 ms→10 s, stale 30 s) ; double-check `Date.now() < expires` sous lock pour éviter un refresh concurrent (`:409-453`).

**Refresh** (`openai-codex.ts:177-187`) :
```
POST https://auth.openai.com/oauth/token
Content-Type: application/x-www-form-urlencoded
grant_type=refresh_token&refresh_token=<refresh>&client_id=app_EMoamEEZ73f0CkXaXp7hrann
```
Retourne un **nouveau** `{access_token, refresh_token, expires_in}` → **refresh tokens rotatifs (sliding)**, il faut réécrire le refresh à chaque cycle. Seuil de refresh côté Pi : `Date.now() >= cred.expires` (bord exact, **pas de marge** pour OpenAI — contrairement à Anthropic qui anticipe de 5 min, `anthropic.ts:223`). Refresh échoué → credential **préservé** (pas supprimé), retry via `/login`.

---

## 2. Conflit critique : Chat Completions vs Responses API

**Tranché, sans ambiguïté : oui, l'auth abonnement ChatGPT oblige la Responses API sur le backend ChatGPT.** Imposé par le backend, pas un choix de Pi :
- L'endpoint est `chatgpt.com/backend-api/codex/responses` — il n'expose **pas** `/chat/completions`.
- Le body exige `store:false`, `input[]` (format Responses), `instructions` (pas `role:system`), `include:["reasoning.encrypted_content"]`.
- Headers propriétaires obligatoires (`chatgpt-account-id`, `originator`) absents de toute API publique OpenAI.

**Implication pour le canonique Numen.** Le canonique est **Anthropic-like, transcript client-side reconstruit à chaque tour** (`PROVIDERS.md §1.1`), indispensable à la compaction / resume JSONL / replay (`US-009`). Or :
- En **SSE**, le backend Codex est **stateless** (pas de `previous_response_id`) → contexte complet dans `input[]` à chaque tour → **mappe proprement** sur le transcript client-side. Bonne nouvelle.
- En **WebSocket** avec réutilisation de connexion, bascule sur `previous_response_id` (state server-side connection-scoped) = exactement le piège **`PROVIDERS.md §4.1`** (« OpenAI Responses API ne mappe pas sur le canonique »).

**Conclusion : conflit réel mais contournable.** `ProviderKind::OpenAiChat` (cible MVP) ne peut **pas** servir l'abonnement ChatGPT — endpoint et wire format diffèrent.

**Recommandation : adapter dédié, gated, séparé du Chat Completions BYOK.**

Introduire `ProviderKind::OpenAiChatGpt` (subscription), **et non** réutiliser `OpenAiResponses` générique ni `OpenAiChat` :
1. `OpenAiChat` (US-017) reste pur : `api.openai.com/v1/chat/completions`, API key BYOK, mapping client-side, **cible MVP intacte**.
2. `OpenAiResponses` (déjà prévu gated) cible `api.openai.com/v1/responses` au token (API publique). Le backend ChatGPT en diverge (base URL, headers, `store:false` forcé). Mélanger = branches conditionnelles fragiles.
3. Le nouvel adapter `openai-chatgpt` **force le mode SSE stateless** (seul qui mappe le canonique client-side) et **renonce au WebSocket+`previous_response_id`** pour le MVP. `capabilities().server_side_state = false`.

**Lien US-017** : son AC dit « la Responses API est hors scope ». L'abonnement ChatGPT *est* de la Responses API → **US-017 ne le couvre pas** (il couvre OpenAI au token, Chat Completions). L'auth abonnement ChatGPT est une **US séparée gated**, pas du P0.

---

## 3. Design pour Numen : `agent-auth` + `agent-provider`

### 3.a — Type de credential (enum)

```rust
// agent-auth
pub enum Credential {
    /// BYOK : clé brute. Couvre OpenAI Chat (US-017), Gemini, OpenRouter.
    ApiKey { provider: ProviderId, key: SecretString },
    /// OAuth subscription (sliding refresh). Couvre Anthropic OAuth + ChatGPT.
    OAuth(OAuthCredential),
}

pub struct OAuthCredential {
    pub provider: ProviderId,
    pub access: SecretString,
    pub refresh: SecretString,
    pub expires_at: u64,            // timestamp ms absolu (= Pi `expires`)
    pub account_id: Option<String>, // chatgpt_account_id pour ChatGPT ; None pour Anthropic
}
```

### 3.b — Flow de login (pseudo-Rust)

```rust
// agent-auth::oauth::openai_chatgpt
const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const AUTHORIZE_URL: &str = "https://auth.openai.com/oauth/authorize";
const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const REDIRECT_URI: &str = "http://localhost:1455/auth/callback";
const SCOPE: &str = "openid profile email offline_access";
const CALLBACK_PORT: u16 = 1455;

struct Pkce { verifier: String, challenge: String }

fn generate_pkce() -> Pkce {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);                          // rand 0.9
    let verifier = b64url_nopad(&bytes);                        // base64url SANS padding
    // ⚠️ hash la STRING verifier (ses octets UTF-8), pas les 32 octets bruts :
    let challenge = b64url_nopad(&Sha256::digest(verifier.as_bytes())); // sha2
    Pkce { verifier, challenge }
}

async fn login_browser() -> Result<OAuthCredential> {
    let pkce = generate_pkce();
    let state = hex::encode(rand_16_bytes());

    // Serveur callback local (hyper nu) sur 127.0.0.1:1455 ; oneshot::channel résout sur /auth/callback.
    let (tx, rx) = oneshot::channel();
    let server = spawn_callback_server(CALLBACK_PORT, state.clone(), tx);

    let url = build_authorize_url(&pkce.challenge, &state); // + id_token_add_organizations=true,
                                                            //   codex_cli_simplified_flow=true, originator=numen
    open::that(&url)?;  // crate `open`

    // Race : callback server vs paste manuel (réplique login-dialog)
    let CallbackResult { code, .. } = tokio::select! {
        r = rx => r?,
        c = prompt_manual_paste() => parse_authorization_input(c)?,
    };
    server.shutdown();

    let tokens = exchange_code(&code, &pkce.verifier, REDIRECT_URI).await?;
    let account_id = extract_account_id(&tokens.access_token)?;  // décode JWT, claim custom
    Ok(OAuthCredential { /* expires_at: now_ms() + expires_in*1000 */ })
}

async fn exchange_code(code: &str, verifier: &str, redirect_uri: &str) -> Result<TokenResp> {
    // reqwest, form-urlencoded, AUCUN client_secret, AUCUN header Authorization
    client.post(TOKEN_URL)
        .form(&[
            ("grant_type", "authorization_code"),
            ("client_id", CLIENT_ID),
            ("code", code),
            ("code_verifier", verifier),
            ("redirect_uri", redirect_uri),
        ])
        .send().await?.json().await
}
```

**Device flow** (headless, optionnel) : `POST .../deviceauth/usercode` JSON `{client_id}` → afficher `user_code` + `https://auth.openai.com/codex/device` → poll `.../deviceauth/token` (RFC 8628 : `pending` sur 403/404/`deviceauth_authorization_pending`, `slow_down` → +5 s, timeout 900 s). **Attention** : le `code_verifier` vient de la réponse du poll, pas de génération locale ; l'échange final utilise `redirect_uri=https://auth.openai.com/deviceauth/callback`.

**Décodage account_id** :
```rust
fn extract_account_id(access_token: &str) -> Result<String> {
    // décodage manuel de la payload (on NE vérifie PAS la signature — on lit juste un claim ;
    //  la confiance vient du canal TLS d'OpenAI, pas d'une validation crypto locale).
    let claims = decode_jwt_payload_unverified(access_token)?;
    claims["https://api.openai.com/auth"]["chatgpt_account_id"]
        .as_str().map(String::from)
        .ok_or(AuthError::MissingAccountId)
}
```

### 3.c — Stockage keyring (US-018)

Pi stocke en **JSON clair 0o600**. **Numen DOIT faire mieux** : `US-018` impose le secret store OS (keyring), jamais en clair (`prd-numen.md:352`, NFR Security `:409`).

```rust
let entry = keyring::Entry::new("numen", &format!("oauth:{provider}"))?;
entry.set_password(&serde_json::to_string(&oauth_cred)?)?;  // blob JSON dans le keyring, PAS sur disque
```
Le keyring stocke le **blob entier** (access+refresh+expires+account_id). Indisponibilité → erreur explicite + fallback documenté (`:354`). **Ne pas** répliquer l'`auth.json` clair de Pi.

### 3.d — Refresh

```rust
async fn ensure_fresh(cred: &mut OAuthCredential) -> Result<()> {
    if now_ms() < cred.expires_at { return Ok(()); }       // seuil = bord exact (comme Pi OpenAI)
    // (recommandé : marge 60s pour éviter une course expiry/requête)
    let new = refresh_token(&cred.refresh).await?;          // POST TOKEN_URL grant_type=refresh_token
    *cred = OAuthCredential { /* SLIDING : réécrire access ET refresh */ };
    keyring_store(cred)?;
    Ok(())
}
```
Refresh tokens **rotatifs** : réécrire le refresh à chaque cycle. Mappe sur `ErrorClass::Auth(AuthError::Expired)` (`PROVIDERS §2.2`) → refresh puis 1 retry. Refresh échoué : ne pas effacer la credential.

### 3.e — Sélection base URL + headers selon le type de credential

| | `Credential::ApiKey` (US-017, MVP) | `Credential::OAuth` ChatGPT (gated) |
|---|---|---|
| Base URL | `https://api.openai.com/v1` | `https://chatgpt.com/backend-api/codex` |
| Endpoint | `/chat/completions` | `/responses` (SSE) |
| Auth header | `Authorization: Bearer <key>` | `Authorization: Bearer <access_token>` |
| Headers spéciaux | — | `chatgpt-account-id: <account_id>`, `originator: numen`, `OpenAI-Beta: responses=experimental` |
| Wire format | `messages[]`, `store` libre | `input[]`, `instructions`, `store:false` forcé, `include:["reasoning.encrypted_content"]` |
| Mapping canonique | propre (client-side) | propre **en SSE stateless** ; éviter WS/`previous_response_id` |

```rust
fn resolve_endpoint(cred: &Credential) -> Endpoint {
    match cred {
        Credential::ApiKey { .. } => Endpoint {
            base: "https://api.openai.com/v1", path: "/chat/completions",
            wire: Wire::ChatCompletions,
        },
        Credential::OAuth(o) => Endpoint {
            base: "https://chatgpt.com/backend-api/codex", path: "/responses",
            wire: Wire::CodexResponses { account_id: o.account_id.clone() },
        },
    }
}
```
Conséquence : **deux adapters / deux `ProviderKind`** (`OpenAiChat` vs `OpenAiChatGpt`), pas un seul avec un `if`. Branchement par credential local à l'adapter (`PROVIDERS.md §1.1`).

### 3.f — Crates Rust

| Besoin | Crate | Note |
|---|---|---|
| HTTP / token exchange / SSE | `reqwest` + `eventsource-stream` | déjà actées |
| Serveur callback local | `hyper` nu (ou `axum`) | un seul handler `GET /auth/callback` ; hyper évite une dép lourde |
| PKCE | `sha2` + `rand` + base64url maison | base64url **sans padding** |
| Décodage JWT (account_id) | split+`base64`+`serde_json` manuel | on lit un claim, on ne vérifie **pas** la signature → évite `jsonwebtoken` |
| Keyring | `keyring` | déjà dans PRD |
| Ouvrir le navigateur | `open` | équivalent `openBrowser` de Pi |
| Secret en mémoire | `secrecy` (`SecretString`) | évite les fuites de token en logs/`Debug` |

**Sur `oauth2` (la crate)** : **s'en passer.** Le flow est trivial (PKCE + un POST form), Pi lui-même n'utilise aucune lib OAuth (`fetch` nus). `oauth2` colle mal aux particularités (claim custom, `code_verifier` venu du serveur en device flow, `redirect_uri` divergente browser/device, params non-standard). ~150 lignes à la main = plus simple.

**Mapping Pi → Numen** :

| Pi | Numen |
|---|---|
| `oauth/openai-codex.ts` | `agent-auth/src/oauth/openai_chatgpt.rs` |
| `oauth/pkce.ts` | `agent-auth/src/oauth/pkce.rs` |
| `oauth/device-code.ts` | `agent-auth/src/oauth/device.rs` (RFC 8628) |
| `auth-storage.ts` (JSON 0o600) | `agent-auth/src/store.rs` (**keyring**, pas JSON clair) |
| `providers/openai-codex-responses.ts` | adapter `agent-provider/src/openai_chatgpt.rs` (`OpenAiChatGpt`) |
| `login-dialog.ts` / `oauth-selector.ts` | `agent-tui` dialog login (US-019) |

---

## 4. Risques & verdict

**ToS-grey (réutilisation du `client_id` Codex).** `app_EMoamEEZ73f0CkXaXp7hrann` est le client OAuth du **Codex CLI officiel**. « Sign in with ChatGPT » est gaté à Codex + IDE partenaires, **aucun programme tiers ouvert** (issue `openai/codex#10974` fermée « not planned »). Réutiliser le client = techniquement possible (client OSS), **zone grise ToS, usage perso, révocable unilatéralement**.

**Précédent Anthropic = preuve que le risque est réel.** Anthropic a coupé les abonnements Pro/Max aux outils tiers (janv→4 avril 2026, `prd-numen.md:13`). OpenAI peut faire **pareil** sur le `client_id` Codex du jour au lendemain. C'est le RISQUE N°1 PRODUIT (`PROVIDERS.md §6`) — toute la thèse de Numen est d'être **model-agnostic pour ne pas en dépendre**.

**Refresh / expiry.** Tokens rotatifs sliding : robuste tant que le `client_id` vit. Révocation du client → tous les refresh échouent en bloc → bascule forcée. Prévoir un équivalent `Auth(ThirdPartyBlocked)` côté OpenAI (à sonder en live, comme le leg Anthropic de US-001).

**`chatgpt-account-id` obligatoire.** Sans ce header (dérivé du JWT), le backend ne route pas. Si OpenAI change le namespace `https://api.openai.com/auth`, ça casse silencieusement.

### Verdict

**Oui, l'implémenter pour le dogfood d'Arthur — mais strictement comme credential optionnelle étiquetée « fragile », derrière BYOK, jamais en P0.**

1. **Arthur a rejeté Ollama comme défaut perso et veut son abonnement OpenAI.** Le verdict spike acte Ollama comme premier dogfood « officiel » (gratuit/local), mais l'abonnement ChatGPT couvre le confort quotidien réel qu'Arthur veut.
2. **Le MVP ne doit pas en dépendre** (`FR-11`, verdict US-001). P0 = OpenAI Chat **au token** (US-017) + le provider non-bloqué. L'abonnement ChatGPT = **US séparée gated**, hors chemin critique.
3. **Forme concrète** : `ProviderKind::OpenAiChatGpt`, derrière un flag/credential labellisé `# fragile: réutilise le client Codex, révocable par OpenAI`, en **SSE stateless uniquement** (mappe le canonique, évite le piège WS/`previous_response_id`). `originator=numen`.
4. **Coût d'opportunité** : ~150-250 lignes (`openai_chatgpt.rs` + `pkce.rs`), réutilise l'infra `agent-auth`/keyring déjà nécessaire pour Anthropic OAuth. ROI dogfood élevé, blast radius nul (gated). Pire scénario (OpenAI révoque) déjà couvert par l'archi multi-provider.

**À ne pas faire** : en faire le défaut, le mettre en AC P0, ou shipper WebSocket+`previous_response_id` (casserait compaction/resume). **À faire** : credential opt-in, SSE stateless, étiquette fragile, fallback (OpenAI-token) toujours présent.

---

## Données manquantes (non inventées)
- La forme exacte du mapping `tools[]`/`input[]` canonique → Responses API n'est **pas** dans l'extraction (seul le squelette du body l'est). À dériver de la doc Responses API à l'implémentation.
- Le wording exact d'un éventuel message de blocage côté OpenAI (équivalent du « only authorized for use with Claude Code ») est **inconnu** : Pi ne le capture pas. À sonder en live.
- La validation `originator` côté backend ChatGPT (rejette-t-il un originator inconnu ?) est **non vérifiée** — à tester avec `"numen"` au premier run.

> **ADR-10 à écrire** (input pour `docs/DECISIONS.md`) : « Auth abonnement ChatGPT via `ProviderKind::OpenAiChatGpt` (Responses API sur backend ChatGPT, SSE stateless, gated, fragile) — distinct de `OpenAiChat` (Chat Completions BYOK, P0) ». Acte aussi la clarification de scope US-017 (ne couvre pas l'abonnement).
