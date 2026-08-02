use super::*;
use agent_core::model::{
    ModelRuntimeSource, ModelToolMode, MultiAgentVersion, ReasoningReplaySupport, ResponsesDialect,
    WebSearchToolType,
};

const RICH_FIXTURE: &str = include_str!("../../fixtures/models-2026-07-28.json");

fn retry() -> ModelRetryPolicy {
    ModelRetryPolicy {
        max_attempts: 4,
        backoff_base_ms: 50,
    }
}

#[test]
fn configured_and_remote_only_catalogs_keep_their_own_sources() {
    let descriptor = embedded::embedded_descriptors()
        .into_iter()
        .find(|descriptor| descriptor.tool_mode == ModelToolMode::Direct)
        .expect("direct embedded fixture");
    let slug = descriptor.slug.clone();
    let configured = ModelCatalog::from_static(vec![descriptor]).expect("static catalog");
    let configured_runtime = configured
        .resolve(&slug, None, 4096, retry())
        .expect("configured runtime");
    assert_eq!(configured_runtime.source, ModelRuntimeSource::Configured);

    let remote = ModelCatalog::remote_only();
    assert!(remote.models().is_empty());
    assert!(remote.resolve("gpt-5.5", None, 4096, retry()).is_err());
}

#[test]
fn remote_runtime_source_uses_the_scoped_models_endpoint() {
    let mut catalog = ModelCatalog::remote_only();
    let endpoint = "https://provider.example/v1/models";
    catalog
        .install_remote_scoped(
            RICH_FIXTURE,
            "2026-07-28",
            CatalogScope {
                provider: "configured".into(),
                endpoint: endpoint.into(),
                identity_fingerprint: "identity".into(),
            },
            Some("etag".into()),
        )
        .expect("remote catalog");
    let runtime = catalog
        .resolve("fixture-lite", None, 4096, retry())
        .expect("remote runtime");
    assert!(matches!(
        runtime.source,
        ModelRuntimeSource::Remote { endpoint: ref actual, .. } if actual == endpoint
    ));
}

#[test]
fn rich_fixture_preserves_every_runtime_field() {
    let mut catalog = ModelCatalog::embedded();
    let models = catalog
        .install_remote(RICH_FIXTURE, "2026-07-28")
        .expect("fixture parses");
    assert_eq!(
        models
            .iter()
            .map(|model| model.slug.as_str())
            .collect::<Vec<_>>(),
        ["fixture-lite", "gpt-5.5"]
    );
    let runtime = catalog
        .resolve(
            "fixture-lite",
            Some("high"),
            4096,
            ModelRetryPolicy {
                max_attempts: 4,
                backoff_base_ms: 50,
            },
        )
        .expect("runtime resolves");
    assert_eq!(runtime.instructions, "fixture base instructions");
    assert_eq!(runtime.context_window, 200_000);
    assert_eq!(runtime.auto_compact_token_limit, 170_000);
    assert_eq!(runtime.responses_dialect, ResponsesDialect::Lite);
    assert_eq!(runtime.tool_mode, ModelToolMode::Direct);
    assert_eq!(runtime.truncation.limit, 9_000);
    assert_eq!(runtime.comp_hash.as_deref(), Some("fixture-1"));
    assert!(runtime.supports_parallel_tool_calls);
    assert!(runtime.tool_capabilities.supports_search_tool);
    assert_eq!(
        runtime.tool_capabilities.web_search_tool_type,
        WebSearchToolType::TextAndImage
    );
    assert_eq!(
        runtime.tool_capabilities.experimental_supported_tools,
        ["tool_search"]
    );
    assert!(runtime.accepts_images());
    assert_eq!(runtime.reasoning_replay, ReasoningReplaySupport::Disabled);
    assert!(runtime.reasoning_replay_disabled_reason().is_some());
    let listed = models
        .iter()
        .find(|model| model.slug == "fixture-lite")
        .expect("listed fixture");
    assert_eq!(
        listed.metadata.description.as_deref(),
        Some("Complete fixture model")
    );
    assert_eq!(
        listed.metadata.default_service_tier.as_deref(),
        Some("priority")
    );
    assert_eq!(
        listed.metadata.service_tiers[0].name.as_deref(),
        Some("Priority")
    );
    assert_eq!(listed.metadata.max_context_window, Some(240_000));
    assert_eq!(listed.metadata.effective_context_window_percent, 87);
    assert_eq!(listed.metadata.input_modalities, ["text", "image"]);
    assert!(listed.metadata.supports_search_tool);
    assert!(listed.metadata.supports_image_detail_original);
    assert_eq!(
        listed
            .metadata
            .upgrade
            .as_ref()
            .and_then(|value| value["id"].as_str()),
        Some("fixture-next")
    );
    assert_eq!(
        listed.metadata.unrecognized_fields,
        ["future_catalog_capability"]
    );
    assert!(catalog.diagnostics().iter().any(|diagnostic| {
        diagnostic.contains("unrecognized catalog field future_catalog_capability")
    }));
}

