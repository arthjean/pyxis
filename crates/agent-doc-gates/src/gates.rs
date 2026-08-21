//! The non-drift gate: the `justfile` and `.github/workflows/ci.yml` carry the
//! same `cargo` invocations, in the same order, and the prescriptive documents
//! name the recipes instead of writing a third formulation of their own.
//!
//! The repository keeps two inventories of its own gates on purpose. The
//! workflow keeps its steps verbatim, with their per-step `timeout`, their
//! streaming filter and their step summary, because a job cancelled by
//! `timeout-minutes` archives no log; wrapping them in `just check` would trade
//! those diagnostics for one line. The recipe file exists so a contributor and an
//! agent reach the same gates without reading YAML. Two inventories that nothing
//! compares is how `CONTRIBUTING.md` came to prescribe `cargo clippy --no-deps`
//! against the workflow's `--all-targets`, so the duplication is paid for here:
//! the aggregate cannot lie about what the CI runs without failing
//! `cargo test --workspace`.
//!
//! Pairing goes through an explicit marker carried above the recipe rather than
//! through the recipe name, because workflow step names hold spaces (`Build
//! tests`) and deriving one from the other would force the YAML into
//! identifiers.
//!
//! Both parsers are written by hand over a narrow, documented subset, the way
//! `agent-parity`'s `offline_suite.rs` reads a markdown table: a YAML crate would
//! buy robustness on input this repository's own workflow never produces, at the
//! price of the dependency this crate forbids. The subset is: one `cargo` token
//! per logical line, backslash continuations joined, `timeout` and its options
//! removed, everything from the first shell operator dropped. Anything else
//! wrapping `cargo` fails loudly and names itself, because a gate that silently
//! skips what it does not understand is not a gate.
//!
//! The prose half holds `AGENTS.md` and `CONTRIBUTING.md` to those same
//! invocations. The divergence it forbids is the one that actually happened:
//! `CONTRIBUTING.md` prescribed `cargo clippy --workspace --no-deps` while the
//! workflow ran `--all-targets`, so a contributor could run the gate green and
//! still be refused, `--no-deps` never compiling the test targets. A document is
//! therefore refused an invocation that shares a gate's head and diverges from
//! it, and the message says which recipe to write instead.

use std::fs;
use std::path::Path;

/// The recipe file, relative to the repository root.
pub const JUSTFILE: &str = "justfile";

/// The workflow, relative to the repository root.
pub const WORKFLOW: &str = ".github/workflows/ci.yml";

/// The comment pairing a recipe with a workflow step, compared literally. It
/// sits ABOVE the recipe's documentation comment, so `just --list` keeps showing
/// the documentation line and not this one.
pub const GATE_MARKER: &str = "# ci-step:";

/// The recipe composing every marked gate. Its dependency list is the execution
/// order, so it is held to the order of the workflow too.
pub const AGGREGATE_RECIPE: &str = "check";

/// The only command either inventory is allowed to name as a gate.
const CARGO: &str = "cargo";

/// The one wrapper accepted in front of `cargo`, removed before comparison. The
/// workflow bounds its two expensive steps from the inside, because reaching the
/// job-level cap cancels the job and a cancelled job archives no log.
const ACCEPTED_WRAPPER: &str = "timeout";

/// One gate, as read from either file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Gate {
    /// The workflow step name: the marker value on the recipe side, the `name:`
    /// on the workflow side. This is the pairing key.
    pub step: String,
    /// The `cargo` invocation, wrappers removed and shell plumbing dropped.
    pub argv: Vec<String>,
}

impl Gate {
    /// The invocation as a reader would type it, for the failure messages.
    fn command(&self) -> String {
        self.argv.join(" ")
    }
}

/// Read both files and report every divergence between the two inventories.
pub fn check_gates(repository_root: &Path) -> Vec<String> {
    let justfile = match fs::read_to_string(repository_root.join(JUSTFILE)) {
        Ok(content) => content,
        Err(error) => return vec![format!("gates: {JUSTFILE} : illisible ({error})")],
    };
    let workflow = match fs::read_to_string(repository_root.join(WORKFLOW)) {
        Ok(content) => content,
        Err(error) => return vec![format!("gates: {WORKFLOW} : illisible ({error})")],
    };
    check_gate_documents(&justfile, &workflow)
}

