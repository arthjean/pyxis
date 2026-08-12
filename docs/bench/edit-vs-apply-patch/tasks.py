"""Banc d'édition: mêmes tâches, deux outils.

Chaque tâche porte des fichiers de départ, une instruction indépendante de
l'outil, et une assertion sur l'état final. L'instruction ne décrit JAMAIS un
patch ni une ancre: elle décrit le résultat voulu, sinon on mesurerait la
qualité de la consigne et non celle de l'outil.
"""

RUST_PARSER = '''use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct Config {
    pub name: String,
    pub retries: u32,
}

pub fn parse_config(raw: &str) -> Option<Config> {
    let mut fields = HashMap::new();
    for line in raw.lines() {
        let (key, value) = line.split_once('=')?;
        fields.insert(key.trim().to_string(), value.trim().to_string());
    }
    Some(Config {
        name: fields.get("name")?.clone(),
        retries: fields.get("retries")?.parse().ok()?,
    })
}

pub fn describe(config: &Config) -> String {
    format!("{} ({} retries)", config.name, config.retries)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_full_config() {
        let cfg = parse_config("name = pyxis\\nretries = 3").unwrap();
        assert_eq!(describe(&cfg), "pyxis (3 retries)");
    }
}
'''

RUST_QUEUE = '''pub struct Queue<T> {
    items: Vec<T>,
    capacity: usize,
}

impl<T> Queue<T> {
    pub fn new(capacity: usize) -> Self {
        Self {
            items: Vec::new(),
            capacity,
        }
    }

    pub fn push(&mut self, item: T) -> bool {
        if self.items.len() > self.capacity {
            return false;
        }
        self.items.push(item);
        true
    }

    pub fn pop(&mut self) -> Option<T> {
        if self.items.is_empty() {
            return None;
        }
        Some(self.items.remove(0))
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }
}
'''

PY_REPORT = '''import json
import sys


def load(path):
    with open(path, encoding="utf-8") as handle:
        return json.load(handle)


def summarize(rows):
    total = 0
    failed = 0
    for row in rows:
        total += 1
        if row.get("status") == "failed":
            failed += 1
    return {"total": total, "failed": failed}


def render(summary):
    return "total=%d failed=%d" % (summary["total"], summary["failed"])


def main():
    rows = load(sys.argv[1])
    print(render(summarize(rows)))


if __name__ == "__main__":
    main()
'''

MD_TABLE = '''# Supported backends

| Backend | Status | Notes |
|---|---|---|
| chatgpt | shipped | subscription channel |
| openai | planned | BYOK |
| anthropic | planned | BYOK |

The table above is the source of truth for the roadmap.
'''

TOML_SETTINGS = '''[server]
host = "127.0.0.1"
port = 8080

[limits]
max_body_bytes = 1000000
timeout_seconds = 30

[logging]
level = "info"
'''

JS_HANDLERS = '''const routes = new Map();

export function register(path, handler) {
  routes.set(path, handler);
}

export function dispatch(path, request) {
  const handler = routes.get(path);
  if (!handler) {
    return { status: 404, body: "not found" };
  }
  return handler(request);
}

export function listRoutes() {
  return Array.from(routes.keys()).sort();
}
'''

