# Provider Inference and Model Discovery — Architecture Sketch

Status: implemented on `codex/model-catalog-picker`

Implementation: `provider::backend` owns stable provider definitions and
operation-time capability resolution; `provider::client` constructs inference
clients from resolved inference plans; `provider::discovery` owns normalized
live model listing; and `provider::catalog` retains application enumeration,
caching, merging, ranking, and notices. Add future wire adapters by extending
the discovery surface and `ModelDiscoveryClient` enum without adding provider
conditionals to the catalog or TUI.

## Purpose

Refactor Dirge's provider abstractions so that inference and model discovery
are separate capabilities produced from one shared provider resolution.

This is an abstraction-only proposal. It should preserve current runtime and
catalog behavior while creating the seam needed for later provider-specific
discovery work.

## Scope boundary

This proposal does not change:

- provider authentication behavior;
- inference endpoints or wire formats;
- model-list endpoints or response parsers;
- provider or model defaults;
- model-picker behavior or presentation;
- configuration syntax;
- the set of providers or models currently supported.

In particular, it does not specify or implement a fix for any individual
provider's discovery flow. Such work should become smaller after this
refactor because it can be implemented inside the appropriate discovery
adapter without changing inference or TUI code.

## Terminology

Use these terms consistently:

- **Provider definition**: merged configuration for one provider alias.
- **Resolved provider definition**: validated, non-secret description of the
  stable policy for an alias: provider kind, configured authentication policy,
  configured endpoints, headers-by-presence, and rules for selecting API
  surfaces. It does not claim that credentials have already been resolved.
- **Credential shape**: non-secret facts that can affect routing, such as
  whether a credential is present, whether it is an API key or OAuth token,
  and whether it can be refreshed. It never includes the credential value.
- **Resolved capability**: operation-time, non-secret plan produced from a
  resolved provider definition plus the current credential shape. It names
  the effective authentication mode, API surface, and base URL for either
  inference or discovery.
- **Provider backend**: provider-level runtime object that owns the resolved
  definition and the shared policy for resolving fresh credentials. It does
  not retain one token forever.
- **Inference client**: the existing `AnyClient`; an authenticated client that
  creates model handles and sends inference requests.
- **Model handle**: the existing `AnyModel`; an inference client bound to one
  model id.
- **Model discovery**: a narrow provider capability that lists models and
  normalizes provider-specific responses.
- **Application catalog**: the aggregate of live discovery results plus local
  sources such as configured, current, cached, and suggested entries.

`AnyClient` may retain its current name during this refactor. In architecture
discussion it should be called the inference client so it is not confused
with provider-level discovery.

## Current architecture

Inference already has a useful high-level abstraction:

```text
ProviderEntry + environment
        |
        v
create_client_with_auth
        |
        v
AnyClient (authenticated inference connection)
        |
        +-- completion_model(model id) --> AnyModel
                                              |
                                              v
                                      prompt / stream response
```

Callers do not need to know which concrete client, endpoint, authentication
mechanism, or inference wire format is used. That is good encapsulation.

Model discovery was added separately:

```text
ProviderEntry + environment
        |
        v
create_catalog_transport_with_auth
        |
        v
CatalogTransport
        |
        v
adapter selected from ProviderKind
        |
        v
Vec<ModelCatalogEntry>
```

The two paths independently resolve overlapping concerns. In the current
branch, `create_catalog_transport_with_auth` re-derives substantial behavior
already owned by inference construction, including:

- provider kind and alias;
- effective authentication mode;
- credentials and custom headers;
- base URL and transport policy;
- which concrete API behavior to use;
- authentication-kind safety guards;
- OAuth-specific endpoint overrides and beta/account headers;
- provider default base URLs.

This is not merely similar code. It is duplicated policy: a future inference
authentication change can succeed while discovery silently retains the old
headers, endpoint, or credential precedence.

This creates two risks:

1. Inference and discovery can resolve the same configured provider
   differently.