/// The same check over two documents held in memory, so a fixture can reproduce
/// a divergence without touching either real file.
pub fn check_gate_documents(justfile: &str, workflow: &str) -> Vec<String> {
    let recipes = parse_recipes(justfile);
    let mut violations = aggregate_violations(&recipes);
    match (gates_of_recipes(&recipes), workflow_gates(workflow)) {
        (Ok(left), Ok(right)) => violations.extend(compare_gates(&left, &right)),
        (left, right) => {
            // An inventory that failed to parse has no list to compare, so the
            // extraction errors are the whole report: a positional diff against
            // an empty side would bury them under noise.
            if let Err(errors) = left {
                violations.extend(errors);
            }
            if let Err(errors) = right {
                violations.extend(errors);
            }
        }
    }
    violations
}

/// The ordered gates of the recipe file: one per recipe carrying a marker.
pub fn justfile_gates(content: &str) -> Result<Vec<Gate>, Vec<String>> {
    gates_of_recipes(&parse_recipes(content))
}

/// The ordered gates of the workflow: one per step running exactly one `cargo`
/// command. A step that runs none is not a gate and is skipped in silence, which
/// is what keeps the system-dependency install and the failure report out of the
/// comparison.
pub fn workflow_gates(content: &str) -> Result<Vec<Gate>, Vec<String>> {
    let mut gates = Vec::new();
    let mut errors = Vec::new();
    for step in parse_steps(content) {
        let named = if step.name.is_empty() {
            "<sans nom>".to_string()
        } else {
            step.name.clone()
        };
        let mut found = Vec::new();
        for line in logical_lines(&step.run) {
            match cargo_invocations(&line) {
                Ok(argvs) => found.extend(argvs),
                Err(reason) => {
                    errors.push(format!("gates: {WORKFLOW} : étape « {named} » : {reason}"));
                }
            }
        }
        match found.len() {
            0 => {}
            1 => {
                let Some(argv) = found.into_iter().next() else {
                    continue;
                };
                if step.name.is_empty() {
                    errors.push(format!(
                        "gates: {WORKFLOW} : une étape sans « name: » exécute « {} » ; l'appariement passe par le nom d'étape",
                        argv.join(" ")
                    ));
                    continue;
                }
                gates.push(Gate {
                    step: step.name,
                    argv,
                });
            }
            count => errors.push(format!(
                "gates: {WORKFLOW} : étape « {named} » : {count} invocations cargo, une porte en compte exactement une"
            )),
        }
    }
    if errors.is_empty() {
        Ok(gates)
    } else {
        Err(errors)
    }
}

/// Compare the two inventories position by position and report every
/// divergence. Stopping at the first would turn one reordering into as many
/// round trips as there are gates.
pub fn compare_gates(justfile: &[Gate], workflow: &[Gate]) -> Vec<String> {
    let mut violations = Vec::new();
    for gate in justfile {
        if !workflow.iter().any(|step| step.step == gate.step) {
            violations.push(format!(
                "gates: {JUSTFILE} : le marqueur « {GATE_MARKER} {} » est orphelin, aucune étape de {WORKFLOW} ne porte ce nom",
                gate.step
            ));
        }
    }
    for index in 0..justfile.len().max(workflow.len()) {
        match (justfile.get(index), workflow.get(index)) {
            (Some(left), Some(right)) if left.step != right.step => violations.push(format!(
                "gates: porte n°{} : l'ordre diverge, {WORKFLOW} exécute « {} » et {JUSTFILE} « {} » ; le CI fait foi",
                index + 1,
                right.step,
                left.step
            )),
            (Some(left), Some(right)) if left.argv != right.argv => violations.push(format!(
                "gates: étape « {} » : {WORKFLOW} exécute « {} », {JUSTFILE} exécute « {} »",
                right.step,
                right.command(),
                left.command()
            )),
            (Some(_), Some(_)) => {}
            (None, Some(right)) => violations.push(format!(
                "gates: étape « {} » absente de {JUSTFILE} : ajouter une recette précédée de « {GATE_MARKER} {} » et exécutant « {} »",
                right.step,
                right.step,
                right.command()
            )),
            (Some(left), None) => violations.push(format!(
                "gates: {JUSTFILE} : la recette marquée « {} » exécute « {} » qu'aucune étape de {WORKFLOW} n'exécute",
                left.step,
                left.command()
            )),
            (None, None) => {}
        }
    }
    violations
}