#[test]
fn remote_replay_requires_both_encrypted_and_stateless_proofs() {
    let body = RICH_FIXTURE.replace(
        r#""supports_parallel_tool_calls": true,"#,
        r#""supports_parallel_tool_calls": true,
      "supports_encrypted_reasoning": true,
      "supports_reasoning_replay": true,"#,
    );
    let mut catalog = ModelCatalog::embedded();
    catalog.install_remote(&body, "2026-07-28").unwrap();
    let runtime = catalog
        .resolve(
            "fixture-lite",
            None,
            4096,
            ModelRetryPolicy {
                max_attempts: 4,
                backoff_base_ms: 50,
            },
        )
        .unwrap();
    assert_eq!(runtime.reasoning_replay, ReasoningReplaySupport::Enabled);
    assert!(runtime.reasoning_replay_disabled_reason().is_none());
}

#[test]
fn remote_complete_wins_and_partial_uses_whole_embedded_descriptor() {
    let mut catalog = ModelCatalog::embedded();
    catalog
        .install_remote(RICH_FIXTURE, "2026-07-28")
        .expect("fixture parses");
    let runtime = catalog
        .resolve(
            "gpt-5.5",
            None,
            4096,
            ModelRetryPolicy {
                max_attempts: 4,
                backoff_base_ms: 50,
            },
        )
        .expect("embedded fallback resolves");
    assert!(matches!(
        runtime.source,
        ModelRuntimeSource::Embedded { .. }
    ));
    assert_eq!(runtime.context_window, 272_000);
    assert!(
        catalog
            .diagnostics()
            .iter()
            .any(|diagnostic| diagnostic.contains("missing base instructions"))
    );
}

#[test]
fn null_auto_compact_limit_uses_the_reference_ninety_percent_rule() {
    let mut catalog = ModelCatalog::embedded();
    catalog
        .install_remote(
            &RICH_FIXTURE.replace(
                r#""auto_compact_token_limit": 170000"#,
                r#""auto_compact_token_limit": null"#,
            ),
            "2026-07-28",
        )
        .expect("current catalog null remains a complete descriptor");
    let runtime = catalog
        .resolve(
            "fixture-lite",
            None,
            4096,
            ModelRetryPolicy {
                max_attempts: 4,
                backoff_base_ms: 50,
            },
        )
        .expect("runtime resolves");
    assert_eq!(runtime.auto_compact_token_limit, 180_000);
}

#[test]
fn instruction_template_and_default_personality_override_base_instructions() {
    let body = RICH_FIXTURE.replace(
        r#""base_instructions": "fixture base instructions","#,
        r#""base_instructions": "fixture base instructions",
      "model_messages": {
        "instructions_template": "before {{ personality }} after",
        "instructions_variables": {
          "personality_default": "default voice"
        }
      },"#,
    );
    let mut catalog = ModelCatalog::embedded();
    catalog
        .install_remote(&body, "2026-07-28")
        .expect("templated descriptor parses");
    let runtime = catalog
        .resolve(
            "fixture-lite",
            None,
            4096,
            ModelRetryPolicy {
                max_attempts: 4,
                backoff_base_ms: 50,
            },
        )
        .expect("runtime resolves");
    assert_eq!(runtime.instructions, "before default voice after");
}