TASKS = [
    {
        "id": "rename-fn",
        "files": {"src/config.rs": RUST_PARSER},
        "instruction": (
            "Dans src/config.rs, renomme la fonction `describe` en `format_config`. "
            "Tous ses appels doivent être mis à jour, y compris dans les tests."
        ),
        "assert": lambda f: (
            "fn format_config" in f["src/config.rs"]
            and "format_config(&cfg)" in f["src/config.rs"]
            and "fn describe" not in f["src/config.rs"]
            and "describe(" not in f["src/config.rs"]
        ),
    },
    {
        "id": "add-field",
        "files": {"src/config.rs": RUST_PARSER},
        "instruction": (
            "Dans src/config.rs, ajoute à la struct `Config` un champ `timeout_ms: u64`, "
            "lis-le depuis la clé `timeout_ms` dans `parse_config` comme les autres champs, "
            "et fais-le apparaître dans la sortie de `describe` sous la forme "
            "`nom (N retries, M ms)`. Mets le test à jour en conséquence."
        ),
        "assert": lambda f: (
            "timeout_ms: u64" in f["src/config.rs"]
            and '"timeout_ms"' in f["src/config.rs"]
            and "ms)" in f["src/config.rs"]
        ),
    },
    {
        "id": "off-by-one",
        "files": {"src/queue.rs": RUST_QUEUE},
        "instruction": (
            "Dans src/queue.rs, `push` accepte un élément de trop: la file peut dépasser "
            "`capacity`. Corrige la condition pour qu'une file pleine refuse l'ajout."
        ),
        "assert": lambda f: "self.items.len() >= self.capacity" in f["src/queue.rs"],
    },
    {
        "id": "add-method",
        "files": {"src/queue.rs": RUST_QUEUE},
        "instruction": (
            "Dans src/queue.rs, ajoute à `Queue` une méthode publique `is_empty` qui rend "
            "vrai quand la file est vide, et une méthode `clear` qui la vide."
        ),
        "assert": lambda f: (
            "pub fn is_empty" in f["src/queue.rs"] and "pub fn clear" in f["src/queue.rs"]
        ),
    },
    {
        "id": "two-files",
        "files": {"src/config.rs": RUST_PARSER, "src/queue.rs": RUST_QUEUE},
        "instruction": (
            "Ajoute le commentaire de tête `//! Internal module, unstable API.` en toute "
            "première ligne de src/config.rs ET de src/queue.rs."
        ),
        "assert": lambda f: (
            f["src/config.rs"].startswith("//! Internal module, unstable API.")
            and f["src/queue.rs"].startswith("//! Internal module, unstable API.")
        ),
    },
    {
        "id": "md-row",
        "files": {"docs/backends.md": MD_TABLE},
        "instruction": (
            "Dans docs/backends.md, ajoute une ligne au tableau pour le backend `gemini`, "
            "en statut `planned`, avec la note `BYOK`. Elle doit venir après `anthropic`."
        ),
        "assert": lambda f: (
            "| gemini | planned | BYOK |" in f["docs/backends.md"]
            and f["docs/backends.md"].index("anthropic")
            < f["docs/backends.md"].index("gemini")
        ),
    },
    {
        "id": "toml-key",
        "files": {"settings.toml": TOML_SETTINGS},
        "instruction": (
            "Dans settings.toml, passe `timeout_seconds` à 60 et ajoute une clé "
            "`max_connections = 128` dans la section [limits]."
        ),
        "assert": lambda f: (
            "timeout_seconds = 60" in f["settings.toml"]
            and "max_connections = 128" in f["settings.toml"]
        ),
    },
    {
        "id": "py-guard",
        "files": {"tools/report.py": PY_REPORT},
        "instruction": (
            "Dans tools/report.py, `main` plante si aucun argument n'est fourni. "
            "Ajoute une garde qui affiche `usage: report.py <file.json>` sur stderr "
            "et sort avec le code 2 quand l'argument manque."
        ),
        # `raise SystemExit(2)` est équivalent à `sys.exit(2)`: l'oracle accepte
        # les deux, sinon il mesure un style et non un résultat.
        "assert": lambda f: (
            "usage: report.py" in f["tools/report.py"]
            and ("sys.exit(2)" in f["tools/report.py"]
                 or "SystemExit(2)" in f["tools/report.py"])
        ),
    },
    {
        "id": "py-field",
        "files": {"tools/report.py": PY_REPORT},
        "instruction": (
            "Dans tools/report.py, `summarize` doit aussi compter les lignes dont le "
            "statut vaut `skipped`, sous la clé `skipped`, et `render` doit l'afficher "
            "à la fin sous la forme ` skipped=N`."
        ),
        "assert": lambda f: (
            '"skipped"' in f["tools/report.py"] and "skipped=%d" in f["tools/report.py"]
        ),
    },
    {
        "id": "js-method",
        "files": {"src/router.js": JS_HANDLERS},
        "instruction": (
            "Dans src/router.js, ajoute une fonction exportée `unregister(path)` qui "
            "retire une route et rend vrai si elle existait, faux sinon."
        ),
        "assert": lambda f: (
            "export function unregister" in f["src/router.js"]
            and "routes.delete" in f["src/router.js"]
        ),
    },
    {
        "id": "js-status",
        "files": {"src/router.js": JS_HANDLERS},
        "instruction": (
            "Dans src/router.js, `dispatch` doit rendre le statut 405 avec le corps "
            "`method not allowed` quand `request.method` vaut `TRACE`, avant de chercher "
            "la route."
        ),
        "assert": lambda f: (
            "405" in f["src/router.js"] and "method not allowed" in f["src/router.js"]
        ),
    },
    {
        "id": "delete-fn",
        "files": {"src/router.js": JS_HANDLERS},
        "instruction": (
            "Dans src/router.js, supprime entièrement la fonction `listRoutes`, qui n'est "
            "plus utilisée."
        ),
        "assert": lambda f: "listRoutes" not in f["src/router.js"],
    },
]