/// One recipe of the file, reduced to what the gate reads.
struct Recipe {
    name: String,
    dependencies: Vec<String>,
    marker: Option<String>,
    body: Vec<String>,
}

/// The aggregate composes the marked recipes in the order they are written, and
/// the order they are written is the order compared against the workflow. Checked
/// on its own because reordering the dependency list alone changes what
/// `just check` runs first while leaving both inventories identical.
fn aggregate_violations(recipes: &[Recipe]) -> Vec<String> {
    let marked: Vec<String> = recipes
        .iter()
        .filter(|recipe| recipe.marker.is_some())
        .map(|recipe| recipe.name.clone())
        .collect();
    let Some(aggregate) = recipes
        .iter()
        .find(|recipe| recipe.name == AGGREGATE_RECIPE)
    else {
        return vec![format!(
            "gates: {JUSTFILE} : aucune recette « {AGGREGATE_RECIPE} », l'agrégat des portes du CI"
        )];
    };
    if aggregate.dependencies == marked {
        return Vec::new();
    }
    vec![format!(
        "gates: {JUSTFILE} : « {AGGREGATE_RECIPE} » compose « {} » alors que les recettes marquées se lisent « {} » ; l'ordre d'exécution est celui du CI",
        aggregate.dependencies.join(" "),
        marked.join(" ")
    )]
}

fn gates_of_recipes(recipes: &[Recipe]) -> Result<Vec<Gate>, Vec<String>> {
    gates_with_recipes(recipes).map(|gates| gates.into_iter().map(|(_, gate)| gate).collect())
}

/// The same gates, each kept next to the recipe that carries it, which is what a
/// prose document is told to write instead of the invocation.
fn gates_with_recipes(recipes: &[Recipe]) -> Result<Vec<(String, Gate)>, Vec<String>> {
    let mut gates = Vec::new();
    let mut errors = Vec::new();
    for recipe in recipes {
        let Some(step) = recipe.marker.as_ref() else {
            continue;
        };
        let name = &recipe.name;
        if step.is_empty() {
            errors.push(format!(
                "gates: {JUSTFILE} : recette « {name} » : « {GATE_MARKER} » sans nom d'étape"
            ));
            continue;
        }
        let mut found = Vec::new();
        for line in &recipe.body {
            if line.starts_with('#') {
                continue;
            }
            let (command, ignores_failure) = strip_sigils(line);
            match cargo_invocations(command) {
                Ok(argvs) => {
                    for argv in argvs {
                        if ignores_failure {
                            errors.push(format!(
                                "gates: {JUSTFILE} : recette « {name} » : le sigil « - » rend « {} » non bloquant alors qu'elle porte un marqueur ; une étape du CI ne peut pas échouer en silence",
                                argv.join(" ")
                            ));
                        }
                        found.push(argv);
                    }
                }
                Err(reason) => {
                    errors.push(format!("gates: {JUSTFILE} : recette « {name} » : {reason}"));
                }
            }
        }
        match found.len() {
            1 => {
                let Some(argv) = found.into_iter().next() else {
                    continue;
                };
                gates.push((
                    name.clone(),
                    Gate {
                        step: step.clone(),
                        argv,
                    },
                ));
            }
            0 => errors.push(format!(
                "gates: {JUSTFILE} : recette « {name} » : marquée « {step} » mais n'exécute aucune commande cargo"
            )),
            count => errors.push(format!(
                "gates: {JUSTFILE} : recette « {name} » : {count} invocations cargo, une porte en compte exactement une"
            )),
        }
    }
    if errors.is_empty() {
        Ok(gates)
    } else {
        Err(errors)
    }
}

