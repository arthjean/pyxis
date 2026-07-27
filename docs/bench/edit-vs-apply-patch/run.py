"""Exécute le banc: chaque tâche, deux fois, un outil d'édition imposé par run.

Un workspace neuf par run, donc une session JSONL par run, donc un dépouillement
sans ambiguïté. Le run est headless (`-p`), en `full-access`, borné en tokens et
en temps: aucune confirmation ne doit jamais être attendue.
"""

import concurrent.futures as futures
import json
import os
import pathlib
import shutil
import subprocess
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from tasks import TASKS  # noqa: E402

PYXIS = "/home/arthur/dev/pyxis/target/release/pyxis"
MODEL = "gpt-5.3-codex-spark"
ROOT = pathlib.Path(__file__).resolve().parent / "runs"
TOKEN_BUDGET = "120000"
TIMEOUT_S = 300
PARALLEL = 3

FORCE = {
    "edit": (
        "Pour toute modification de fichier, utilise EXCLUSIVEMENT l'outil `edit`. "
        "N'utilise ni `apply_patch`, ni `write`, ni `bash` pour écrire."
    ),
    "apply_patch": (
        "Pour toute modification de fichier, utilise EXCLUSIVEMENT l'outil `apply_patch`. "
        "N'utilise ni `edit`, ni `write`, ni `bash` pour écrire."
    ),
}


def prepare(ws: pathlib.Path, files: dict) -> None:
    if ws.exists():
        shutil.rmtree(ws)
    ws.mkdir(parents=True)
    for rel, content in files.items():
        target = ws / rel
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(content, encoding="utf-8")


def edit_calls(ws: pathlib.Path) -> list:
    """Appels d'édition de la session, appariés à leur résultat."""
    out = []
    sessions = sorted((ws / ".pyxis" / "sessions").glob("*.jsonl"))
    pending = {}
    for session in sessions:
        for line in session.read_text(encoding="utf-8", errors="replace").splitlines():
            line = line.strip()
            if not line:
                continue
            try:
                entry = json.loads(line)
            except Exception:
                continue
            messages = []
            if "role" in entry:
                messages = [entry]
            elif entry.get("type") == "compact_checkpoint":
                messages = entry.get("messages", [])
            for message in messages:
                for block in message.get("content") or []:
                    if block.get("type") == "tool_use":
                        pending[block.get("id")] = block.get("name")
                    elif block.get("type") == "tool_result":
                        name = pending.pop(block.get("tool_use_id"), None)
                        if name in ("edit", "apply_patch", "write", "bash"):
                            out.append(
                                {
                                    "tool": name,
                                    "is_error": bool(block.get("is_error")),
                                    "content": (block.get("content") or "")[:400],
                                }
                            )
    return out


def run_one(task: dict, tool: str) -> dict:
    ws = ROOT / f"{task['id']}--{tool}"
    prepare(ws, task["files"])
    prompt = f"{task['instruction']}\n\n{FORCE[tool]}"
    started = time.time()
    try:
        proc = subprocess.run(
            [
                PYXIS,
                "-p",
                prompt,
                "--model",
                MODEL,
                "--permission-mode",
                "full-access",
                "--token-budget",
                TOKEN_BUDGET,
            ],
            cwd=ws,
            capture_output=True,
            text=True,
            timeout=TIMEOUT_S,
        )
        rc, err = proc.returncode, proc.stderr[-600:]
    except subprocess.TimeoutExpired:
        rc, err = -1, "timeout"
    elapsed = round(time.time() - started, 1)

    final = {}
    for rel in task["files"]:
        path = ws / rel
        final[rel] = path.read_text(encoding="utf-8") if path.exists() else ""
    try:
        ok = bool(task["assert"](final))
    except Exception as exc:  # une assertion qui explose = tâche non aboutie
        ok = False
        err = f"{err}\nassert: {exc}"

    calls = edit_calls(ws)
    target = [c for c in calls if c["tool"] == tool]
    other = [c for c in calls if c["tool"] != tool]
    return {
        "task": task["id"],
        "tool": tool,
        "exit_code": rc,
        "seconds": elapsed,
        "task_ok": ok,
        "calls": len(target),
        "failed_calls": sum(1 for c in target if c["is_error"]),
        "off_tool_calls": len(other),
        "off_tools": sorted({c["tool"] for c in other}),
        "failures": [c["content"] for c in target if c["is_error"]],
        "stderr_tail": err.strip()[-300:],
    }


def main() -> None:
    ROOT.mkdir(parents=True, exist_ok=True)
    jobs = [(task, tool) for task in TASKS for tool in ("edit", "apply_patch")]
    results = []
    with futures.ThreadPoolExecutor(max_workers=PARALLEL) as pool:
        pending = {pool.submit(run_one, task, tool): (task["id"], tool) for task, tool in jobs}
        for future in futures.as_completed(pending):
            name = pending[future]
            try:
                row = future.result()
            except Exception as exc:
                row = {"task": name[0], "tool": name[1], "error": str(exc)}
            results.append(row)
            print(
                f"[{len(results):>2}/{len(jobs)}] {row['task']:<12} {row['tool']:<12} "
                f"calls={row.get('calls')} failed={row.get('failed_calls')} "
                f"ok={row.get('task_ok')} {row.get('seconds')}s",
                flush=True,
            )
    out = ROOT.parent / "results.json"
    out.write_text(json.dumps(results, indent=2, ensure_ascii=False), encoding="utf-8")
    print(f"\nwritten: {out}")


if __name__ == "__main__":
    main()