2. `ProviderKind` can be asked to carry more meaning than it has. Provider
   identity, authentication, inference surface, and discovery surface are
   related but are not the same concept.

The current adapter dispatch makes the second risk concrete. It handles a
small set of provider kinds explicitly and routes every remaining kind through
the OpenAI-compatible discovery parser. That makes an implicit compatibility
claim for custom and future provider kinds. A separate discovery-surface
decision turns that wildcard assumption into an explicit, reviewable choice.

The problem is not that inference hides its concrete API surface. Hiding it
is desirable. The problem is that discovery cannot reuse the provider-level
decision that inference already made.

## Design goals

1. Resolve stable provider identity and policy once through one authoritative
   path, then resolve current credential shape and operation-specific API
   surfaces through that same policy at operation time.
2. Keep inference and discovery as separate, narrow capabilities.
3. Keep provider-specific wire formats below provider-neutral interfaces.
4. Prevent credentials from entering catalog records, UI state, cache keys,
   logs, or ordinary `Debug` output.
5. Preserve partial catalog results when one provider fails.
6. Preserve the atomic route-switching seam already used by `/model` and
   session switching.
7. Make a future provider-specific discovery adapter change local to the
   discovery layer.
8. Preserve all current behavior during the abstraction migration.

## Non-goals

- A universal provider SDK.
- A provider marketplace or plugin redesign.
- Pricing, quota, billing, or benchmark discovery.
- Model compatibility guarantees beyond what a provider advertises.
- Eagerly constructing inference clients for every provider during catalog
  loading.
- Exposing inference or discovery surfaces in the TUI.
- Renaming every existing provider type in the first change.

## Proposed architecture

Introduce one stable provider definition, one shared credential-resolution
policy, and two sibling capabilities. Credential values are resolved at the
moment an operation needs them:

```text
                 merged provider configuration
                              |
                              v
             ResolvedProviderDefinition
                              |
                              v
                     ProviderBackend
              (definition + credential policy)
                    /                     \
                   /                       \
                  v                         v
       inference_client()              model_discovery()
                  |                         |
                  +---- resolve current ----+
                        credential shape
                        and fresh secret
                              |
                  +-----------+------------+
                  |                        |
                  v                        v
       ResolvedInference          ResolvedDiscovery
                  |                        |
                  v                        v
             AnyClient           ModelDiscoveryClient
                  |                        |
                  v                        v
             AnyModel            Vec<DiscoveredModel>
                                           |
                                           v
                                    application catalog
                                           |
                                           v
                                         picker
```

The important property is not whether the implementation uses traits or
enums. It is that both capabilities originate from the same resolved provider
definition and credential policy. They share the rules, not a token captured
at backend-construction time.

### Two-stage resolution

Provider resolution has a stable stage and an operation-time stage.

The stable stage validates configuration and records policy without reading or
retaining secret values:

```rust
pub struct ResolvedProviderDefinition {
    pub alias: String,
    pub kind: ProviderKind,
    pub configured_auth: Option<ProviderAuth>,
    pub configured_base_url: Option<String>,
    pub credential_policy: CredentialPolicy,
}
```

The operation-time stage asks the shared credential resolver for both a fresh
secret and its non-secret shape, then derives the effective surface and URL:

```rust
pub struct CredentialShape {
    pub kind: CredentialKind,
    pub refreshable: bool,
}

pub struct ResolvedInference {
    pub auth_mode: ProviderAuth,
    pub inference_surface: InferenceSurface,
    pub inference_base_url: String,
    // Secret-bearing credential stays private to construction.
}

pub struct ResolvedDiscovery {
    pub auth_mode: ProviderAuth,
    pub discovery_surface: DiscoverySurface,
    pub discovery_base_url: Option<String>,
    // Secret-bearing credential stays private to construction.
}
```

`CredentialKind` is intentionally left as an implementation-defined enum in
this sketch. Phase 1 characterization should determine the minimum variants
needed to preserve current credential-source distinctions; the reference here
is deliberate, not an omitted provider list.

