# Provider Inference and Model Discovery — Architecture Sketch

Status: proposal for independent review

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
- **Resolved provider**: validated, non-secret description of how that alias
  behaves, including its provider kind, authentication mode, endpoints, and
  API surfaces.
- **Provider backend**: provider-level runtime object created from a resolved
  provider and a protected credential handle.
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

The two paths independently resolve overlapping concerns:

- provider kind and alias;
- effective authentication mode;
- credentials and custom headers;
- base URL and transport policy;
- which concrete API behavior to use.

This creates two risks:

1. Inference and discovery can resolve the same configured provider
   differently.
2. `ProviderKind` can be asked to carry more meaning than it has. Provider
   identity, authentication, inference surface, and discovery surface are
   related but are not the same concept.

The problem is not that inference hides its concrete API surface. Hiding it
is desirable. The problem is that discovery cannot reuse the provider-level
decision that inference already made.

## Design goals

1. Resolve provider identity, authentication mode, endpoints, and API
   surfaces once through one authoritative path.
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

Introduce one provider-level resolution and two sibling capabilities:

```text
                    merged provider configuration
                               |
                               v
                     resolve_provider_backend
                               |
                               v
                      ProviderBackend
                     /               \
                    /                 \
                   v                   v
          inference_client()       model_discovery()
                   |                   |
                   v                   v
              AnyClient          ModelDiscoveryClient
                   |                   |
                   v                   v
              AnyModel          Vec<DiscoveredModel>
                                           |
                                           v
                                    application catalog
                                           |
                                           v
                                         picker
```

The important property is not whether the implementation uses traits or
enums. It is that both capabilities originate from the same resolved provider
description and credential policy.

### Resolved provider

Add a non-secret descriptor similar to:

```rust
pub struct ResolvedProvider {
    pub alias: String,
    pub kind: ProviderKind,
    pub auth_mode: ProviderAuth,
    pub inference_surface: InferenceSurface,
    pub discovery_surface: DiscoverySurface,
    pub inference_base_url: String,
    pub discovery_base_url: Option<String>,
}
```

The exact fields may change during implementation, but the descriptor must
distinguish provider identity from the operations used to communicate with
it. Inference and discovery URLs are separate because a provider is not
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

`ResolvedProvider` must not contain raw API keys, access tokens, or refresh
tokens. Credential resolution should produce a protected internal handle used
only when constructing a concrete capability. Secret-bearing types must have
redacted `Debug` implementations or no `Debug` implementation.

### Provider backend

`ProviderBackend` is the provider-centred interface. It owns the resolved
description and access to protected credentials, and lazily creates only the
capability a caller needs:

```rust
pub struct ProviderBackend {
    resolved: ResolvedProvider,
    credentials: CredentialHandle,
}

impl ProviderBackend {
    pub fn descriptor(&self) -> &ResolvedProvider;
    pub fn inference_client(&self) -> anyhow::Result<AnyClient>;
    pub fn model_discovery(&self) -> anyhow::Result<ModelDiscoveryClient>;
}
```

`CredentialHandle` is illustrative. The implementation may instead retain
private resolver callbacks or provider-specific refreshable credential
objects. The invariant is more important than the concrete type: inference
and discovery must use the same effective authentication decision and safety
policy.

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
`ProviderBackend`/`ResolvedProvider` decision rather than independently
resolving provider configuration inside `client.rs`.

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

An enum matches the existing `AnyClient`/`AnyModel` dispatch style and avoids
requiring an async-trait dependency. A trait is also reasonable if it makes
tests or plugin integration materially cleaner.

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

This layer owns concurrency, cache freshness, stale fallback, deduplication,
source precedence, ranking, and notices. It does not construct authentication
headers or select provider-specific response adapters.

Cache identity should be based on a non-secret discovery fingerprint such as:

```text
provider alias + provider kind + discovery surface + discovery base URL + auth mode
```

No credential material should enter the key. The fingerprint prevents a
catalog obtained through one resolved backend from being reused after a
material provider configuration change in the same process.

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
| Resolve alias, kind, auth mode, endpoints, and surfaces | provider backend resolver |
| Resolve and protect credentials | provider backend/auth layer |
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
  backend.rs             ResolvedProvider, surfaces, ProviderBackend factory
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
- Record base URLs, auth modes, headers-by-presence, and surface choices
  without snapshotting secrets.

