//! The model-experience gate, over its fixtures and over this repository.
//!
//! `panic!` is the reporting mechanism of these tests: a violation is a line of
//! prose, and printing the lines the gate produced is what makes a failure
//! actionable without a second run.
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use agent_doc_gates::{
    CLASSIFICATION, CONTRACT_DOC, Classified, INDIRECT_OPENING, NONE_OPENING, SECTION_HEADING,
    Shape, TOOL_CATALOG_DOC, catalog_fragments, check_catalog_anchors, check_classification,
    check_model_experience, check_readme, classification_of, repository_root,
};

/// The sixteen names, as `crate_directories` would hand them over.
fn directories() -> Vec<String> {
    CLASSIFICATION
        .iter()
        .map(|entry| entry.name.to_string())
        .collect()
}

/// A catalog holding the two fragments the fixtures link to.
fn fragments() -> Vec<String> {
    vec!["read".to_string(), "shell".to_string()]
}

fn structured(name: &'static str) -> Classified {
    Classified {
        name,
        shape: Shape::Structured,
        justification: "",
    }
}

fn short(name: &'static str, shape: Shape) -> Classified {
    Classified {
        name,
        shape,
        justification: "une raison écrite en toutes lettres",
    }
}

fn assert_none(violations: &[String]) {
    if !violations.is_empty() {
        panic!(
            "aucune violation attendue, obtenu :\n{}",
            violations.join("\n")
        );
    }
}

fn assert_mentions(violations: &[String], needle: &str) {
    if !violations.iter().any(|line| line.contains(needle)) {
        panic!("« {needle} » attendu dans :\n{}", violations.join("\n"));
    }
}

// US-108: the classification is exhaustive, ordered and confronted with the disk.

#[test]
fn the_classification_covers_the_sixteen_crates_of_this_workspace() {
    assert_none(&check_classification(CLASSIFICATION, &directories()));
}

#[test]
fn the_classification_is_ordered_by_crate_name_so_an_insertion_reads_at_its_place() {
    let names: Vec<&str> = CLASSIFICATION.iter().map(|entry| entry.name).collect();
    let mut sorted = names.clone();
    sorted.sort_unstable();
    assert_eq!(names, sorted, "la table doit rester triée par nom de crate");
}

#[test]
fn the_classification_names_each_crate_once() {
    let mut names: Vec<&str> = CLASSIFICATION.iter().map(|entry| entry.name).collect();
    let total = names.len();
    names.sort_unstable();
    names.dedup();
    assert_eq!(
        names.len(),
        total,
        "un crate ne peut porter deux classements"
    );
}

#[test]
fn a_crate_present_on_disk_and_absent_from_the_table_fails_the_gate() {
    let mut on_disk = directories();
    on_disk.push("agent-nouveau".to_string());
    on_disk.sort();
    let violations = check_classification(CLASSIFICATION, &on_disk);
    assert_mentions(&violations, "`agent-nouveau` n'est pas classé");
    assert_mentions(&violations, CONTRACT_DOC);
}

#[test]
fn an_entry_of_the_table_matching_no_crate_fails_the_gate() {
    let on_disk: Vec<String> = directories()
        .into_iter()
        .filter(|name| name != "agent-tokenizer")
        .collect();
    let violations = check_classification(CLASSIFICATION, &on_disk);
    assert_mentions(
        &violations,
        "l'entrée `agent-tokenizer` ne correspond à aucun crate",
    );
}

#[test]
fn a_non_structured_entry_without_a_readable_motive_fails_the_gate() {
    for justification in ["", "indirect"] {
        let table = [Classified {
            name: "agent-fixture",
            shape: Shape::ShortIndirect,
            justification,
        }];
        let violations = check_classification(&table, &["agent-fixture".to_string()]);
        assert_mentions(&violations, "sans motif lisible");
        assert_mentions(&violations, "un oubli déguisé");
    }
}

#[test]
fn a_structured_entry_owes_no_motive_because_its_section_is_the_motive() {
    let table = [structured("agent-fixture")];
    assert_none(&check_classification(
        &table,
        &["agent-fixture".to_string()],
    ));
}

