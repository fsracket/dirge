# Model Catalog and Searchable Picker — Implementation Plan

Status: proposed

## Objective

Replace the current bare `/model` output with a searchable model picker that
shows models available through the user's configured providers. Selecting a
row switches the complete `(provider alias, model id)` route, so model choice
and provider choice cannot drift apart.

Keep `/model <exact-id>` working for scripts, muscle memory, and models that a
provider accepts but does not advertise. A future `/provider` command may use
the same route-switching primitive, but it is not required for this feature.

## User experience

The initial command surface is:

```text
/model                  open the searchable catalog
/model search <query>   open the catalog with an initial filter
/model refresh          refresh provider catalogs before opening
/model <exact-id>       switch directly, preserving current behavior
```

The picker searches across configured provider aliases and displays enough
route context to make duplicate model ids unambiguous:

```text
anthropic   claude-sonnet-4-6       Claude Sonnet 4.6   200k
openrouter  anthropic/claude-opus-4 Claude Opus 4       200k
ollama      qwen3-coder:latest      Qwen3 Coder         local
```

Picker controls:

- Printable characters update a fuzzy-search query.
- Backspace edits the query; Up/Down move selection.
- Enter activates the selected route.
- Esc closes the picker without changing the session.
- The current route and config-pinned models rank ahead of ordinary results.
- Failures from one provider appear as a non-blocking notice; successful
  catalogs from other providers remain usable.

When the agent is idle, bare `/model` opens the picker. During an active turn,
the same command remains safe but renders the current route plus "model picker
available when idle" instead of opening a modal. This preserves the existing
read-only inspection behavior without making picker keystrokes compete with
interrupt and interjection input.

## Catalog semantics

Introduce a provider-neutral record in a new `src/provider/catalog.rs`:

```rust
pub struct ModelCatalogEntry {
    pub provider_alias: String,
    pub id: String,
    pub display_name: Option<String>,
    pub context_window: Option<u64>,
    pub supports_tools: Option<bool>,
    pub supports_images: Option<bool>,
    pub source: CatalogSource,
}
```

Catalog results merge, in precedence order:

1. Models returned by authenticated provider discovery.
2. `providers.<alias>.model` entries from merged config.
3. The live session route.
4. Recently used `(provider, model)` pairs from saved sessions, if cheap to
   obtain through the existing session index.
5. A deliberately small built-in fallback containing each provider's default
   and a few common choices.

Remote results mean "advertised by this endpoint under the current
credentials," not a guarantee that every subsequent request will be accepted.
Fallback and historical rows must be visibly marked as suggested or recent so
the UI does not overstate availability.

### Provider adapters

Use small response adapters rather than teaching the UI provider-specific
JSON:

- OpenAI-compatible (`openai`, `deepseek`, `glm`, `cerebras`, `opencode`, and
  compatible custom aliases): `GET <base>/models`, parse `data[].id`.
- OpenRouter: `GET /api/v1/models`, retaining useful context/capability fields.
- Anthropic: paginated `GET /v1/models`.
- Gemini: paginated `models.list`, filtering for generation-capable models.
- Ollama: `GET /api/tags`, representing installed local models only.
- Plugin providers: use their declared provider type/base URL when the adapter
  is known; otherwise fall back to configured/default/recent entries.

Do not duplicate credential resolution in the UI. Refactor the internal
provider client construction path to expose a private catalog transport that
uses the same resolved base URL, authentication mode, custom headers, OAuth
refresh behavior, and insecure-URL policy as completion requests. Secret-
bearing transport types must not implement verbose `Debug` output.

Each request must have a short timeout, a bounded response body, and isolated
failure handling. Fetch provider aliases concurrently. Cache successful
results in memory for the process lifetime with a 15-minute freshness window;
retain the last successful value as stale fallback after a refresh failure.
Disk caching is deferred until usage demonstrates that startup-to-startup
latency needs it.

## Runtime route correctness prerequisite

Before picker selection can safely switch providers, centralize active-route
installation. Today `/model` can replace the live client, while `build_agent`
still derives provider identity from launch-time CLI/config state. That can
leave the transport on one provider and provider-specific timeout, model-family
steering, telemetry, or auxiliary routing on another.

Add an explicit runtime route value:

```rust
pub struct ActiveRoute {
    pub provider_alias: String,
    pub model_id: String,
    pub model_explicit: bool,
}
```

Create one atomic `switch_active_route` operation shared by:

- picker selection;
- `/model <exact-id>` including inferred cross-provider switches;
- plugin `prepare-next-run` model changes;
- future `/provider` support;
- session switching, which must rebuild the client for the saved provider.

The operation must:

1. Resolve and canonicalize the provider alias.
2. Construct/authenticate the target client without mutating live state.
3. Resolve provider-specific default/Codex model naming.
4. Build the replacement agent with the runtime provider alias supplied
   explicitly.
5. Commit client, agent, session provider, model, `model_explicit`, and context
   window together.
6. Preserve the old route and render an actionable error if preparation fails.
7. Reuse the current context-shrink warning and normal slash-command session
   persistence.

Change `provider::build_agent` to receive the active provider alias rather than
calling `cli.resolve_provider(cfg)` internally. Audit every rebuild call site
and every auxiliary-role comparison that currently treats
`ConfigRole::Default` as necessarily equal to the live route.