Acceptance: tests describe current behavior before structural movement.

### Phase 2 — Introduce resolved-provider types

- Add `ResolvedProvider`, `InferenceSurface`, and `DiscoverySurface`.
- Extract one authoritative resolver from the overlapping logic in
  `create_client_with_auth` and `create_catalog_transport_with_auth`.
- Preserve existing validation, URL-safety, header, OAuth-refresh, and
  credential-precedence behavior.

Acceptance: the new resolver produces descriptors matching the Phase 1
characterization matrix; no callers change behavior yet.

### Phase 3 — Move inference construction behind ProviderBackend

- Construct `AnyClient` from `ProviderBackend` rather than re-resolving
  configuration in `client.rs`.
- Keep existing public/internal factory signatures as thin compatibility
  wrappers while migrating call sites.
- Verify startup, role clients, session switching, plugin switching, and
  `/model <id>` still choose the same concrete clients.

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
- Remove provider-specific adapter selection and credential construction from
  the aggregation layer.
- Key caches by the non-secret discovery fingerprint.

Acceptance: ordering, merging, notices, caching, and partial-failure behavior
remain stable.

### Phase 6 — Remove compatibility paths

- Delete duplicated provider/auth/base-URL resolution.
- Remove wrappers that no longer have callers.
- Document the provider-backend extension point for future adapters.

Acceptance: one authoritative provider resolution feeds both capabilities,
and provider-specific discovery code is unreachable from the TUI.

## Invariants

The refactor is complete only if these invariants hold:

1. A provider alias is resolved once per backend construction.
2. Inference and discovery cannot independently choose conflicting auth modes
   or endpoints for the same backend.
3. Provider-specific JSON never reaches the TUI.
4. Raw credentials never enter catalog entries, cache keys, notices, or logs.
5. Discovery is lazy, cancellable by dropping its task, bounded by timeout and
   response-size limits, and isolated per provider.
6. A discovery failure never mutates the active inference route.
7. Route installation remains atomic.
8. Providers without live discovery retain configured/current/suggested
   catalog behavior.
9. The abstraction refactor changes no provider-specific behavior by itself.

## Test strategy

### Provider-resolution tests

- Provider alias and `provider_type` resolution.
- Effective authentication selection.
- Inference and discovery surface selection.
- Separate inference/discovery base URLs.
- Custom-header and URL-safety policy preservation.
- Missing or invalid credential errors remain actionable and secret-free.

### Inference parity tests

- Every existing provider definition produces the same `AnyClient` variant as
  before the refactor.
- Default and explicit model handling is unchanged.
- Role clients and the active route use the shared resolver.
- OAuth refresh and billing-fallback behavior remain unchanged.

### Discovery parity tests

- Existing response fixtures produce the same normalized model metadata.
- Pagination, timeouts, body limits, and malformed responses behave as before.
- One provider failure does not suppress successful providers.
- Cache fingerprints change when a non-secret discovery-relevant setting
  changes and remain stable otherwise.

### Boundary tests

- TUI/catalog code has no provider-specific JSON parsing.
- Discovery adapters do not create `AnyModel` or rebuild agents.
- Inference construction does not populate application catalog entries.
- Debug/error formatting cannot reveal credentials.

## Open design questions for review

1. Should `ProviderBackend` be a struct with surface enums, or an enum whose
   variants encode fully resolved backend combinations?
2. Should `ModelDiscoveryClient` use enum dispatch, a trait, or a small
   function object?
3. Should the current `AnyClient` name remain, or should a later mechanical
   change rename it to `InferenceClient`?
4. Should surface selection eventually become configurable, or remain derived
   from provider type and authentication until a concrete use case requires a
   config field?
5. Should discovery and inference share one protected credential handle, or
   independently request credentials through the same resolver so refresh can
   occur at operation time?
6. Which non-secret fields are sufficient for a correct in-process discovery
   cache fingerprint?

These questions should be resolved on architectural grounds before adding new
provider-specific discovery behavior.

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