# ── Second lot: éditions à plusieurs sites, ancres répétées, indentation.
# C'est là que l'hypothèse est censée se jouer: sur une édition à un
# seul site, les deux outils font la même chose.

RUST_SERVICE = '''use std::time::Duration;

const RETRY_DELAY: Duration = Duration::from_millis(200);

pub struct Client {
    endpoint: String,
    retries: u32,
}

impl Client {
    pub fn new(endpoint: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
            retries: 3,
        }
    }

    pub fn fetch(&self, path: &str) -> Result<String, String> {
        let mut attempt = 0;
        loop {
            attempt += 1;
            match self.get(path) {
                Ok(body) => return Ok(body),
                Err(err) if attempt >= self.retries => return Err(err),
                Err(_) => std::thread::sleep(RETRY_DELAY),
            }
        }
    }

    pub fn post(&self, path: &str, body: &str) -> Result<String, String> {
        let mut attempt = 0;
        loop {
            attempt += 1;
            match self.send(path, body) {
                Ok(body) => return Ok(body),
                Err(err) if attempt >= self.retries => return Err(err),
                Err(_) => std::thread::sleep(RETRY_DELAY),
            }
        }
    }

    fn get(&self, path: &str) -> Result<String, String> {
        Err(format!("GET {}{} unimplemented", self.endpoint, path))
    }

    fn send(&self, path: &str, _body: &str) -> Result<String, String> {
        Err(format!("POST {}{} unimplemented", self.endpoint, path))
    }
}
'''

PY_PIPELINE = '''import logging

logger = logging.getLogger(__name__)


class Stage:
    def __init__(self, name, fn):
        self.name = name
        self.fn = fn

    def run(self, payload):
        logger.info("running %s", self.name)
        return self.fn(payload)


class Pipeline:
    def __init__(self):
        self.stages = []

    def add(self, name, fn):
        self.stages.append(Stage(name, fn))
        return self

    def run(self, payload):
        for stage in self.stages:
            payload = stage.run(payload)
        return payload

    def names(self):
        return [stage.name for stage in self.stages]
'''

GO_STORE = '''package store

import "errors"

var ErrMissing = errors.New("missing key")

type Store struct {
	data map[string]string
}

func New() *Store {
	return &Store{data: make(map[string]string)}
}

func (s *Store) Get(key string) (string, error) {
	value, ok := s.data[key]
	if !ok {
		return "", ErrMissing
	}
	return value, nil
}

func (s *Store) Set(key, value string) {
	s.data[key] = value
}

func (s *Store) Len() int {
	return len(s.data)
}
'''

SH_DEPLOY = '''#!/usr/bin/env bash
set -euo pipefail

ENVIRONMENT="${1:-staging}"
IMAGE="registry.example.com/app:latest"

echo "deploying to ${ENVIRONMENT}"
docker pull "${IMAGE}"
docker stop app || true
docker run -d --name app "${IMAGE}"
echo "done"
'''

