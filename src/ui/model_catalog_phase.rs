//! Off-loop provider model discovery for `/model`.

use std::collections::HashMap;

use crate::config::{ProviderAuth, ProviderEntry};
use crate::provider::catalog::ModelCatalog;

pub(crate) struct ModelCatalogPhaseHandle {
    pub core: crate::ui::phase::PhaseHandle<ModelCatalog>,
    pub initial_query: String,
}

pub(crate) fn spawn(
    providers: HashMap<String, ProviderEntry>,
    default_auth: Option<ProviderAuth>,
    active_provider: String,
    active_model: String,
    initial_query: String,
    refresh: bool,
) -> ModelCatalogPhaseHandle {
    let core = crate::ui::phase::PhaseHandle::spawn(1, move |tx| async move {
        let catalog = crate::provider::catalog::load_model_catalog(
            providers,
            default_auth,
            active_provider,
            active_model,
            refresh,
        )
        .await;
        let _ = tx.send(catalog).await;
    });
    ModelCatalogPhaseHandle {
        core,
        initial_query,
    }
}
