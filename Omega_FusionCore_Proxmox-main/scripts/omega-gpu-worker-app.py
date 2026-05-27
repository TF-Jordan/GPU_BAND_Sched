#!/usr/bin/env python3
"""Worker applicatif multi-backend pour omega-gpu-proxy.

Jobs supportés:
- matrix_multiply: PyTorch CUDA si disponible, sinon CPU PyTorch, sinon CPU pur.
- inference: ONNX Runtime si model_path est fourni, sinon PyTorch matmul de test.
- video_encode: ffmpeg avec h264_nvenc/hevc_nvenc si disponible.
- render: blender en mode batch si scene_path est fourni.
- custom: commande externe explicitement autorisée par payload.command.

Le worker lit un JSON sur stdin et retourne un JSON sur stdout.
"""

from __future__ import annotations

import json
import os
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path
from typing import Any


def deterministic_matrix(n: int, seed: int) -> list[float]:
    state = max(seed, 1)
    out: list[float] = []
    for _ in range(n * n):
        state = (state * 2862933555777941757 + 3037000493) & ((1 << 64) - 1)
        out.append((state >> 32) / 0xFFFFFFFF)
    return out


def matrix_multiply_cpu(payload: dict[str, Any]) -> dict[str, Any]:
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


def matrix_multiply_torch(payload: dict[str, Any]) -> dict[str, Any]:
    import torch

    n = int(payload.get("n", 64))
    seed = int(payload.get("seed", 1))
    if n <= 0:
        raise ValueError("n must be positive")

    device = "cuda" if torch.cuda.is_available() else "cpu"
    torch.manual_seed(seed)
    started = time.time()
    a = torch.rand((n, n), device=device, dtype=torch.float32)
    b = torch.rand((n, n), device=device, dtype=torch.float32)
    c = a @ b
    if device == "cuda":
        torch.cuda.synchronize()
    elapsed_ms = int((time.time() - started) * 1000)
    return {
        "operation": "matrix_multiply",
        "backend": "torch",
        "device": device,
        "cuda_available": bool(torch.cuda.is_available()),
        "gpu_name": torch.cuda.get_device_name(0) if torch.cuda.is_available() else None,
        "n": n,
        "checksum": float(c.sum().item()),
        "duration_ms": elapsed_ms,
    }


def run_matrix_multiply(payload: dict[str, Any]) -> dict[str, Any]:
    require_cuda = bool(payload.get("require_cuda")) or os.environ.get("OMEGA_GPU_WORKER_REQUIRE_CUDA") == "1"
    try:
        result = matrix_multiply_torch(payload)
        if require_cuda and result.get("device") != "cuda":
            raise RuntimeError(
                f"CUDA obligatoire mais torch utilise device={result.get('device')} "
                f"cuda_available={result.get('cuda_available')}"
            )
        return result
    except ModuleNotFoundError as exc:
        if require_cuda:
            raise RuntimeError(
                f"CUDA obligatoire mais PyTorch est absent dans {sys.executable}"
            ) from exc
        return matrix_multiply_cpu(payload)


def run_inference(payload: dict[str, Any]) -> dict[str, Any]:
    model_path = payload.get("model_path")
    if model_path:
        try:
            import numpy as np
            import onnxruntime as ort
        except ModuleNotFoundError as exc:
            raise RuntimeError("onnxruntime et numpy sont requis pour inference model_path") from exc

        providers = ["CUDAExecutionProvider", "CPUExecutionProvider"]
        session = ort.InferenceSession(str(model_path), providers=providers)
        input_name = session.get_inputs()[0].name
        shape = [dim if isinstance(dim, int) and dim > 0 else 1 for dim in session.get_inputs()[0].shape]
        data = np.ones(shape, dtype=np.float32)
        started = time.time()
        outputs = session.run(None, {input_name: data})
        elapsed_ms = int((time.time() - started) * 1000)
        return {
            "operation": "inference",
            "backend": "onnxruntime",
            "providers": session.get_providers(),
            "model_path": str(model_path),
            "output_count": len(outputs),
            "first_output_shape": list(outputs[0].shape) if outputs else None,
            "duration_ms": elapsed_ms,
        }

    result = matrix_multiply_torch({"n": int(payload.get("n", 256)), "seed": int(payload.get("seed", 1))})
    result["operation"] = "inference_synthetic"
    return result