/// Read the recipe file: a marker attaches to the next recipe header, a header
/// is an unindented `name: deps` line, and everything indented below belongs to
/// its body.
fn parse_recipes(content: &str) -> Vec<Recipe> {
    let mut recipes: Vec<Recipe> = Vec::new();
    let mut marker: Option<String> = None;
    for line in content.lines() {
        if line.starts_with([' ', '\t']) {
            if let Some(recipe) = recipes.last_mut() {
                recipe.body.push(line.trim().to_string());
            }
            continue;
        }
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            continue;
        }
        if let Some(step) = trimmed.strip_prefix(GATE_MARKER) {
            marker = Some(step.trim().to_string());
            continue;
        }
        if trimmed.starts_with('#') || trimmed.contains(":=") {
            continue;
        }
        if let Some((head, tail)) = trimmed.split_once(':') {
            recipes.push(Recipe {
                name: head.trim().to_string(),
                dependencies: tail.split_whitespace().map(str::to_string).collect(),
                marker: marker.take(),
                body: Vec::new(),
            });
        }
    }
    recipes
}

/// A recipe line may open on `just`'s line sigils. `@` only silences the echo,
/// `-` changes the verdict, so the two are told apart.
fn strip_sigils(line: &str) -> (&str, bool) {
    let mut rest = line;
    let mut ignores_failure = false;
    loop {
        if let Some(tail) = rest.strip_prefix('-') {
            ignores_failure = true;
            rest = tail;
        } else if let Some(tail) = rest.strip_prefix('@') {
            rest = tail;
        } else {
            return (rest, ignores_failure);
        }
    }
}

/// One workflow step, reduced to what the gate reads.
#[derive(Default)]
struct Step {
    name: String,
    run: String,
}

/// Read the workflow: a `- ` item opens a step, `name:` names it, `run:` carries
/// its shell, inline or as a block scalar whose body is every line indented at
/// least as far as its first one.
fn parse_steps(content: &str) -> Vec<Step> {
    let mut steps: Vec<Step> = Vec::new();
    let mut current: Option<Step> = None;
    let mut block: Option<usize> = None;
    let mut awaiting_block = false;

    for line in content.lines() {
        let indent = line.len() - line.trim_start().len();
        if line.trim().is_empty() {
            continue;
        }
        if awaiting_block {
            block = Some(indent);
            awaiting_block = false;
        }
        if let Some(body_indent) = block {
            if indent >= body_indent {
                if let Some(step) = current.as_mut() {
                    step.run.push_str(line.trim_end());
                    step.run.push('\n');
                }
                continue;
            }
            block = None;
        }
        let trimmed = line.trim();
        if trimmed.starts_with('#') {
            continue;
        }
        let rest = match trimmed.strip_prefix("- ") {
            Some(rest) => {
                if let Some(step) = current.take() {
                    steps.push(step);
                }
                current = Some(Step::default());
                rest
            }
            None => trimmed,
        };
        let Some(step) = current.as_mut() else {
            continue;
        };
        if let Some(value) = rest.strip_prefix("name:") {
            step.name = value.trim().to_string();
        } else if let Some(value) = rest.strip_prefix("run:") {
            let value = value.trim();
            if value.starts_with('|') || value.starts_with('>') {
                awaiting_block = true;
            } else {
                step.run.push_str(value);
                step.run.push('\n');
            }
        }
    }
    if let Some(step) = current.take() {
        steps.push(step);
    }
    steps
}

