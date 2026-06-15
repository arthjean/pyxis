//! `Deps` — toutes les dépendances I/O de la boucle, injectées en traits
//! (ARCHITECTURE §3.2). C'est ce qui rend `run_agent` testable sans API réelle,
//! sans terminal, sans disque réel (DoD EP-002).

use std::sync::Arc;

use agent_tokenizer::TokenCounter;

use crate::clock::Clock;
use crate::provider::Provider;
use crate::session::Session;
use crate::tools::ToolDispatch;

#[derive(Clone)]
pub struct Deps {
    pub provider: Arc<dyn Provider>,
    pub session: Arc<dyn Session>,
    pub tokenizer: Arc<dyn TokenCounter>,
    pub clock: Arc<dyn Clock>,
    pub tools: Arc<dyn ToolDispatch>,
}
