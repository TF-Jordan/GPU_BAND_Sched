from __future__ import annotations

import json
import logging
import os
import subprocess
from pathlib import Path
from typing import Optional

logger = logging.getLogger(__name__)


def _repo_root() -> Path:
    return Path(__file__).resolve().parents[2]


def _candidate_binaries() -> list[Path]:
    root = _repo_root()
    env_bin = os.environ.get("OMEGA_POLICY_BIN")
    candidates = []
    if env_bin:
        candidates.append(Path(env_bin))
    candidates.extend([
        root / "target" / "release" / "omega-policy",
        root / "target" / "debug" / "omega-policy",
    ])
    return candidates


def resolve_policy_binary() -> Optional[Path]:
    for candidate in _candidate_binaries():
        if candidate.is_file() and os.access(candidate, os.X_OK):
            return candidate
    return None


def rust_available() -> bool:
    return resolve_policy_binary() is not None


def call_policy(command: str, payload: dict) -> Optional[dict | list | str]:
    binary = resolve_policy_binary()
    if binary is None:
        return None

    proc = subprocess.run(
        [str(binary), command],
        input=json.dumps(payload),
        text=True,
        capture_output=True,
        check=False,
    )
    if proc.returncode != 0:
        logger.warning(
            "moteur de politique Rust indisponible, fallback Python",
            command=command,
            returncode=proc.returncode,
            stderr=proc.stderr.strip(),
        )
        return None
    try:
        return json.loads(proc.stdout)
    except json.JSONDecodeError as exc:
        logger.warning(
            "réponse JSON invalide du moteur de politique Rust, fallback Python",
            command=command,
            error=str(exc),
        )
        return None