The exact fields may change during implementation. The required ordering does
not: effective surface and base URL are derived from provider policy plus the
current credential shape. This accommodates existing behavior in which
credential presence or kind affects authentication mode, endpoint choice, or
wire behavior, while keeping secret values out of ordinary descriptors.

Inference and discovery URLs remain distinct because a provider is not
required to expose both operations under the same base path.

Example surface enums:

```rust
pub enum InferenceSurface {
    ChatCompletionsCompatible,
    ResponsesCompatible,
    AnthropicMessages,
    GeminiGenerateContent,
    Ollama,
}

pub enum DiscoverySurface {
    OpenAiModelsCompatible,
    AnthropicModels,
    GeminiModels,
    OllamaTags,
    Unavailable,
}
```

These names describe wire behavior, not provider brands. Multiple provider
aliases may legitimately use the same surface.

`ResolvedProviderDefinition`, `CredentialShape`, and resolved capability
metadata must not contain raw API keys, access tokens, or refresh tokens.
Secret-bearing values exist only inside the operation-time construction path.
Secret-bearing types must have redacted `Debug` implementations or no `Debug`
implementation.

### Provider backend

`ProviderBackend` is the provider-centred interface. It owns the stable
definition and a shared credential-resolution policy, then lazily creates only
the capability a caller needs:

```rust
pub struct ProviderBackend {
    definition: ResolvedProviderDefinition,
}

impl ProviderBackend {
    pub fn descriptor(&self) -> &ResolvedProviderDefinition;
    pub fn inference_client(&self) -> anyhow::Result<AnyClient>;
    pub fn model_discovery(&self) -> anyhow::Result<ModelDiscoveryClient>;
}
```

`CredentialPolicy` has one home: `ResolvedProviderDefinition`. It is a stable,
non-secret recipe for applying credential precedence and obtaining current
credentials; it is not one resolved token. `ProviderBackend` invokes that
policy when constructing either capability, conceptually
`self.definition.credential_policy.resolve_now()`.

Each capability method therefore resolves credentials at operation time. That
is required for refreshable OAuth credentials and sparse operations: a
discovery call may occur long after backend construction or after a 15-minute
catalog cache interval.

For inference, **operation time means inference-client construction**. A built
`AnyClient` retains the credentials and refresh behavior of its concrete
variant for the life of that active route, exactly as it does today. This
refactor does not introduce per-prompt credential re-resolution. Constructing
a later replacement `AnyClient` consults `CredentialPolicy` again and can use
newly refreshed credentials.

Inference and discovery may receive different fresh token values at different
times. They must nevertheless use the same precedence rules, credential-kind
classification, refresh behavior, authentication guards, URL-safety policy,
and header construction rules.

The operation-time sequence is:

```text
ProviderBackend method called
        |
        v
resolve current credential and CredentialShape
        |
        v
derive operation surface + base URL + auth/header policy
        |
        v
construct concrete inference or discovery capability
```

Capability construction should remain lazy:

- opening `/model` constructs discovery capabilities but not inference
  clients for every provider;
- selecting a route constructs the target inference client but does not need
  to repeat live discovery;
- a provider with no remote catalog may still support inference;
- a discovery failure must not disturb the active inference client.

### Inference capability

The current inference abstraction remains conceptually intact:

```rust
impl AnyClient {
    pub fn completion_model(&self, id: impl Into<String>) -> AnyModel;
}
```

Provider-specific inference clients and request serialization continue to be
hidden inside `AnyClient`, `AnyModel`, and existing stream dispatch.

The main change is construction: `AnyClient` should be created from the shared
`ProviderBackend` definition and operation-time resolution rather than
independently resolving provider configuration inside `client.rs`.

This proposal does not require renaming `AnyClient`. An eventual rename to
`InferenceClient` could improve clarity, but it would create broad mechanical
churn and should be evaluated separately.

### Model-discovery capability

Keep the discovery interface narrow:

