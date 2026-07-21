//! `/model` and `/reasoning` handlers.

use crate::provider::provider_for_model;
use crate::ui::slash::{SlashCtx, SlashOutcome, c_agent, c_error};

#[derive(Debug, PartialEq, Eq)]
enum ModelRequest<'a> {
    InspectBusy,
    Catalog {
        initial_query: String,
        refresh: bool,
    },
    Switch(&'a str),
}

fn parse_model_request<'a>(parts: &'a [&'a str], running: bool) -> ModelRequest<'a> {
    match parts.get(1).copied() {
        None if running => ModelRequest::InspectBusy,
        None => ModelRequest::Catalog {
            initial_query: String::new(),
            refresh: false,
        },
        Some("search") => ModelRequest::Catalog {
            initial_query: parts.get(2..).unwrap_or_default().join(" "),
            refresh: false,
        },
        Some("refresh") => ModelRequest::Catalog {
            initial_query: String::new(),
            refresh: true,
        },
        Some(model) => ModelRequest::Switch(model.trim()),
    }
}

pub(crate) async fn cmd_model(
    ctx: &mut SlashCtx<'_>,
    parts: &[&str],
) -> anyhow::Result<SlashOutcome> {
    let requested = match parse_model_request(parts, *ctx.is_running) {
        ModelRequest::InspectBusy => {
            ctx.renderer.write_line(
                &format!(
                    "current route: {}  ·  {}",
                    ctx.session.provider, ctx.session.model
                ),
                c_agent(),
            )?;
            ctx.renderer
                .write_line("model picker available when idle", c_agent())?;
            return Ok(SlashOutcome::Handled);
        }
        ModelRequest::Catalog {
            initial_query,
            refresh,
        } => {
            return Ok(SlashOutcome::DeferModelPicker {
                initial_query,
                refresh,
            });
        }
        ModelRequest::Switch(model) => model,
    };
    let providers = ctx.cfg.providers_map();
    let provider_alias =
        match provider_for_model(&providers, ctx.session.provider.as_str(), requested) {
            Ok(alias) => alias,
            Err(e) => {
                ctx.renderer.write_line(
                    &format!(
                        "could not switch model: {e} Keeping '{}' on '{}'.",
                        ctx.session.model, ctx.session.provider
                    ),
                    c_error(),
                )?;
                return Ok(SlashOutcome::Handled);
            }
        };
    let report = match crate::ui::slash::switch_active_route_parts(
        ctx.agent,
        ctx.client,
        ctx.session,
        &provider_alias,
        requested,
        true,
        ctx.cli,
        ctx.cfg,
        ctx.context,
        ctx.permission,
        ctx.ask_tx,
        ctx.question_tx,
        ctx.plan_tx,
        ctx.bg_store,
        ctx.sandbox,
        #[cfg(feature = "mcp")]
        ctx.mcp_manager,
        #[cfg(feature = "semantic")]
        ctx.semantic_manager,
        #[cfg(feature = "lsp")]
        ctx.lsp_manager,
    )
    .await
    {
        Ok(report) => report,
        Err(e) => {
            ctx.renderer.write_line(
                &format!(
                    "could not prepare provider '{provider_alias}': {e}. Keeping '{}' on '{}'.",
                    ctx.session.model, ctx.session.provider
                ),
                c_error(),
            )?;
            return Ok(SlashOutcome::Handled);
        }
    };

    ctx.renderer.write_line(
        &format!(
            "switched to model: {}  ·  {}",
            ctx.session.model, ctx.session.provider
        ),
        c_agent(),
    )?;
    let reserve = ctx.cfg.resolve_reserve_tokens();
    let budget = report.new_context_window.saturating_sub(reserve);
    if report.new_context_window < report.old_context_window
        && ctx.session.total_estimated_tokens > budget
    {
        ctx.renderer.write_line(
            &format!(
                "warning: session uses ~{}k tokens but new model's context budget is ~{}k. Run /compress before the next prompt or the next turn may overflow.",
                ctx.session.total_estimated_tokens / 1_000,
                budget / 1_000,
            ),
            c_error(),
        )?;
    }
    Ok(SlashOutcome::Handled)
}

pub(crate) async fn cmd_reasoning(ctx: &mut SlashCtx<'_>) -> anyhow::Result<()> {
    *ctx.show_reasoning = !*ctx.show_reasoning;
    ctx.renderer.write_line(
        &format!(
            "reasoning visibility: {}",
            if *ctx.show_reasoning { "on" } else { "off" }
        ),
        c_agent(),
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(command: &str, running: bool) -> ModelRequest<'_> {
        let parts: Vec<&str> = command.split_whitespace().collect();
        // Tiny test-only leak keeps the borrowed exact-id variant simple.
        parse_model_request(Box::leak(parts.into_boxed_slice()), running)
    }

    #[test]
    fn bare_model_opens_catalog_only_when_idle() {
        assert_eq!(
            parse("/model", false),
            ModelRequest::Catalog {
                initial_query: String::new(),
                refresh: false,
            }
        );
        assert_eq!(parse("/model", true), ModelRequest::InspectBusy);
    }

    #[test]
    fn search_refresh_and_exact_id_are_distinct() {
        assert_eq!(
            parse("/model search claude opus", false),
            ModelRequest::Catalog {
                initial_query: "claude opus".into(),
                refresh: false,
            }
        );
        assert_eq!(
            parse("/model refresh", false),
            ModelRequest::Catalog {
                initial_query: String::new(),
                refresh: true,
            }
        );
        assert_eq!(
            parse("/model anthropic/claude-opus", false),
            ModelRequest::Switch("anthropic/claude-opus")
        );
    }
}
