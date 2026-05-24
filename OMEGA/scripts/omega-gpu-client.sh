#!/usr/bin/env bash
set -euo pipefail

usage() {
    cat <<'USAGE'
Usage:
  scripts/omega-gpu-client.sh status --proxy http://NODE:9400
  scripts/omega-gpu-client.sh budget --proxy http://NODE:9400 --vmid VMID --vram-mib MIB
  scripts/omega-gpu-client.sh matmul --proxy http://NODE:9400 --vmid VMID [--n 64] [--vram-mib 64]
  scripts/omega-gpu-client.sh inference --proxy http://NODE:9400 --vmid VMID [--model-path MODEL.onnx] [--n 256]
  scripts/omega-gpu-client.sh encode --proxy http://NODE:9400 --vmid VMID [--input-path in.mp4] [--codec h264_nvenc]
  scripts/omega-gpu-client.sh render --proxy http://NODE:9400 --vmid VMID --scene-path scene.blend [--frame 1]
  scripts/omega-gpu-client.sh custom --proxy http://NODE:9400 --vmid VMID --command-json '["nvidia-smi"]'

Objectif:
  Client minimal utilisable depuis une VM ou depuis l'hote pour valider
  le proxy GPU applicatif Omega.

Sécurité:
  Définir OMEGA_GPU_PROXY_API_TOKEN ou utiliser --token TOKEN si le proxy
  est protégé. Le client envoie Authorization: Bearer TOKEN.
USAGE
}

cmd="${1:-}"
shift || true

proxy="${OMEGA_GPU_PROXY_URL:-http://127.0.0.1:9400}"
token="${OMEGA_GPU_PROXY_API_TOKEN:-}"
vmid="${OMEGA_TEST_VMID:-}"
vram_mib="64"
n="64"
model_path=""
input_path=""
codec="h264_nvenc"
scene_path=""
frame="1"
command_json=""

while [[ $# -gt 0 ]]; do
    case "$1" in
        --proxy) proxy="$2"; shift 2 ;;
        --token) token="$2"; shift 2 ;;
        --vmid) vmid="$2"; shift 2 ;;
        --vram-mib) vram_mib="$2"; shift 2 ;;
        --n) n="$2"; shift 2 ;;
        --model-path) model_path="$2"; shift 2 ;;
        --input-path) input_path="$2"; shift 2 ;;
        --codec) codec="$2"; shift 2 ;;
        --scene-path) scene_path="$2"; shift 2 ;;
        --frame) frame="$2"; shift 2 ;;
        --command-json) command_json="$2"; shift 2 ;;
        -h|--help) usage; exit 0 ;;
        *) echo "argument inconnu: $1" >&2; usage; exit 2 ;;
    esac
done

json_pretty() {
    if command -v python3 >/dev/null 2>&1; then
        python3 -m json.tool
    else
        cat
    fi
}

curl_auth() {
    if [[ -n "$token" ]]; then
        curl -H "Authorization: Bearer ${token}" "$@"
    else
        curl "$@"
    fi
}

submit_and_wait() {
    local kind="$1"
    local payload="$2"
    [[ "$vmid" =~ ^[0-9]+$ ]] || { echo "--vmid requis" >&2; exit 2; }
    response="$(curl_auth -fsS -X POST "$proxy/v1/jobs" \
        -H "Content-Type: application/json" \
        -d "{\"vm_id\":$vmid,\"kind\":\"$kind\",\"vram_mib\":$vram_mib,\"payload\":$payload}")"
    job_id="$(printf '%s' "$response" | python3 -c 'import sys,json; print(json.load(sys.stdin)["job_id"])')"
    echo "$response" | json_pretty
    for _ in $(seq 1 300); do
        state_json="$(curl_auth -fsS "$proxy/v1/jobs/$job_id")"
        state="$(printf '%s' "$state_json" | python3 -c 'import sys,json; print(json.load(sys.stdin)["state"])')"
        case "$state" in
            succeeded|failed|cancelled)
                printf '%s' "$state_json" | json_pretty
                [[ "$state" == "succeeded" ]]
                exit
                ;;
        esac
        sleep 1
    done
    echo "timeout attente job $job_id" >&2
    exit 1
}

case "$cmd" in
    status)
        curl_auth -fsS "$proxy/gpu/status" | json_pretty
        ;;
    budget)
        [[ "$vmid" =~ ^[0-9]+$ ]] || { echo "--vmid requis" >&2; exit 2; }
        curl_auth -fsS -X POST "$proxy/v1/vm/$vmid/budget" \
            -H "Content-Type: application/json" \
            -d "{\"vram_budget_mib\":$vram_mib}" | json_pretty
        ;;
    matmul)
        submit_and_wait "matrix_multiply" "{\"n\":$n,\"seed\":$vmid}"
        ;;
    inference)
        if [[ -n "$model_path" ]]; then
            payload="$(python3 -c 'import json,sys; print(json.dumps({"model_path": sys.argv[1]}))' "$model_path")"
        else
            payload="{\"n\":$n,\"seed\":$vmid}"
        fi
        submit_and_wait "inference" "$payload"
        ;;
    encode)
        payload="$(python3 -c 'import json,sys; p={"codec": sys.argv[1]}; inp=sys.argv[2]; p.update({"input_path": inp} if inp else {}); print(json.dumps(p))' "$codec" "$input_path")"
        submit_and_wait "video_encode" "$payload"
        ;;
    render)
        [[ -n "$scene_path" ]] || { echo "--scene-path requis" >&2; exit 2; }
        payload="$(python3 -c 'import json,sys; print(json.dumps({"scene_path": sys.argv[1], "frame": int(sys.argv[2])}))' "$scene_path" "$frame")"
        submit_and_wait "render" "$payload"
        ;;
    custom)
        [[ -n "$command_json" ]] || { echo "--command-json requis" >&2; exit 2; }
        payload="$(python3 -c 'import json,sys; print(json.dumps({"command": json.loads(sys.argv[1])}))' "$command_json")"
        submit_and_wait "custom" "$payload"
        ;;
    ""|-h|--help)
        usage
        ;;
    *)
        echo "commande inconnue: $cmd" >&2
        usage
        exit 2
        ;;
esac