```rust
pub struct DiscoveredModel {
    pub id: String,
    pub display_name: Option<String>,
    pub context_window: Option<u64>,
    pub supports_tools: Option<bool>,
    pub supports_images: Option<bool>,
}

pub enum ModelDiscoveryClient {
    OpenAiCompatible(/* private transport */),
    Anthropic(/* private transport */),
    Gemini(/* private transport */),
    Ollama(/* private transport */),
    Unavailable,
}

impl ModelDiscoveryClient {
    pub async fn list_models(&self) -> anyhow::Result<Vec<DiscoveredModel>>;
}
```

Use enum dispatch for the initial implementation. It matches the existing
`AnyClient`/`AnyModel` house style, keeps supported adapters explicit, and
avoids requiring an async-trait dependency. A trait can be reconsidered only
if a concrete plugin or testing requirement cannot be expressed cleanly
through the enum.

`list_models` owns provider-specific HTTP calls, pagination, response limits,
and response parsing. It returns normalized live results only. It does not
own:

- application caching;
- local fallback models;
- current or configured model insertion;
- cross-provider ranking;
- fuzzy searching;
- route switching.

An optional `get_model(id)` operation can be added later if a provider API can
retrieve richer metadata efficiently. It should not be added until there is a
real consumer; `list_models` is sufficient for this refactor.

Adapters should preserve current pagination behavior during the refactor. A
future behavior change may return successfully collected pages plus a notice
when a provider exceeds a pagination bound, rather than discarding all pages.
That change belongs entirely inside discovery and is intentionally deferred.

### Application catalog

`provider::catalog` remains the provider-neutral aggregation layer:

```text
ModelDiscoveryClient::list_models()
                 + configured models
                 + current route
                 + suggested models
                 + cached/stale results
                           |
                           v
                  Vec<ModelCatalogEntry>
```

Before starting discovery, this layer also owns **provider enumeration**. It
decides which aliases should be queried by combining application policy such
as:

- provider aliases present in merged configuration;
- the active provider alias;
- built-in aliases whose supported credential sources are present.

This is distinct from resolving one alias. `ProviderBackend` answers "how do I
operate this provider?" while the application catalog answers "which
providers should participate in this catalog load?" Keeping enumeration here
prevents it from being duplicated in provider construction or the TUI.

The catalog layer therefore owns enumeration, concurrency, cache freshness,
stale fallback, deduplication, source precedence, ranking, and notices. It does
not construct authentication headers or select provider-specific response
adapters.

For this behavior-preserving refactor, retain the current lowercased-alias
cache key. Provider configuration is immutable for the life of the process,
so a more detailed fingerprint does not fix a current collision.

A future runtime-config-reload feature should replace the alias key with a
non-secret discovery fingerprint such as:

```text
provider alias + provider kind + discovery surface + discovery base URL + auth mode
```

No credential material should enter such a future key. Deferring the change
keeps cache semantics stable during this refactor and avoids presenting
defensive future-proofing as a current bug fix.

### TUI and route switching

The TUI continues to consume only `ModelCatalogEntry`. It should not branch on
`ProviderKind`, `ProviderAuth`, `InferenceSurface`, or `DiscoverySurface`.

Route switching continues to prepare replacements before installation:

```text
selected provider alias + model id
              |
              v
       resolve ProviderBackend
              |
              v
       build inference client
              |
              v
       build model and agent
              |
              v
atomically install client + agent + session route
```

This refactor should change where the inference client comes from, not the
atomic installation behavior.

## Ownership boundaries

| Concern | Owner |
|---|---|
| Merge global/project provider configuration | `config` |
| Select which aliases participate in a catalog load | `provider::catalog` |
| Resolve stable alias, kind, configured endpoints, and credential policy | provider backend resolver |
| Resolve fresh credentials and credential shape per operation | provider backend/auth layer |
| Derive operation-specific auth mode, endpoint, and surface | provider backend resolver |
| Build inference client | provider backend + `client` |
| Create a model handle | `AnyClient` |
| Send/stream inference | `AnyModel` and stream dispatch |
| Perform provider-specific model listing | `ModelDiscoveryClient` |
| Cache and merge model sources | `provider::catalog` |
| Search and render catalog entries | TUI picker |
| Atomically install a selected route | `provider::route` + slash UI seam |