#[test]
fn every_non_structured_entry_of_the_shipped_table_carries_a_readable_motive() {
    assert!(
        CLASSIFICATION
            .iter()
            .filter(|entry| entry.shape != Shape::Structured)
            .all(|entry| entry.justification.split_whitespace().count() >= 2),
        "chaque classement non structuré porte un motif lisible"
    );
}

#[test]
fn an_empty_crates_directory_fails_the_gate_rather_than_passing_it_silently() {
    let violations = check_classification(CLASSIFICATION, &[]);
    assert_mentions(&violations, "est vide ou illisible");
    assert_mentions(
        &violations,
        "une classification vide n'est pas une classification",
    );
}

// US-109: the structured form, its order and its density.

const SURFACE: &str = "\
## Model Experience

### Tool descriptions

#### What the model sees

The `read` tool, rendered in [`docs/tool-catalog.md`](../../docs/tool-catalog.md#read).

#### Token effect

Around 3 000 tokens for the whole catalog.

#### KV Cache effect

Croissance en ajout seul: n'invalide rien tant qu'aucune description ne bouge.
";

#[test]
fn a_conforming_structured_section_passes_the_gate() {
    assert_none(&check_readme(
        &structured("agent-fixture"),
        Some(SURFACE),
        &fragments(),
    ));
}

#[test]
fn a_structured_section_without_a_surface_fails_the_gate() {
    let readme = format!("{SECTION_HEADING}\n\nUne phrase et rien d'autre.\n");
    let violations = check_readme(&structured("agent-fixture"), Some(&readme), &fragments());
    assert_mentions(&violations, "ne porte aucun H3");
}

#[test]
fn the_three_fields_out_of_order_fail_the_gate_naming_the_expected_order() {
    let readme = SURFACE.replace(
        "#### Token effect\n\nAround 3 000 tokens for the whole catalog.\n\n#### KV Cache effect\n\nCroissance en ajout seul: n'invalide rien tant qu'aucune description ne bouge.\n",
        "#### KV Cache effect\n\nCroissance en ajout seul.\n\n#### Token effect\n\nAround 3 000 tokens.\n",
    );
    let violations = check_readme(&structured("agent-fixture"), Some(&readme), &fragments());
    assert_mentions(&violations, "ordre attendu");
}

#[test]
fn an_unknown_field_under_a_surface_fails_the_gate() {
    let readme = format!("{SURFACE}\n#### Notes\n\nUn quatrième champ.\n");
    let violations = check_readme(&structured("agent-fixture"), Some(&readme), &fragments());
    assert_mentions(&violations, "champ inconnu");
    assert_mentions(&violations, "l'ensemble des champs est fermé");
}

#[test]
fn a_field_holding_two_paragraphs_fails_the_gate_on_density() {
    let readme = SURFACE.replace(
        "Around 3 000 tokens for the whole catalog.\n",
        "Around 3 000 tokens for the whole catalog.\n\nEt un second paragraphe.\n",
    );
    let violations = check_readme(&structured("agent-fixture"), Some(&readme), &fragments());
    assert_mentions(&violations, "2 paragraphe(s), exactement un est attendu");
}

#[test]
fn an_empty_field_fails_the_gate_on_the_same_rule() {
    let readme = SURFACE.replace("Around 3 000 tokens for the whole catalog.\n", "");
    let violations = check_readme(&structured("agent-fixture"), Some(&readme), &fragments());
    assert_mentions(&violations, "0 paragraphe(s), exactement un est attendu");
}

#[test]
fn a_first_field_carrying_only_a_paraphrase_fails_for_lack_of_an_anchor() {
    let readme = SURFACE.replace(
        "The `read` tool, rendered in [`docs/tool-catalog.md`](../../docs/tool-catalog.md#read).",
        "Les descriptions des outils, telles que le catalogue les rend.",
    );
    let violations = check_readme(&structured("agent-fixture"), Some(&readme), &fragments());
    assert_mentions(
        &violations,
        "ni code inline, ni bloc imbriqué, ni lien ancré",
    );
}

#[test]
fn a_nested_block_anchors_a_field_as_well_as_inline_code_does() {
    let readme = SURFACE.replace(
        "The `read` tool, rendered in [`docs/tool-catalog.md`](../../docs/tool-catalog.md#read).",
        "Le texte envoyé, cité tel quel :\n\n```text\nRead a file.\n```",
    );
    assert_none(&check_readme(
        &structured("agent-fixture"),
        Some(&readme),
        &fragments(),
    ));
}

#[test]
fn a_system_prompt_surface_that_only_describes_its_text_fails_the_gate() {
    let readme = SURFACE.replace("### Tool descriptions", "### The system prompt");
    let violations = check_readme(&structured("agent-fixture"), Some(&readme), &fragments());
    assert_mentions(&violations, "le texte système se cite, il ne se décrit pas");
}

#[test]
fn a_system_prompt_surface_quoting_its_text_under_a_titled_heading_passes() {
    let readme = SURFACE
        .replace("### Tool descriptions", "### The system prompt")
        .replace(
            "The `read` tool, rendered in [`docs/tool-catalog.md`](../../docs/tool-catalog.md#read).",
            "Le texte ajouté, cité depuis `crates/agent-cli/src/prompt.rs` :\n\n##### `HARNESS`\n\n```markdown\n# Pyxis harness contract\n```",
        );
    assert_none(&check_readme(
        &structured("agent-fixture"),
        Some(&readme),
        &fragments(),
    ));
}

#[test]
fn several_defects_of_one_readme_are_all_reported_in_a_single_run() {
    let readme = SURFACE
        .replace(
            "The `read` tool, rendered in [`docs/tool-catalog.md`](../../docs/tool-catalog.md#read).",
            "Les descriptions des outils, telles que le catalogue les rend.",
        )
        .replace(
            "Around 3 000 tokens for the whole catalog.\n",
            "Around 3 000 tokens.\n\nEt un second paragraphe.\n",
        );
    let violations = check_readme(&structured("agent-fixture"), Some(&readme), &fragments());
    assert_mentions(
        &violations,
        "ni code inline, ni bloc imbriqué, ni lien ancré",
    );
    assert_mentions(&violations, "2 paragraphe(s), exactement un est attendu");
    assert!(
        violations.len() >= 2,
        "les deux défauts sont rendus en une fois, obtenu :\n{}",
        violations.join("\n")
    );
}

#[test]
fn a_readme_declaring_the_section_twice_fails_the_gate() {
    let readme = format!("{SURFACE}\n{SURFACE}");
    let violations = check_readme(&structured("agent-fixture"), Some(&readme), &fragments());
    assert_mentions(&violations, "porte 2 sections");
}

#[test]
fn a_structured_crate_without_a_readme_fails_the_gate() {
    let violations = check_readme(&structured("agent-fixture"), None, &fragments());
    assert_mentions(&violations, "est attendu et absent");
    assert_mentions(&violations, "structurée");
}

#[test]
fn a_readme_without_the_section_fails_the_gate_for_a_structured_crate() {
    let violations = check_readme(
        &structured("agent-fixture"),
        Some("# agent-fixture\n\nUn crate.\n"),
        &fragments(),
    );
    assert_mentions(&violations, "ne porte aucune section");
}

#[test]
fn a_quoted_section_heading_inside_a_fenced_block_does_not_open_a_section() {
    let readme = format!("# agent-fixture\n\n```markdown\n{SECTION_HEADING}\n```\n");
    let violations = check_readme(&structured("agent-fixture"), Some(&readme), &fragments());
    assert_mentions(&violations, "ne porte aucune section");
}

#[test]
fn a_second_level_heading_after_the_section_closes_it() {
    let readme = format!("{SURFACE}\n## Autre chose\n\n### Un titre étranger\n\nDu texte.\n");
    assert_none(&check_readme(
        &structured("agent-fixture"),
        Some(&readme),
        &fragments(),
    ));
}

// US-110: the short form is bounded and the omission stays fileless.

const SHORT: &str = "\
## Model Experience

Indirectly, through the `shell` tool result that carries the refusal body.

#### KV Cache effect

Remplacement de tokens antérieurs: un refus injecté au milieu d'un tour invalide
la suite du préfixe.
";

#[test]
fn a_conforming_short_section_passes_the_gate() {
    assert_none(&check_readme(
        &short("agent-fixture", Shape::ShortIndirect),
        Some(SHORT),
        &fragments(),
    ));
}

#[test]
fn a_short_section_carrying_a_surface_heading_fails_the_gate() {
    let readme = SHORT.replace(
        "#### KV Cache effect",
        "### Une surface\n\n#### KV Cache effect",
    );
    let violations = check_readme(
        &short("agent-fixture", Shape::ShortIndirect),
        Some(&readme),
        &fragments(),
    );
    assert_mentions(&violations, "un H3 y est interdit");
}

#[test]
fn a_short_section_carrying_a_second_field_fails_the_gate() {
    let readme = SHORT.replace(
        "#### KV Cache effect",
        "#### Token effect\n\nAucun.\n\n#### KV Cache effect",
    );
    let violations = check_readme(
        &short("agent-fixture", Shape::ShortIndirect),
        Some(&readme),
        &fragments(),
    );
    assert_mentions(&violations, "`#### Token effect` y est interdit");
}

#[test]
fn a_short_section_without_its_cache_field_fails_the_gate() {
    let readme = "## Model Experience\n\nIndirectly, through the `shell` tool result.\n";
    let violations = check_readme(
        &short("agent-fixture", Shape::ShortIndirect),
        Some(readme),
        &fragments(),
    );
    assert_mentions(&violations, "sans `#### KV Cache effect`");
}

#[test]
fn a_short_section_opening_on_an_unknown_formula_fails_the_gate_naming_both() {
    let readme = SHORT.replace(
        "Indirectly, through the `shell` tool result that carries the refusal body.",
        "Ce crate ne parle pas vraiment au modèle.",
    );
    let violations = check_readme(
        &short("agent-fixture", Shape::ShortIndirect),
        Some(&readme),
        &fragments(),
    );
    assert_mentions(&violations, "amorce inconnue");
    assert_mentions(&violations, NONE_OPENING);
    assert_mentions(&violations, INDIRECT_OPENING);
}

#[test]
fn a_table_and_a_readme_that_contradict_each_other_fail_the_gate() {
    let violations = check_readme(
        &short("agent-fixture", Shape::ShortNone),
        Some(SHORT),
        &fragments(),
    );
    assert_mentions(&violations, "les deux se contredisent");
}

#[test]
fn a_classification_sentence_running_over_two_paragraphs_fails_the_gate() {
    let readme = SHORT.replace(
        "Indirectly, through the `shell` tool result that carries the refusal body.",
        "Indirectly, through the `shell` tool result.\n\nEt un développement de plus.",
    );
    let violations = check_readme(
        &short("agent-fixture", Shape::ShortIndirect),
        Some(&readme),
        &fragments(),
    );
    assert_mentions(&violations, "paragraphe(s) de classification");
}

#[test]
fn an_omitted_crate_carrying_a_section_fails_the_gate() {
    let entry = short("agent-fixture", Shape::Omitted);
    let violations = check_readme(&entry, Some(SHORT), &fragments());
    assert_mentions(
        &violations,
        "est classé en omission et porte pourtant une section",
    );
    assert_mentions(&violations, "la table tranche");
}

#[test]
fn an_omitted_crate_without_a_readme_passes_the_gate() {
    assert_none(&check_readme(
        &short("agent-fixture", Shape::Omitted),
        None,
        &fragments(),
    ));
}

#[test]
fn an_omitted_crate_with_a_readme_that_says_nothing_of_the_model_passes() {
    assert_none(&check_readme(
        &short("agent-fixture", Shape::Omitted),
        Some("# agent-fixture\n\nUn crate de vérification.\n"),
        &fragments(),
    ));
}

// US-111: the catalog anchors resolve against the generated catalog.

#[test]
fn the_harvest_reads_the_headings_of_the_generated_catalog() {
    let catalog =
        "# Catalogue\n\n## Outils\n\n### `read`\n\nDu texte.\n\n### `apply_patch`\n\nDu texte.\n";
    let fragments = catalog_fragments(catalog).expect("la récolte aboutit");
    assert_eq!(
        fragments,
        vec!["apply_patch".to_string(), "read".to_string()]
    );
}

#[test]
fn the_harvest_ignores_a_heading_quoted_inside_a_fenced_block() {
    let catalog = "### `read`\n\n```markdown\n### `pas_un_outil`\n```\n";
    let fragments = catalog_fragments(catalog).expect("la récolte aboutit");
    assert_eq!(fragments, vec!["read".to_string()]);
}

#[test]
fn a_catalog_rendering_no_section_fails_instead_of_accepting_every_link() {
    let errors = catalog_fragments("# Catalogue\n\nRien.\n").expect_err("la récolte échoue");
    assert_mentions(&errors, "ne rend aucune section");
}

#[test]
fn a_tool_name_outside_the_expected_set_fails_rather_than_producing_a_wrong_link() {
    let errors = catalog_fragments("### `outil étrange`\n").expect_err("la récolte échoue");
    assert_mentions(&errors, "sort du jeu attendu");
}

#[test]
fn a_link_aiming_at_a_missing_fragment_fails_the_gate_with_its_count() {
    let content = "Voir [`grep`](../../docs/tool-catalog.md#grep).\n";
    let violations = check_catalog_anchors("agent-fixture", content, &fragments());
    assert_mentions(&violations, "`#grep` n'existe pas parmi les 2 sections");
    assert_mentions(&violations, "`agent-fixture:1`");
}

#[test]
fn a_link_aiming_at_a_live_fragment_passes_the_gate() {
    let content = "Voir [`read`](../../docs/tool-catalog.md#read).\n";
    assert_none(&check_catalog_anchors(
        "agent-fixture",
        content,
        &fragments(),
    ));
}

#[test]
fn a_link_without_a_fragment_is_left_to_the_link_gate() {
    let content = "Voir [le catalogue](../../docs/tool-catalog.md).\n";
    assert_none(&check_catalog_anchors(
        "agent-fixture",
        content,
        &fragments(),
    ));
}

#[test]
fn a_fragment_quoted_inside_a_fenced_block_is_an_example_not_a_link() {
    let content = "```markdown\n[`grep`](../../docs/tool-catalog.md#grep)\n```\n";
    assert_none(&check_catalog_anchors(
        "agent-fixture",
        content,
        &fragments(),
    ));
}

#[test]
fn the_link_gate_now_reads_the_crate_readmes_alongside_the_documentation_tree() {
    let documents = agent_doc_gates::markdown_documents(&repository_root());
    let root = repository_root();
    for entry in CLASSIFICATION
        .iter()
        .filter(|entry| entry.shape != Shape::Omitted)
    {
        let readme = root.join("crates").join(entry.name).join("README.md");
        assert!(
            documents.contains(&readme),
            "{} doit entrer dans la porte des liens",
            readme.display()
        );
    }
}

// The gate, over this repository.

#[test]
fn this_repository_declares_what_each_of_its_crates_sends_to_the_model() {
    let violations = check_model_experience(&repository_root());
    if !violations.is_empty() {
        panic!(
            "le dépôt viole son propre contrat d'expérience du modèle :\n{}\n\nvoir {CONTRACT_DOC}",
            violations.join("\n")
        );
    }
}

#[test]
fn the_normative_document_is_present_and_names_the_two_short_openings() {
    let path = repository_root().join(CONTRACT_DOC);
    let content = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("{} est lisible : {error}", path.display()));
    for needle in [
        SECTION_HEADING,
        NONE_OPENING,
        INDIRECT_OPENING,
        TOOL_CATALOG_DOC,
    ] {
        assert!(
            content.contains(needle),
            "{CONTRACT_DOC} doit écrire « {needle} »"
        );
    }
}

#[test]
fn every_classified_crate_is_reachable_by_name() {
    for entry in CLASSIFICATION {
        assert_eq!(
            classification_of(entry.name).map(|found| found.shape),
            Some(entry.shape)
        );
    }
    assert!(classification_of("agent-inexistant").is_none());
}
