//! Sortie JSONL d'événements en mode headless (US-017).
//!
//! Un objet JSON par ligne sur la sortie standard, vidé immédiatement : un
//! appelant automatisé (CI, orchestrateur) lit le déroulement d'un run sans
//! analyser du texte destiné à un humain. Le schéma est documenté dans
//! `docs/EVENT_SCHEMA.md` et chaque ligne porte son numéro de version, seule
//! façon pour un consommateur de savoir s'il comprend ce qu'il lit.
//!
//! Ce module n'existe que côté CLI : `agent-core` reste sans I/O (invariant 1).
//! Il ne fait aucune décision de présentation non plus — la sérialisation est
//! celle d'`AgentEvent`, donc le contrat, pas un rendu.

use std::io::Write;

use agent_core::{AgentEvent, HeadlessEnd};
use serde::Serialize;

/// Version du schéma d'événements. À incrémenter dès qu'une ligne déjà émise
/// change de forme ; ajouter une variante ou un champ optionnel n'en change pas
/// la lecture et ne l'incrémente donc pas (même règle qu'`AgentEvent`).
pub const SCHEMA_VERSION: u32 = 1;

/// Format de sortie du mode headless.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    /// Texte de la réponse finale, tel qu'avant US-017 (défaut).
    Text,
    /// Un événement JSON par ligne.
    Json,
}

pub fn output_format_from_arg(arg: &str) -> Option<OutputFormat> {
    match arg.trim().to_ascii_lowercase().as_str() {
        "text" => Some(OutputFormat::Text),
        "json" | "jsonl" | "stream-json" => Some(OutputFormat::Json),
        _ => None,
    }
}

/// Ligne d'événement. `event` est aplati : la forme d'`AgentEvent`
/// (`{"type": …, "data": …}`) reste visible telle quelle, augmentée du `schema`.
#[derive(Serialize)]
struct EventLine<'a> {
    schema: u32,
    #[serde(flatten)]
    event: &'a AgentEvent,
}

/// Récapitulatif de fin de run (AC3). Ce n'est PAS un `AgentEvent` : l'identifiant
/// de session et le code de sortie sont des faits de processus, que le cœur n'a
/// aucune raison de connaître.
#[derive(Serialize)]
struct SummaryLine<'a> {
    schema: u32,
    r#type: &'static str,
    data: RunSummary<'a>,
}

#[derive(Serialize)]
struct RunSummary<'a> {
    session_id: &'a str,
    /// Nombre d'allers-retours modèle observés (`model_turn`).
    model_turns: u32,
    input_tokens: u64,
    output_tokens: u64,
    /// Cause de fin : `end_turn`, `exhausted` ou `error`.
    end: &'static str,
    /// Détail de la cause quand il en existe un.
    #[serde(skip_serializing_if = "Option::is_none")]
    end_detail: Option<String>,
    exit_code: i32,
}

/// Écrivain de lignes. En mode `Text`, tout est inerte : aucun octet n'est écrit,
/// ce qui garantit AC4 (sortie textuelle par défaut inchangée) par construction
/// plutôt que par relecture.
pub struct EventWriter {
    format: OutputFormat,
    model_turns: u32,
    input_tokens: u64,
    output_tokens: u64,
}

impl EventWriter {
    pub fn new(format: OutputFormat) -> Self {
        Self {
            format,
            model_turns: 0,
            input_tokens: 0,
            output_tokens: 0,
        }
    }

    pub fn is_json(&self) -> bool {
        self.format == OutputFormat::Json
    }

    /// Écrit un événement et met à jour la comptabilité du récapitulatif. Appelé
    /// pour CHAQUE événement, y compris en mode texte, afin que le comptage ne
    /// dépende pas du format choisi.
    pub fn event(&mut self, event: &AgentEvent) {
        if let AgentEvent::ModelTurn(view) = event {
            self.model_turns = self.model_turns.max(view.index);
            self.input_tokens = view.input_tokens;
            self.output_tokens = view.output_tokens;
        }
        if self.format != OutputFormat::Json {
            return;
        }
        let line = EventLine {
            schema: SCHEMA_VERSION,
            event,
        };
        // Un événement non sérialisable serait un bug de contrat, pas une
        // condition d'exécution : on le signale sur stderr sans couper le flux.
        match serde_json::to_string(&line) {
            Ok(json) => write_line(&json),
            Err(err) => eprintln!("[jsonl] event not serializable: {err}"),
        }
    }