## Suggested file boundaries

The exact layout is negotiable, but a reviewable decomposition would be:

```text
src/provider/
  backend.rs             stable definition, capability plans, surfaces, factory
  client.rs              inference-client construction from ProviderBackend
  discovery/
    mod.rs               DiscoveredModel and ModelDiscoveryClient dispatch
    openai_compatible.rs response adapter
    anthropic.rs         response adapter
    gemini.rs            response adapter
    ollama.rs            response adapter
  catalog.rs             cache, merge, ranking, fallbacks, notices
  route.rs               prepare/install inference route
```

Keeping adapters in one file initially is acceptable if splitting them would
create more navigation than clarity. The important boundary is that adapter
selection belongs to discovery construction, not to the TUI or catalog
aggregator.

## Migration sequence

Each phase should preserve behavior and be independently testable.

### Phase 1 — Characterize current resolution

- Add table-driven tests covering the current mapping from provider
  definitions to concrete inference-client variants.
- Add equivalent characterization tests for current discovery transports and
  adapters.
- Record credential source/shape, base URLs, auth modes, headers-by-presence,
  and surface choices without snapshotting secrets.
- Cover cases where credential presence or kind changes effective auth,
  endpoint, or concrete client variant.

Acceptance: tests describe current behavior before structural movement.

### Phase 2 — Introduce provider-definition and capability types

- Add `ResolvedProviderDefinition`, `CredentialShape`, resolved capability
  plans, `InferenceSurface`, and `DiscoverySurface`.
- Extract one authoritative resolver from the overlapping logic in
  `create_client_with_auth` and `create_catalog_transport_with_auth`.
- Split stable config validation from operation-time credential resolution;
  do not attempt to finalize surfaces or base URLs before credential shape is
  known.
- Preserve existing validation, URL-safety, header, OAuth-refresh, and
  credential-precedence behavior.

Acceptance: the stable definition plus operation-time resolver produce
capability plans matching the Phase 1 characterization matrix; no callers
change behavior yet.

### Phase 3 — Move inference construction behind ProviderBackend

- Construct `AnyClient` from `ProviderBackend` rather than re-resolving
  configuration in `client.rs`.
- Keep existing public/internal factory signatures as thin compatibility
  wrappers while migrating call sites.
- Verify startup, role clients, session switching, plugin switching, and
  `/model <id>` still choose the same concrete clients.
- Replace role-vs-active comparisons that manufacture
  `ProviderEntry::default()` merely to satisfy an old tuple shape. Compare the
  live resolved alias with the role alias directly.

Acceptance: inference tests and client-variant characterization tests remain
unchanged.

### Phase 4 — Introduce ModelDiscoveryClient

- Move provider-specific live-listing transport and adapter selection behind
  `ProviderBackend::model_discovery`.
- Make discovery adapters return `DiscoveredModel` rather than application
  catalog entries.
- Retain current endpoint, parsing, timeout, pagination, and error behavior.

Acceptance: existing fixture tests produce equivalent normalized live rows.

### Phase 5 — Simplify the application catalog

- Change `provider::catalog` to request discovery capabilities from resolved
  provider backends.
- Keep `discovery_aliases` (or its successor) in the catalog layer as the one
  owner of provider-enumeration policy.
- Remove provider-specific adapter selection and credential construction from
  the aggregation layer.
- Preserve the current lowercased-alias cache key; defer a richer fingerprint
  until runtime provider configuration can actually change.

Acceptance: ordering, merging, notices, caching, and partial-failure behavior
remain stable.

### Phase 6 — Remove compatibility paths

- Delete duplicated provider/auth/base-URL resolution.
- Remove wrappers that no longer have callers.
- Document the provider-backend extension point for future adapters.

Acceptance: one authoritative stable definition and one operation-time
credential/surface resolution policy feed both capabilities, and
provider-specific discovery code is unreachable from the TUI.