/// Join backslash continuations so a command split over five lines is read as
/// the one command it is. Comments are dropped first: a `#` line never continues
/// the line above it.
fn logical_lines(body: &str) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();
    for line in body.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        match trimmed.strip_suffix('\\') {
            Some(head) => {
                current.push_str(head.trim_end());
                current.push(' ');
            }
            None => {
                current.push_str(trimmed);
                lines.push(std::mem::take(&mut current));
            }
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

/// The `cargo` invocations of one logical shell line, in order.
///
/// Quoted spans go first, so the `echo "--- full cargo output ---"` the workflow
/// prints on a red run is text and not a gate. What is left is cut at every
/// shell operator, and each piece is a command: `cargo` opens it, or
/// `ACCEPTED_WRAPPER` and its options do. Any other prefix is an error rather
/// than a silent removal, because the comparison is only worth what its
/// normalization is.
fn cargo_invocations(line: &str) -> Result<Vec<Vec<String>>, String> {
    let unquoted = strip_quoted(line);
    let tokens: Vec<&str> = unquoted.split_whitespace().collect();
    let mut invocations = Vec::new();
    for segment in tokens.split(|token| is_shell_operator(token)) {
        let Some(start) = segment.iter().position(|token| *token == CARGO) else {
            continue;
        };
        if let Some(unknown) = unknown_wrapper(segment.get(..start).unwrap_or_default()) {
            return Err(format!(
                "« {unknown} » enveloppe cargo ; seul « {ACCEPTED_WRAPPER} » et ses options sont retirés avant comparaison"
            ));
        }
        invocations.push(
            segment
                .get(start..)
                .unwrap_or_default()
                .iter()
                .map(|token| (*token).to_string())
                .collect(),
        );
    }
    Ok(invocations)
}

/// Replace every quoted span by one space. A gate never quotes an argument, so
/// nothing a comparison reads is lost, and a `cargo` written inside a message,
/// a `grep` pattern or a `sed` program stops looking like a command.
fn strip_quoted(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut opened: Option<char> = None;
    for character in line.chars() {
        match opened {
            Some(quote) => {
                if character == quote {
                    opened = None;
                }
            }
            None => {
                if matches!(character, '\'' | '"' | '`') {
                    opened = Some(character);
                    out.push(' ');
                } else {
                    out.push(character);
                }
            }
        }
    }
    out
}

/// The first token of a prefix this gate refuses to remove, if there is one.
fn unknown_wrapper<'a>(prefix: &[&'a str]) -> Option<&'a str> {
    let head = prefix.first()?;
    if *head != ACCEPTED_WRAPPER {
        return Some(head);
    }
    prefix
        .get(1..)
        .unwrap_or_default()
        .iter()
        .find(|token| !token.starts_with('-') && !is_duration(token))
        .copied()
}

/// A `timeout` duration: digits, optionally suffixed by one unit.
fn is_duration(token: &str) -> bool {
    let digits = token.trim_end_matches(['s', 'm', 'h', 'd']);
    !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())
}

/// Where an argv stops: the shell takes over. `2>&1` counts, which is why the
/// leading digits are trimmed first.
fn is_shell_operator(token: &str) -> bool {
    token
        .trim_start_matches(|character: char| character.is_ascii_digit())
        .starts_with(['|', '&', ';', '>', '<'])
}

/// The prescriptive documents held to the recipe names, and only those. The scope
/// is closed on purpose: `docs/parity/offline-suite.md` publishes a normative
/// three-line recipe a reader is meant to copy, and `README.md` shows session
/// transcripts, so a rule wide enough to cover them would forbid the two places
/// where an invocation is the point.
pub const PROSE_DOCUMENTS: &[&str] = &["AGENTS.md", "CONTRIBUTING.md"];

/// The aggregate a document cites when it has no reason to name a single gate.
const AGGREGATE_CITATION: &str = "just check";

/// How many leading tokens identify a gate: `cargo`, its subcommand, and the
/// argument that scopes it. Two invocations sharing that head are two
/// formulations of the same gate, which is exactly how `CONTRIBUTING.md` came to
/// prescribe `cargo clippy --workspace --no-deps` against the workflow's
/// `--all-targets`. The head itself stays allowed: `cargo test --workspace` is
/// the shorter path `CONTRIBUTING.md` offers to a contributor without `just`, and
/// it contradicts no gate.
const HEAD_LENGTH: usize = 3;

/// Read the prescriptive documents and report every gate invocation they write
/// out instead of naming its recipe.
pub fn check_prose_gates(repository_root: &Path) -> Vec<String> {
    let justfile = match fs::read_to_string(repository_root.join(JUSTFILE)) {
        Ok(content) => content,
        Err(error) => return vec![format!("gates: {JUSTFILE} : illisible ({error})")],
    };
    let mut documents = Vec::new();
    for name in PROSE_DOCUMENTS {
        match fs::read_to_string(repository_root.join(name)) {
            Ok(content) => documents.push(((*name).to_string(), content)),
            Err(error) => return vec![format!("gates: {name} : illisible ({error})")],
        }
    }
    let borrowed: Vec<(&str, &str)> = documents
        .iter()
        .map(|(name, content)| (name.as_str(), content.as_str()))
        .collect();
    check_prose_documents(&justfile, &borrowed)
}