TASKS_B = [
    {
        "id": "const-rename",
        "files": {"src/client.rs": RUST_SERVICE},
        "instruction": (
            "Dans src/client.rs, renomme la constante `RETRY_DELAY` en `BACKOFF_DELAY` "
            "et mets à jour toutes ses utilisations."
        ),
        "assert": lambda f: (
            "BACKOFF_DELAY" in f["src/client.rs"]
            and "RETRY_DELAY" not in f["src/client.rs"]
            and f["src/client.rs"].count("BACKOFF_DELAY") >= 3
        ),
    },
    {
        "id": "dup-blocks",
        "files": {"src/client.rs": RUST_SERVICE},
        "instruction": (
            "Dans src/client.rs, les boucles de `fetch` et de `post` sont identiques. "
            "Dans CHACUNE des deux, remplace `std::thread::sleep(RETRY_DELAY)` par "
            "`std::thread::sleep(RETRY_DELAY * attempt)` pour un backoff progressif."
        ),
        "assert": lambda f: f["src/client.rs"].count("RETRY_DELAY * attempt") == 2,
    },
    {
        "id": "add-param",
        "files": {"src/client.rs": RUST_SERVICE},
        "instruction": (
            "Dans src/client.rs, `new` doit accepter un second paramètre `retries: u32` "
            "utilisé à la place de la valeur 3 codée en dur."
        ),
        "assert": lambda f: (
            "retries: u32" in f["src/client.rs"]
            and "retries: 3" not in f["src/client.rs"]
        ),
    },
    {
        "id": "py-logging",
        "files": {"pipeline.py": PY_PIPELINE},
        "instruction": (
            "Dans pipeline.py, `Stage.run` doit journaliser aussi la fin de l'étape avec "
            "`logger.info(\"finished %s\", self.name)` APRÈS l'appel, et rendre le résultat."
        ),
        "assert": lambda f: (
            'finished %s' in f["pipeline.py"]
            and f["pipeline.py"].index("running %s") < f["pipeline.py"].index("finished %s")
        ),
    },
    {
        "id": "py-error",
        "files": {"pipeline.py": PY_PIPELINE},
        "instruction": (
            "Dans pipeline.py, `Pipeline.run` doit attraper toute exception levée par une "
            "étape, journaliser `logger.error(\"stage %s failed\", stage.name)` et relancer."
        ),
        "assert": lambda f: (
            "stage %s failed" in f["pipeline.py"]
            and "raise" in f["pipeline.py"]
            and "except" in f["pipeline.py"]
        ),
    },
    {
        "id": "py-two-methods",
        "files": {"pipeline.py": PY_PIPELINE},
        "instruction": (
            "Dans pipeline.py, ajoute à `Pipeline` une méthode `remove(name)` qui retire "
            "l'étape portant ce nom et rend vrai si elle existait, et une méthode "
            "`clear()` qui vide la liste des étapes."
        ),
        "assert": lambda f: (
            "def remove" in f["pipeline.py"] and "def clear" in f["pipeline.py"]
        ),
    },
    {
        "id": "go-method",
        "files": {"store/store.go": GO_STORE},
        "instruction": (
            "Dans store/store.go, ajoute une méthode `Delete(key string) bool` qui retire "
            "une clé et rend vrai si elle existait."
        ),
        "assert": lambda f: (
            "func (s *Store) Delete" in f["store/store.go"]
            and "delete(s.data" in f["store/store.go"]
        ),
    },
    {
        "id": "go-error",
        "files": {"store/store.go": GO_STORE},
        "instruction": (
            "Dans store/store.go, ajoute une erreur exportée `ErrEmptyKey` et fais que "
            "`Set` ne fasse rien quand la clé est vide. `Set` doit alors rendre une erreur, "
            "donc sa signature devient `Set(key, value string) error`."
        ),
        "assert": lambda f: (
            "ErrEmptyKey" in f["store/store.go"]
            and "Set(key, value string) error" in f["store/store.go"]
        ),
    },
    {
        "id": "sh-flag",
        "files": {"deploy.sh": SH_DEPLOY},
        "instruction": (
            "Dans deploy.sh, l'image doit être paramétrable: lis-la depuis la variable "
            "d'environnement IMAGE si elle existe, sinon garde la valeur actuelle par "
            "défaut. Ajoute aussi `--restart unless-stopped` à la commande docker run."
        ),
        "assert": lambda f: (
            "${IMAGE:-" in f["deploy.sh"] and "--restart unless-stopped" in f["deploy.sh"]
        ),
    },
    {
        "id": "multi-file-const",
        "files": {"src/client.rs": RUST_SERVICE, "store/store.go": GO_STORE},
        "instruction": (
            "Ajoute une ligne de licence en toute première ligne des DEUX fichiers "
            "src/client.rs et store/store.go: `// SPDX-License-Identifier: GPL-3.0-or-later`. "
            "Dans store/store.go elle doit précéder la déclaration `package`."
        ),
        "assert": lambda f: (
            f["src/client.rs"].startswith("// SPDX-License-Identifier: GPL-3.0-or-later")
            and f["store/store.go"].startswith(
                "// SPDX-License-Identifier: GPL-3.0-or-later"
            )
        ),
    },
]