def run_video_encode(payload: dict[str, Any]) -> dict[str, Any]:
    ffmpeg = shutil.which("ffmpeg")
    if not ffmpeg:
        raise RuntimeError("ffmpeg introuvable")

    input_path = payload.get("input_path")
    codec = payload.get("codec", "h264_nvenc")
    duration = int(payload.get("duration_secs", 3))
    output_path = payload.get("output_path")

    with tempfile.TemporaryDirectory(prefix="omega-gpu-encode-") as tmp:
        tmpdir = Path(tmp)
        if not input_path:
            input_path = str(tmpdir / "input.mp4")
            subprocess.run(
                [
                    ffmpeg,
                    "-hide_banner",
                    "-loglevel",
                    "error",
                    "-f",
                    "lavfi",
                    "-i",
                    "testsrc2=size=1280x720:rate=30",
                    "-t",
                    str(duration),
                    "-pix_fmt",
                    "yuv420p",
                    "-y",
                    input_path,
                ],
                check=True,
            )
        if not output_path:
            output_path = str(tmpdir / "output.mp4")

        started = time.time()
        proc = subprocess.run(
            [ffmpeg, "-hide_banner", "-loglevel", "error", "-y", "-i", str(input_path), "-c:v", str(codec), str(output_path)],
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
        elapsed_ms = int((time.time() - started) * 1000)
        if proc.returncode != 0:
            raise RuntimeError(f"ffmpeg {codec} a échoué: {proc.stderr.strip()}")

        size = os.path.getsize(output_path)
        return {
            "operation": "video_encode",
            "backend": "ffmpeg",
            "codec": codec,
            "input_path": str(input_path),
            "output_path": str(output_path),
            "output_bytes": size,
            "duration_ms": elapsed_ms,
        }


def run_render(payload: dict[str, Any]) -> dict[str, Any]:
    blender = shutil.which("blender")
    if not blender:
        raise RuntimeError("blender introuvable")

    scene_path = payload.get("scene_path")
    if not scene_path:
        raise ValueError("payload.scene_path requis pour render")

    frame = str(payload.get("frame", 1))
    output_prefix = payload.get("output_prefix") or tempfile.mkdtemp(prefix="omega-gpu-render-") + "/frame_"
    engine = str(payload.get("engine", "CYCLES"))
    device = str(payload.get("device", "CUDA"))

    script = (
        "import bpy\n"
        f"bpy.context.scene.render.engine = {engine!r}\n"
        "prefs = bpy.context.preferences.addons.get('cycles')\n"
        "cprefs = prefs.preferences if prefs else None\n"
        f"cprefs.compute_device_type = {device!r} if cprefs else ''\n"
        "bpy.context.scene.cycles.device = 'GPU'\n"
    )

    started = time.time()
    proc = subprocess.run(
        [blender, "-b", str(scene_path), "--python-expr", script, "-o", str(output_prefix), "-f", frame],
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    elapsed_ms = int((time.time() - started) * 1000)
    if proc.returncode != 0:
        raise RuntimeError(f"blender render a échoué: {proc.stderr.strip() or proc.stdout[-1000:]}")

    return {
        "operation": "render",
        "backend": "blender",
        "engine": engine,
        "device": device,
        "scene_path": str(scene_path),
        "output_prefix": str(output_prefix),
        "frame": int(frame),
        "duration_ms": elapsed_ms,
    }


def run_custom(payload: dict[str, Any]) -> dict[str, Any]:
    command = payload.get("command")
    if not isinstance(command, list) or not command:
        raise ValueError("payload.command doit être une liste non vide")

    allow_custom = os.environ.get("OMEGA_GPU_WORKER_ALLOW_CUSTOM", "0") == "1"
    if not allow_custom:
        raise RuntimeError("custom désactivé: définir OMEGA_GPU_WORKER_ALLOW_CUSTOM=1")

    started = time.time()
    proc = subprocess.run(command, text=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    elapsed_ms = int((time.time() - started) * 1000)
    if proc.returncode != 0:
        raise RuntimeError(f"custom command failed rc={proc.returncode}: {proc.stderr.strip()}")
    return {
        "operation": "custom",
        "backend": "subprocess",
        "command": command,
        "stdout": proc.stdout[-4096:],
        "stderr": proc.stderr[-4096:],
        "duration_ms": elapsed_ms,
    }


def main() -> int:
    request = json.loads(sys.stdin.read())
    kind = request.get("kind", "matrix_multiply")
    payload = request.get("payload") or {}

    if kind == "echo":
        result = {"operation": "echo", "payload": payload}
    elif kind == "matrix_multiply":
        result = run_matrix_multiply(payload)
    elif kind == "inference":
        result = run_inference(payload)
    elif kind == "video_encode":
        result = run_video_encode(payload)
    elif kind == "render":
        result = run_render(payload)
    elif kind == "custom":
        result = run_custom(payload)
    else:
        raise ValueError(f"unsupported job kind: {kind}")

    result["worker"] = "omega-gpu-worker-app.py"
    print(json.dumps(result, separators=(",", ":")))
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except Exception as exc:
        print(json.dumps({"error": str(exc)}), file=sys.stderr)
        raise SystemExit(1)