## Asynchronous UI architecture

Provider discovery must never run inline inside `cmd_model` or block the input
loop.

1. Add `SlashOutcome::DeferModelPicker { initial_query, refresh }`.
2. Add `ui/model_catalog_phase.rs`, implemented with the existing generic
   `PhaseHandle`, to fetch and merge catalogs off the UI thread.
3. Store the phase handle in `UiState` and enter a distinct catalog-loading
   modal state, preventing prompt submission while preserving Ctrl+C/Esc
   cancellation. Do not overload the agent-running state with catalog work.
4. When the phase completes, enter
   `InputMode::ModelPicker(ModelPickerState)` and render through the existing
   picker overlay.
5. On Enter, leave the modal and run the atomic route switch on the UI thread.
6. On Esc or catalog cancellation, restore compose mode without changing the
   route.

Generalize `ui/picker.rs` with a typed searchable picker instead of adding
model-specific key handling to `ui/mod.rs`. Reuse `nucleo-matcher`, already
used by chat search, for ranked fuzzy matching. Keep the existing rewind and
file pickers working; migrate them only when doing so simplifies the code
rather than expanding this feature's scope.

## Implementation sequence

### Phase 1 — Active-route seam

- Add `ActiveRoute` and atomic route preparation/installation.
- Pass the runtime provider alias through `build_agent` and all rebuild paths.
- Move `/model <exact-id>` and plugin swaps onto the shared path.
- Correctly set `session.model_explicit = true` for an explicit `/model` id.
- Make `/sessions switch` install a matching provider client before rebuilding.

Acceptance: after every switch, client variant, agent provider metadata,
session provider/model, timeout resolution, and banner agree.

### Phase 2 — Catalog core

- Add normalized catalog types and merge/dedup/ranking logic.
- Add authenticated adapters and pagination.
- Add concurrent fetching, timeouts, response limits, and in-memory caching.
- Add configured/current/recent/default fallbacks.

Acceptance: deterministic tests using fixture responses produce stable,
deduplicated rows; no tests require network access or real credentials.

### Phase 3 — Searchable picker

- Add a typed fuzzy-search picker with query editing and selection.
- Add catalog phase and model-picker input mode.
- Change bare `/model`, `/model search`, and `/model refresh` dispatch.
- Keep active-turn `/model` non-modal and read-only.
- Selecting a row invokes the shared active-route operation.

Acceptance: the TUI remains responsive during discovery, partial provider
failures are visible but non-fatal, Esc is side-effect-free, and Enter switches
the exact displayed route.

### Phase 4 — Documentation and polish

- Update the README slash-command table and `docs/tui.md`.
- Document live, cached, recent, configured, and suggested provenance.
- Add empty/error/loading states and context-window/capability display.
- Evaluate whether `/provider` adds value as a provider-filter shortcut.

## Test plan

### Unit tests

- Parse each provider response from checked-in JSON fixtures.
- Follow Anthropic/Gemini pagination and stop safely on malformed tokens.
- Merge duplicates without losing the strongest provenance or metadata.
- Rank active, configured, remote, recent, and suggested rows predictably.
- Fuzzy matching is case-insensitive, typo-tolerant, and deterministic.
- Picker editing, navigation, Enter, and Esc handle empty lists safely.
- Cache freshness, forced refresh, stale fallback, timeout, and body limits.
- Secrets never appear in `Debug` or rendered errors.

### Route integration tests

- Selecting an entry changes both provider and model.
- Unknown providers, missing credentials, and failed agent preparation leave
  the old route untouched.
- Provider defaults and explicitly configured models set `model_explicit`
  correctly.
- Context-window changes and shrink warnings match direct `/model` behavior.
- Runtime provider alias reaches timeout resolution and model-family steering.
- Session and plugin switches use the same route path.

### UI tests

- Bare `/model` yields the deferred picker outcome.
- `/model <exact-id>` remains backward compatible.
- Catalog loading does not block key processing and is cancellable.
- One failed provider does not suppress successful provider results.
- The canonical slash-command registry and busy-command policy remain aligned.
- Picker overlays render query, source/provider labels, selection, empty state,
  and errors within narrow terminal widths.

## Non-goals

- Guaranteeing that every advertised model will accept a completion request.
- Fetching pricing or benchmark data unless it comes free in an existing
  catalog response.
- Persistently rewriting `config.json` when a session route changes.
- Building a general provider marketplace.
- Requiring all plugin/custom providers to implement discovery.
- Replacing direct `/model <id>` usage.

## Open decisions before implementation

1. Should bare `/model` query all configured providers immediately, or show the
   active provider first and fetch others lazily? Recommendation: fetch all
   concurrently, because partial results and caching bound the latency.
2. Should recent session models ship in the first release? Recommendation: add
   them only if the existing session index can answer without filesystem-wide
   scans.
3. Should the picker allow a free-form "use query as model id" row?
   Recommendation: defer; `/model <exact-id>` already provides an unambiguous
   escape hatch.
4. Should discovered context windows update static model metadata globally?
   Recommendation: use catalog metadata for the selected session when present,
   but do not mutate configuration or checked-in tables.