    /// Dernière ligne du run (AC3, AC6). `exit_code` distingue un succès d'un
    /// échec pour un appelant qui ne lirait que le code de retour.
    pub fn run_summary(&mut self, session_id: &str, ended: &HeadlessEnd) {
        if self.format != OutputFormat::Json {
            return;
        }
        let (end, end_detail, exit_code) = match ended {
            HeadlessEnd::EndTurn => ("end_turn", None, 0),
            HeadlessEnd::Exhausted(reason) => ("exhausted", Some(format!("{reason:?}")), 1),
            HeadlessEnd::Error(err) => ("error", Some(err.to_string()), 1),
        };
        let line = SummaryLine {
            schema: SCHEMA_VERSION,
            r#type: "run_summary",
            data: RunSummary {
                session_id,
                model_turns: self.model_turns,
                input_tokens: self.input_tokens,
                output_tokens: self.output_tokens,
                end,
                end_detail,
                exit_code,
            },
        };
        match serde_json::to_string(&line) {
            Ok(json) => write_line(&json),
            Err(err) => eprintln!("[jsonl] summary not serializable: {err}"),
        }
    }
}

/// Une ligne, vidée immédiatement : un orchestrateur qui suit le flux ne doit pas
/// attendre la fin du run pour voir le premier événement.
fn write_line(json: &str) {
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    if writeln!(out, "{json}").is_ok() {
        let _ = out.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::event::{ModelTurnView, ToolResultView};

    fn line_of(event: &AgentEvent) -> serde_json::Value {
        let line = EventLine {
            schema: SCHEMA_VERSION,
            event,
        };
        serde_json::to_value(&line).unwrap()
    }

    #[test]
    fn output_format_parses_aliases_and_rejects_garbage() {
        assert_eq!(output_format_from_arg("text"), Some(OutputFormat::Text));
        assert_eq!(output_format_from_arg(" JSON "), Some(OutputFormat::Json));
        assert_eq!(
            output_format_from_arg("stream-json"),
            Some(OutputFormat::Json)
        );
        assert_eq!(output_format_from_arg("yaml"), None);
    }

    /// AC2 : chaque ligne porte la version du schéma, et la forme d'`AgentEvent`
    /// reste lisible telle quelle à côté.
    #[test]
    fn every_line_carries_the_schema_version_and_the_event_shape() {
        let value = line_of(&AgentEvent::Text("bonjour".into()));
        assert_eq!(value["schema"], SCHEMA_VERSION);
        assert_eq!(value["type"], "text");
        assert_eq!(value["data"], "bonjour");

        // Variante unitaire : toujours un objet, jamais une chaîne nue.
        let value = line_of(&AgentEvent::EndTurn);
        assert_eq!(value["schema"], SCHEMA_VERSION);
        assert_eq!(value["type"], "end_turn");
    }

    /// AC5 : contenu non textuel et caractères de contrôle → la ligne reste un
    /// JSON valide et analysable, sans octet brut ni saut de ligne interne.
    #[test]
    fn control_characters_stay_inside_one_valid_json_line() {
        let hostile = "ligne1\nligne2\u{0}\u{1b}[31mrouge\u{7}\r\t\"quote\"\\";
        let event = AgentEvent::ToolResult(ToolResultView {
            id: "call_1".to_string(),
            content: hostile.to_string(),
            is_error: false,
            error_kind: None,
            untrusted: true,
        });

        let json = serde_json::to_string(&EventLine {
            schema: SCHEMA_VERSION,
            event: &event,
        })
        .unwrap();

        assert!(
            !json.contains('\n'),
            "un saut de ligne brut casserait le découpage JSONL: {json}"
        );
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["data"]["content"], hostile);
    }

    /// AC3 : le récapitulatif compte les tours et reprend les jetons du dernier
    /// `model_turn` observé, sans les additionner deux fois (ils sont cumulés).
    #[test]
    fn summary_tracks_the_last_cumulative_counters() {
        let mut writer = EventWriter::new(OutputFormat::Text);
        for index in 1..=3 {
            writer.event(&AgentEvent::ModelTurn(ModelTurnView {
                index,
                input_tokens: u64::from(index) * 100,
                output_tokens: u64::from(index) * 10,
            }));
        }

        assert_eq!(writer.model_turns, 3);
        assert_eq!(writer.input_tokens, 300);
        assert_eq!(writer.output_tokens, 30);
    }

    #[test]
    fn text_format_writes_nothing() {
        let mut writer = EventWriter::new(OutputFormat::Text);
        assert!(!writer.is_json());
        // Rien à observer sur stdout : l'absence d'écriture est structurelle
        // (retour anticipé sur le format), pas une propriété de ce test.
        writer.event(&AgentEvent::Text("x".into()));
        writer.run_summary("s.jsonl", &HeadlessEnd::EndTurn);
    }
}
