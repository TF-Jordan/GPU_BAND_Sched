#!/usr/bin/env python3
"""Worker GPU applicatif de référence pour omega-gpu-proxy.

Contrat:
- stdin: JSON d'un job Omega.
- stdout: JSON résultat.

Ce worker est volontairement CPU. Il sert de gabarit stable pour brancher
ensuite PyTorch, ONNX Runtime, CUDA Python ou un binaire métier.
"""

import json
import sys


def deterministic_matrix(n: int, seed: int) -> list[float]:
    state = max(seed, 1)
    out: list[float] = []
    for _ in range(n * n):
        state = (state * 2862933555777941757 + 3037000493) & ((1 << 64) - 1)
        out.append((state >> 32) / 0xFFFFFFFF)
    return out


def matrix_multiply(payload: dict) -> dict:
    n = int(payload.get("n", 64))
    seed = int(payload.get("seed", 1))
    if n <= 0:
        raise ValueError("n must be positive")

    a = deterministic_matrix(n, seed)
    b = deterministic_matrix(n, (seed * 6364136223846793005 + 1) & ((1 << 64) - 1))
    c = [0.0] * (n * n)

    for i in range(n):
        for k in range(n):
            aik = a[i * n + k]
            row = i * n
            brow = k * n
            for j in range(n):
                c[row + j] += aik * b[brow + j]

    return {
        "operation": "matrix_multiply",
        "backend": "python_cpu_reference",
        "n": n,
        "checksum": sum(c),
    }


def main() -> int:
    request = json.loads(sys.stdin.read())
    kind = request.get("kind", "matrix_multiply")
    payload = request.get("payload") or {}

    if kind == "echo":
        result = {"operation": "echo", "payload": payload}
    elif kind == "matrix_multiply":
        result = matrix_multiply(payload)
    else:
        raise ValueError(f"unsupported job kind: {kind}")

    print(json.dumps(result, separators=(",", ":")))
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except Exception as exc:
        print(json.dumps({"error": str(exc)}), file=sys.stderr)
        raise SystemExit(1)