/// The same check over documents held in memory, so a fixture can reproduce a
/// divergence without touching a real document.
pub fn check_prose_documents(justfile: &str, documents: &[(&str, &str)]) -> Vec<String> {
    let gates = match gates_with_recipes(&parse_recipes(justfile)) {
        Ok(gates) => gates,
        Err(errors) => return errors,
    };
    let mut violations = Vec::new();
    for (name, content) in documents {
        for (line, candidate) in prose_invocations(content) {
            let Some(argv) = cargo_argv(&candidate) else {
                continue;
            };
            violations.extend(prose_violation(name, line, &argv, &gates));
        }
    }
    violations
}

/// The one violation an invocation carries, if it is a formulation of a gate.
fn prose_violation(
    document: &str,
    line: usize,
    argv: &[String],
    gates: &[(String, Gate)],
) -> Option<String> {
    let head = argv.get(..HEAD_LENGTH)?;
    let matching: Vec<&(String, Gate)> = gates
        .iter()
        .filter(|(_, gate)| gate.argv.get(..HEAD_LENGTH) == Some(head))
        .collect();
    if matching.is_empty() || argv.len() <= HEAD_LENGTH {
        return None;
    }
    // Two gates can share a head, `cargo test --workspace` carrying both the
    // build and the run. The one whose invocation is written verbatim names
    // itself; short of that the reader gets every candidate, because the
    // aggregate is the honest citation either way.
    let named = matching
        .iter()
        .find(|(_, gate)| gate.argv == argv)
        .or_else(|| matching.first())?;
    let citation = matching
        .iter()
        .map(|(recipe, _)| format!("just {recipe}"))
        .collect::<Vec<String>>()
        .join(" ou ");
    let written = argv.join(" ");
    let gate = &named.1;
    if gate.argv == argv {
        Some(format!(
            "gates: {document}:{line} : « {written} » est l'invocation brute de la porte « {} » ; écrire « {citation} », ou « {AGGREGATE_CITATION} » qui compose les portes du CI",
            gate.step
        ))
    } else {
        Some(format!(
            "gates: {document}:{line} : « {written} » est une formulation divergente de la porte « {} », que le CI exécute « {} » ; écrire « {citation} », ou « {AGGREGATE_CITATION} » qui compose les portes du CI",
            gate.step,
            gate.command()
        ))
    }
}

/// Every invocation a prose document offers a reader to copy: the lines of its
/// fenced blocks and the inline code spans of its prose. Text outside those two
/// is not something anyone runs, so it is not held to the recipe names.
fn prose_invocations(content: &str) -> Vec<(usize, String)> {
    let mut invocations = Vec::new();
    let mut inside_fence = false;
    for (index, line) in content.lines().enumerate() {
        let number = index + 1;
        if line.trim_start().starts_with("```") {
            inside_fence = !inside_fence;
            continue;
        }
        if inside_fence {
            invocations.push((number, line.trim().to_string()));
            continue;
        }
        for (position, span) in line.split('`').enumerate() {
            if position % 2 == 1 {
                invocations.push((number, span.trim().to_string()));
            }
        }
    }
    invocations
}

/// The `cargo` invocation of one candidate, environment prefix dropped and shell
/// plumbing cut. Unlike the two inventories, an unknown prefix is not an error
/// here: prose legitimately writes `PYXIS_UPDATE_SCHEMAS=1 cargo test ...`, and a
/// document is held to what it prescribes, not to a normalization.
fn cargo_argv(candidate: &str) -> Option<Vec<String>> {
    let tokens: Vec<&str> = candidate.split_whitespace().collect();
    let start = tokens.iter().position(|token| *token == CARGO)?;
    let rest = tokens.get(start..)?;
    let end = rest
        .iter()
        .position(|token| is_shell_operator(token))
        .unwrap_or(rest.len());
    Some(
        rest.get(..end)?
            .iter()
            .map(|token| (*token).to_string())
            .collect(),
    )
}