#[test]
fn malformed_or_empty_refresh_preserves_last_valid_catalog() {
    let mut catalog = ModelCatalog::embedded();
    catalog
        .install_remote(RICH_FIXTURE, "2026-07-28")
        .expect("fixture parses");
    let before = catalog.models();
    assert!(catalog.install_remote("{", "later").is_err());
    assert_eq!(catalog.models(), before);
    assert!(catalog.install_remote(r#"{"models":[]}"#, "later").is_err());
    assert_eq!(catalog.models(), before);
}

#[test]
fn stale_snapshot_is_retained_only_inside_the_same_scope() {
    let first = CatalogScope {
        provider: "openai_chatgpt".into(),
        endpoint: "https://chatgpt.com/backend-api/codex/models".into(),
        identity_fingerprint: "account-a".into(),
    };
    let second = CatalogScope {
        identity_fingerprint: "account-b".into(),
        ..first.clone()
    };
    let mut catalog = ModelCatalog::embedded();
    catalog
        .install_remote_scoped(
            RICH_FIXTURE,
            "2026-07-28",
            first.clone(),
            Some("etag-a".into()),
        )
        .expect("first scoped snapshot");
    assert_eq!(catalog.scope(), Some(&first));
    assert_eq!(catalog.etag(), Some("etag-a"));
    assert!(
        catalog
            .install_remote_scoped("{", "later", first, None)
            .is_err()
    );
    assert!(
        catalog
            .models()
            .iter()
            .any(|model| model.slug == "fixture-lite")
    );

    assert!(
        catalog
            .install_remote_scoped("{", "later", second, None)
            .is_err()
    );
    assert!(catalog.scope().is_none());
    assert!(
        !catalog
            .models()
            .iter()
            .any(|model| model.slug == "fixture-lite")
    );
}

#[test]
fn hidden_models_are_preserved_for_resolution_but_omitted_from_picker_order() {
    let body = RICH_FIXTURE.replace(r#""visibility": "list""#, r#""visibility": "hide""#);
    let mut catalog = ModelCatalog::embedded();
    let listed = catalog
        .install_remote(&body, "2026-07-28")
        .expect("hidden catalog parses");
    assert!(listed.is_empty());
    let hidden = catalog
        .model("fixture-lite")
        .expect("hidden model retained");
    assert_eq!(hidden.metadata.visibility.as_deref(), Some("hide"));
    assert!(
        catalog
            .resolve(
                "fixture-lite",
                None,
                4096,
                ModelRetryPolicy {
                    max_attempts: 1,
                    backoff_base_ms: 1,
                },
            )
            .is_ok()
    );
}

#[test]
fn invalid_effective_context_percent_is_explicitly_diagnosed() {
    let mut catalog = ModelCatalog::embedded();
    catalog
        .install_remote(
            &RICH_FIXTURE.replace(
                r#""effective_context_window_percent": 87"#,
                r#""effective_context_window_percent": 0"#,
            ),
            "2026-07-28",
        )
        .expect("snapshot shape remains valid");
    assert!(catalog.diagnostics().iter().any(|diagnostic| {
        diagnostic.contains("effective_context_window_percent must be between 1 and 100")
    }));
    assert!(
        catalog
            .resolve(
                "fixture-lite",
                None,
                4096,
                ModelRetryPolicy {
                    max_attempts: 1,
                    backoff_base_ms: 1
                },
            )
            .is_err()
    );
}

#[test]
fn unknown_tool_mode_is_explicitly_incompatible() {
    let mut catalog = ModelCatalog::embedded();
    catalog
        .install_remote(
            &RICH_FIXTURE.replace("\"direct\"", "\"future_mode\""),
            "2026-07-28",
        )
        .expect("catalog shape remains valid");
    let error = catalog
        .resolve(
            "fixture-lite",
            None,
            4096,
            ModelRetryPolicy {
                max_attempts: 4,
                backoff_base_ms: 50,
            },
        )
        .expect_err("unknown required mode must fail closed");
    assert!(error.to_string().contains("future_mode"));
}

#[test]
fn terminal_control_characters_in_catalog_identifiers_are_rejected() {
    let mut catalog = ModelCatalog::embedded();
    let error = catalog
        .install_remote(
            &RICH_FIXTURE.replace("fixture-lite", r"fixture-\u001b]52;evil"),
            "2026-07-28",
        )
        .expect_err("control characters must invalidate the snapshot");
    assert!(error.to_string().contains("control characters"));
}

#[test]
fn unknown_mandatory_capability_is_not_replaced_by_embedded_defaults() {
    let mut catalog = ModelCatalog::embedded();
    catalog
        .install_remote(
            &RICH_FIXTURE.replace(
                r#""input_modalities": ["text", "image"]"#,
                r#""input_modalities": ["text", "future_modality"]"#,
            ),
            "2026-07-28",
        )
        .expect("catalog shape remains valid");
    let error = catalog
        .resolve(
            "fixture-lite",
            None,
            4096,
            ModelRetryPolicy {
                max_attempts: 4,
                backoff_base_ms: 50,
            },
        )
        .expect_err("unknown mandatory capability must fail closed");
    assert!(error.to_string().contains("future_modality"));
}

#[test]
fn oversized_remote_instructions_are_incompatible_before_sampling() {
    let oversized = "x".repeat(agent_core::model::MAX_MODEL_INSTRUCTIONS_BYTES + 1);
    let mut catalog = ModelCatalog::embedded();
    catalog
        .install_remote(
            &RICH_FIXTURE.replace("fixture base instructions", &oversized),
            "2026-07-28",
        )
        .expect("catalog shape remains valid");
    let error = catalog
        .resolve(
            "fixture-lite",
            None,
            4096,
            ModelRetryPolicy {
                max_attempts: 4,
                backoff_base_ms: 50,
            },
        )
        .expect_err("oversized instructions must fail closed");
    assert!(error.to_string().contains("instructions exceed"));
}

/// Without a Code Mode runtime the behaviour is unchanged: the model stays
/// visible and is refused before any provider call, naming what is missing.
#[test]
fn a_code_mode_model_is_refused_when_no_runtime_is_wired() {
    let catalog = ModelCatalog::embedded();
    assert!(!catalog.code_mode());
    let listed = catalog
        .models()
        .into_iter()
        .find(|model| model.slug == "gpt-5.6-sol")
        .expect("embedded frontier model");
    assert!(
        listed
            .incompatibility_reason
            .as_deref()
            .is_some_and(|reason| reason.contains("Code Mode")),
        "{:?}",
        listed.incompatibility_reason
    );
    let error = catalog
        .resolve(
            "gpt-5.6-sol",
            None,
            4096,
            ModelRetryPolicy {
                max_attempts: 4,
                backoff_base_ms: 50,
            },
        )
        .expect_err("no runtime, no start");
    assert!(error.to_string().contains("Code Mode"), "{error}");
}

/// US-009 AC2: with a runtime wired, the same model resolves without any
/// local incompatibility and keeps its declared tool mode.
#[test]
fn a_code_mode_model_resolves_once_a_runtime_is_wired() {
    let mut catalog = ModelCatalog::embedded();
    catalog.set_code_mode(true);
    let listed = catalog
        .models()
        .into_iter()
        .find(|model| model.slug == "gpt-5.6-sol")
        .expect("embedded frontier model");
    assert_eq!(listed.incompatibility_reason, None);
    let runtime = catalog
        .resolve(
            "gpt-5.6-sol",
            None,
            4096,
            ModelRetryPolicy {
                max_attempts: 4,
                backoff_base_ms: 50,
            },
        )
        .expect("a wired runtime makes the model usable");
    assert_eq!(runtime.tool_mode, ModelToolMode::CodeModeOnly);
    runtime.validate().expect("the resolved runtime is valid");
}

/// A direct model is untouched by the flag in either position.
#[test]
fn a_direct_model_is_unaffected_by_the_code_mode_flag() {
    for available in [false, true] {
        let mut catalog = ModelCatalog::embedded();
        catalog.set_code_mode(available);
        let runtime = catalog
            .resolve(
                "gpt-5.5",
                None,
                4096,
                ModelRetryPolicy {
                    max_attempts: 4,
                    backoff_base_ms: 50,
                },
            )
            .expect("a direct model always resolves");
        assert_eq!(runtime.tool_mode, ModelToolMode::Direct);
    }
}

/// US-010 AC1: the three capabilities the frontier catalog carries survive
/// resolution together. A runtime that keeps the tool mode but drops the
/// orchestration version would compose a plan the model was not trained on.
#[test]
fn the_frontier_capabilities_survive_resolution_together() {
    let mut catalog = ModelCatalog::embedded();
    catalog.set_code_mode(true);
    let runtime = catalog
        .resolve(
            "gpt-5.6-sol",
            None,
            4096,
            ModelRetryPolicy {
                max_attempts: 4,
                backoff_base_ms: 50,
            },
        )
        .expect("the frontier model resolves");
    assert_eq!(runtime.tool_mode, ModelToolMode::CodeModeOnly);
    assert_eq!(runtime.multi_agent_version, MultiAgentVersion::V2);
    assert!(runtime.uses_responses_lite());
    assert!(runtime.multi_agent_version.drives_v2());
    runtime.validate().expect("the resolved runtime is valid");
}

/// The embedded catalog answers the same three values as the committed
/// baseline matrix, per slug. `luna` is the case that matters: it is a code
/// mode model on v1, so "code mode" and "v2" must not be read as one
/// capability.
#[test]
fn the_embedded_catalog_matches_the_baseline_rows() {
    let mut catalog = ModelCatalog::embedded();
    catalog.set_code_mode(true);
    for (slug, tool_mode, version, lite) in [
        (
            "gpt-5.6-sol",
            ModelToolMode::CodeModeOnly,
            MultiAgentVersion::V2,
            true,
        ),
        (
            "gpt-5.6-terra",
            ModelToolMode::CodeModeOnly,
            MultiAgentVersion::V2,
            true,
        ),
        (
            "gpt-5.6-luna",
            ModelToolMode::CodeModeOnly,
            MultiAgentVersion::V1,
            true,
        ),
        (
            "gpt-5.5",
            ModelToolMode::Direct,
            MultiAgentVersion::Disabled,
            false,
        ),
        (
            "gpt-5.4",
            ModelToolMode::Direct,
            MultiAgentVersion::Disabled,
            false,
        ),
    ] {
        assert_eq!(catalog.tool_mode(slug), Some(tool_mode), "{slug}");
        assert_eq!(catalog.multi_agent_version(slug), Some(version), "{slug}");
        let runtime = catalog
            .resolve(
                slug,
                None,
                4096,
                ModelRetryPolicy {
                    max_attempts: 4,
                    backoff_base_ms: 50,
                },
            )
            .expect("the embedded catalog resolves every slug it lists");
        assert_eq!(runtime.uses_responses_lite(), lite, "{slug}");
    }
}

/// US-010 AC3: an unknown orchestration version is refused with the faulty
/// field, never degraded to `disabled`. Silently dropping it would make a
/// frontier model run without the tools it expects and look merely lazy.
#[test]
fn an_unknown_multi_agent_version_is_incompatible_and_names_the_field() {
    let mut catalog = ModelCatalog::embedded();
    catalog
        .install_remote(
            &RICH_FIXTURE.replace(
                r#""use_responses_lite": true,"#,
                r#""use_responses_lite": true,
      "multi_agent_version": "v3","#,
            ),
            "2026-07-28",
        )
        .expect("catalog shape remains valid");
    let error = catalog
        .resolve(
            "fixture-lite",
            None,
            4096,
            ModelRetryPolicy {
                max_attempts: 4,
                backoff_base_ms: 50,
            },
        )
        .expect_err("an unknown required capability must fail closed");
    let rendered = error.to_string();
    assert!(rendered.contains("multi_agent_version"), "{rendered}");
    assert!(rendered.contains("v3"), "{rendered}");
}

/// US-010 AC4: a remote entry that says nothing about orchestration stays
/// `disabled`, so a historical direct model keeps the exact contract the
/// earlier fixtures pinned.
#[test]
fn a_remote_entry_without_the_field_stays_disabled() {
    let mut catalog = ModelCatalog::embedded();
    catalog
        .install_remote(RICH_FIXTURE, "2026-07-28")
        .expect("fixture parses");
    assert_eq!(
        catalog.multi_agent_version("fixture-lite"),
        Some(MultiAgentVersion::Disabled)
    );
    let runtime = catalog
        .resolve(
            "fixture-lite",
            None,
            4096,
            ModelRetryPolicy {
                max_attempts: 4,
                backoff_base_ms: 50,
            },
        )
        .expect("runtime resolves");
    assert_eq!(runtime.multi_agent_version, MultiAgentVersion::Disabled);
    assert_eq!(runtime.tool_mode, ModelToolMode::Direct);
}

#[test]
fn fingerprints_match_for_identical_interactive_and_headless_sources() {
    let catalog = ModelCatalog::embedded();
    let resolve = || {
        catalog
            .resolve(
                "gpt-5.5",
                Some("high"),
                4096,
                ModelRetryPolicy {
                    max_attempts: 4,
                    backoff_base_ms: 50,
                },
            )
            .expect("runtime resolves")
    };
    assert_eq!(resolve().fingerprint, resolve().fingerprint);
}