## Invariants

The refactor is complete only if these invariants hold:

1. Stable provider configuration is validated once per backend construction.
2. Credentials are resolved when constructing an inference or discovery
   capability, not frozen when the backend is constructed. Inference does not
   re-resolve credentials for every prompt.
3. Inference and discovery share credential precedence, credential-kind
   classification, refresh, auth-guard, URL-safety, and header policies.
4. Effective surfaces and base URLs are derived only after current credential
   shape is known.
5. Provider enumeration has one owner in the application catalog.
6. Provider-specific JSON never reaches the TUI.
7. Raw credentials never enter provider descriptors, catalog entries, cache
   keys, notices, or logs.
8. Discovery is lazy, cancellable by dropping its task, bounded by timeout and
   response-size limits, and isolated per provider.
9. A discovery failure never mutates the active inference route.
10. Route installation remains atomic.
11. Providers without live discovery retain configured/current/suggested
   catalog behavior.
12. The abstraction refactor changes no provider-specific behavior or cache
    identity by itself.

## Test strategy

### Provider-resolution tests

- Provider alias and `provider_type` resolution.
- Effective authentication selection.
- Credential-presence/kind cases that affect the operation plan.
- Inference and discovery surface selection.
- Separate inference/discovery base URLs.
- Custom-header and URL-safety policy preservation.
- Missing or invalid credential errors remain actionable and secret-free.
- Safe provider descriptors never contain raw credential values.

### Inference parity tests

- Every existing provider definition produces the same `AnyClient` variant as
  before the refactor.
- Default and explicit model handling is unchanged.
- Role clients and the active route use the shared resolver.
- OAuth refresh and billing-fallback behavior remain unchanged.
- A credential refreshed after backend construction is used by a later
  inference-client construction.

### Discovery parity tests

- Existing response fixtures produce the same normalized model metadata.
- Pagination, timeouts, body limits, and malformed responses behave as before.
- One provider failure does not suppress successful providers.
- Configured, active, and credentialed built-in alias enumeration remains
  equivalent to current behavior.
- A credential refreshed after backend construction is used by a later
  discovery operation.
- Existing lowercased-alias cache behavior remains unchanged.

### Boundary tests

- TUI/catalog code has no provider-specific JSON parsing.
- Discovery adapters do not create `AnyModel` or rebuild agents.
- Inference construction does not populate application catalog entries.
- Debug/error formatting cannot reveal credentials.

## Design decisions after review

The initial review resolves the major shape questions as follows:

1. **Use a `ProviderBackend` struct plus surface enums.** An enum containing
   every fully resolved provider/auth/surface combination would grow
   combinatorially and obscure the fact that many aliases share wire
   surfaces.
2. **Use enum dispatch for `ModelDiscoveryClient`.** This matches
   `AnyClient`/`AnyModel`, makes wildcard compatibility assumptions visible,
   and avoids a new async-trait dependency.
3. **Keep the `AnyClient` name.** Treat it as the inference client in design
   discussion; a broad rename is unrelated mechanical churn.
4. **Derive surfaces internally.** Do not add a configuration field until a
   concrete user requirement needs one.
5. **Resolve credentials at operation time.** Inference and discovery share
   one resolution policy, not one token value captured during backend
   construction.
6. **Preserve alias-only cache identity.** A richer non-secret fingerprint is
   deferred until provider configuration can change during a process.

Implementation still has freedom over private type names, whether the
credential resolver is a struct or function object, and whether discovery
adapters begin in one file or several. Those choices must preserve the
decisions and invariants above.

## Review criteria

Independent reviewers should evaluate whether the proposal:

- preserves the useful high-level inference abstraction;
- introduces a genuinely provider-centred seam rather than another parallel
  resolver;
- keeps inference and discovery independently usable and testable;
- prevents provider-specific conditionals from leaking into the catalog and
  TUI layers;
- avoids premature configuration or plugin API expansion;
- can be migrated incrementally without changing current behavior;
- provides a clear local extension point for future discovery adapters.
