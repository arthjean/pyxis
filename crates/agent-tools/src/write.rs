//! Outil `write` — crée ou remplace un fichier du workspace. Mutation confinée
//! au workspace (US-012 AC3 ; renfort kernel Landlock en US-020). Sa sortie est
//! une simple confirmation (non untrusted). US-012.

use async_trait::async_trait;
use serde::Deserialize;

use crate::error::ToolError;
use crate::path::confine;
use crate::permission::{PermCtx, PermissionDecision};
use crate::tool::{Tool, ToolCtx, ToolOutput};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WriteInput {
    pub path: String,
    pub content: String,
}

pub struct Write;

#[async_trait]
impl Tool for Write {
    type Input = WriteInput;

    fn name(&self) -> &str {
        "write"
    }
    fn description(&self) -> String {
        "Crée ou remplace intégralement un fichier du workspace. Paramètres : \
         path (relatif au workspace), content (contenu complet). Les dossiers \
         parents manquants sont créés."
            .to_string()
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Chemin du fichier (relatif au workspace)." },
                "content": { "type": "string", "description": "Contenu complet à écrire." }
            },
            "required": ["path", "content"],
            "additionalProperties": false
        })
    }
    // Mutation (non read-only), mais édition de fichier (non « sensible » au sens
    // destructive/réseau) → auto-acceptée en AcceptEdits.
    fn is_read_only(&self) -> bool {
        false
    }
    fn is_sensitive(&self) -> bool {
        false
    }
    /// Sortie = confirmation maison, pas du contenu externe → non untrusted.
    fn returns_untrusted(&self) -> bool {
        false
    }
    fn permission(&self, _input: &Self::Input, _ctx: &PermCtx) -> PermissionDecision {
        PermissionDecision::Ask
    }

    async fn call(&self, input: Self::Input, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let path = confine(&ctx.workspace, &input.path)?;
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| ToolError::Io(format!("création du dossier parent: {e}")))?;
        }
        let bytes = input.content.len();
        tokio::fs::write(&path, input.content.as_bytes())
            .await
            .map_err(|e| ToolError::Io(format!("{}: {e}", input.path)))?;
        Ok(ToolOutput::text(format!(
            "Fichier écrit : {} ({bytes} octets)",
            input.path
        )))
    }
}
