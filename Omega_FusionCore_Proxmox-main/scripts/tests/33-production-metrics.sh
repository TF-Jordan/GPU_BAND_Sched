#!/usr/bin/env bash
# Test 33 — Métriques production Omega

set -euo pipefail
source "$(dirname "$0")/lib.sh"

header "Test 33 — Métriques production complète"

CONTROL_PORT="${OMEGA_CONTROL_PORT:-9300}"
GPU_NODES="${OMEGA_GPU_NODES:-${OMEGA_GPU_PRIMARY_NODE:-}}"
GPU_PRIMARY="${OMEGA_GPU_PRIMARY_NODE:-${GPU_NODES%%,*}}"
GPU_PROXY_URL="${OMEGA_GPU_PROXY_URL:-${GPU_PRIMARY:+http://${GPU_PRIMARY}:9400}}"
GPU_PROXY_URL="${GPU_PROXY_URL:-http://${CONTROLLER_NODE}:9400}"
GPU_TOKEN="${OMEGA_GPU_PROXY_API_TOKEN:-}"
if [[ -z "$GPU_TOKEN" && -n "${OMEGA_GPU_PROXY_API_TOKEN_FILE:-}" && -r "${OMEGA_GPU_PROXY_API_TOKEN_FILE}" ]]; then
    GPU_TOKEN="$(tr -d ' \n\r\t' < "${OMEGA_GPU_PROXY_API_TOKEN_FILE}")"
fi
REPORT_PATH="${OMEGA_PRODUCTION_METRICS_REPORT:-/tmp/omega-production-metrics-$(date -u +%Y%m%dT%H%M%SZ).json}"
TMP_DIR="$(mktemp -d /tmp/omega-production-metrics.XXXXXX)"
_TMPFILES+=("$TMP_DIR")

require_bin curl
require_bin python3

curl_gpu() {
    if [[ -n "$GPU_TOKEN" ]]; then
        curl -H "Authorization: Bearer ${GPU_TOKEN}" "$@"
    else
        curl "$@"
    fi
}

json_path_or() {
    local file="$1" path="$2" default="${3:-?}"
    python3 - "$file" "$path" "$default" <<'PY'
import json, sys
path = sys.argv[2]
default = sys.argv[3]
try:
    with open(sys.argv[1], "r", encoding="utf-8") as f:
        cur = json.load(f)
    for part in path.split("."):
        if not part:
            continue
        if isinstance(cur, list):
            cur = cur[int(part)]
        else:
            cur = cur[part]
    print(cur)
except Exception:
    print(default)
PY
}

step "Collecte endpoints Omega par nœud"
NODE_SUMMARY="$TMP_DIR/nodes.tsv"
: > "$NODE_SUMMARY"
for node in "${OMEGA_NODES_ARR[@]}"; do
    [[ -n "$node" ]] || continue
    status_file="$TMP_DIR/omega-status-${node}.json"
    metrics_file="$TMP_DIR/store-status-${node}.json"
    active="$(ssh_run "$node" "systemctl is-active omega-daemon 2>/dev/null || true" || true)"
    http_time="$(curl -fsS -o "$status_file" -w '%{time_total}' "http://${node}:${CONTROL_PORT}/control/status" 2>/dev/null || echo "ERR")"
    store_time="$(curl -fsS -o "$metrics_file" -w '%{time_total}' "http://${node}:${STATUS_PORT}/status" 2>/dev/null || echo "ERR")"
    if [[ "$http_time" == "ERR" ]]; then
        warn "$node : API omega-daemon indisponible sur :${CONTROL_PORT}"
        printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' "$node" "$active" "ERR" "?" "?" "?" "?" "?" "$store_time" >> "$NODE_SUMMARY"
        continue
    fi
    mem_pct="$(json_path_or "$status_file" node.mem_usage_pct "?")"
    vcpu_free="$(json_path_or "$status_file" node.vcpu_free "?")"
    vcpu_total="$(json_path_or "$status_file" node.vcpu_total "?")"
    pages="$(json_path_or "$status_file" node.pages_stored "?")"
    disk_pressure="$(json_path_or "$status_file" node.disk_pressure_pct "?")"
    vm_count="$(python3 - "$status_file" <<'PY'
import json, sys
try:
    d=json.load(open(sys.argv[1], encoding="utf-8"))
    print(len(d.get("node", {}).get("local_vms", [])))
except Exception:
    print("?")
PY
)"
    printf '%s\t%s\t%s\t%s\t%s/%s\t%s\t%s\t%s\t%s\n' "$node" "$active" "$http_time" "$mem_pct" "$vcpu_free" "$vcpu_total" "$pages" "$disk_pressure" "$vm_count" "$store_time" >> "$NODE_SUMMARY"
    info "$node : omega=${active} api=${http_time}s mem=${mem_pct}% vcpu=${vcpu_free}/${vcpu_total} pages=${pages} disk_pressure=${disk_pressure}% vms=${vm_count}"
done

step "Métriques Proxmox VMs Omega"
VM_SUMMARY="$TMP_DIR/vms.tsv"
: > "$VM_SUMMARY"
pvesh get /cluster/resources --type vm --output-format json > "$TMP_DIR/pve-vms.json"
for vmid in "${TEST_VMIDS_ARR[@]}"; do
    [[ "$vmid" =~ ^[0-9]+$ ]] || continue
    python3 - "$TMP_DIR/pve-vms.json" "$vmid" <<'PY' > "$TMP_DIR/vm-${vmid}.tsv"
import json, sys
vmid=int(sys.argv[2])
data=json.load(open(sys.argv[1], encoding="utf-8"))
for v in data:
    if v.get("vmid") == vmid:
        print("\t".join(str(v.get(k, "")) for k in ("vmid", "node", "status", "cpu", "mem", "maxmem", "disk", "maxdisk")))
        break
PY
    if [[ ! -s "$TMP_DIR/vm-${vmid}.tsv" ]]; then
        warn "VM $vmid : absente de /cluster/resources"
        printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' "$vmid" "?" "missing" "?" "?" "?" "?" "?" "qga=?" >> "$VM_SUMMARY"
        continue
    fi
    vm_node_name="$(cut -f2 "$TMP_DIR/vm-${vmid}.tsv")"
    vm_node_ip="$(_pve_node_to_ip "$vm_node_name")"
    qga="no"
    if ssh_run "$vm_node_ip" "qm guest cmd '$vmid' ping >/dev/null 2>&1" >/dev/null 2>&1; then
        qga="yes"
    fi
    cat "$TMP_DIR/vm-${vmid}.tsv" | awk -v qga="$qga" '{print $0 "\tqga=" qga}' >> "$VM_SUMMARY"
    info "VM $vmid : node=${vm_node_name} status=$(cut -f3 "$TMP_DIR/vm-${vmid}.tsv") qga=${qga}"
done

step "Métriques Ceph"
CEPH_STATUS="$TMP_DIR/ceph-status.json"
CEPH_DF="$TMP_DIR/ceph-df.json"
if ceph -s --format json > "$CEPH_STATUS" 2>/tmp/omega-ceph-status.err; then
    ceph df --format json > "$CEPH_DF" 2>/dev/null || true
    ceph_health="$(json_path_or "$CEPH_STATUS" health.status "?")"
    degraded="$(json_path_or "$CEPH_STATUS" pgmap.degraded_objects "0")"
    misplaced="$(json_path_or "$CEPH_STATUS" pgmap.misplaced_objects "0")"
    pgs="$(json_path_or "$CEPH_STATUS" pgmap.num_pgs "?")"
    info "Ceph : health=${ceph_health} pgs=${pgs} degraded_objects=${degraded} misplaced_objects=${misplaced}"
else
    warn "ceph -s indisponible"
    : > "$CEPH_STATUS"
    : > "$CEPH_DF"
fi

step "Métriques I/O cgroup par VM"
IO_SUMMARY="$TMP_DIR/io.tsv"
: > "$IO_SUMMARY"
for vmid in "${TEST_VMIDS_ARR[@]}"; do
    [[ "$vmid" =~ ^[0-9]+$ ]] || continue
    vm_tsv="$TMP_DIR/vm-${vmid}.tsv"
    if [[ ! -s "$vm_tsv" ]]; then
        printf '%s\t%s\t%s\t%s\n' "$vmid" "?" "missing-vm" "?" >> "$IO_SUMMARY"
        info "VM $vmid : io.weight=missing-vm"
        continue
    fi

    vm_node_name="$(cut -f2 "$vm_tsv")"
    vm_status="$(cut -f3 "$vm_tsv")"
    if [[ "$vm_status" != "running" ]]; then
        printf '%s\t%s\t%s\t%s\n' "$vmid" "$vm_node_name" "not_applicable:${vm_status}" "not_applicable:${vm_status}" >> "$IO_SUMMARY"
        info "VM $vmid : io.weight=not_applicable (${vm_status})"
        continue
    fi

    node_ip="$(_pve_node_to_ip "$vm_node_name")"
    [[ -n "$node_ip" ]] || { printf '%s\t%s\t%s\t%s\n' "$vmid" "$vm_node_name" "missing-node-ip" "?" >> "$IO_SUMMARY"; continue; }
    io_weight="$(ssh_run "$node_ip" "p=\$(find /sys/fs/cgroup -maxdepth 6 -name io.weight \\( -path '*/${vmid}.scope/io.weight' -o -path '*/qemu-${vmid}.scope/io.weight' -o -path '*/${vmid}/io.weight' \\) 2>/dev/null | head -1); if [ -n \"\$p\" ]; then printf '%s=%s' \"\$p\" \"\$(cat \"\$p\")\"; elif [ -e /sys/fs/cgroup/qemu.slice/io.weight ]; then printf 'scope_absent;slice_default=%s' \"\$(cat /sys/fs/cgroup/qemu.slice/io.weight)\"; else echo absent; fi" 2>/dev/null || echo "ssh-error")"
    io_stat="$(ssh_run "$node_ip" "p=\$(find /sys/fs/cgroup -maxdepth 6 -name io.weight \\( -path '*/${vmid}.scope/io.weight' -o -path '*/qemu-${vmid}.scope/io.weight' -o -path '*/${vmid}/io.weight' \\) 2>/dev/null | head -1); s=\${p%/io.weight}/io.stat; if [ -n \"\$p\" ] && [ -s \"\$s\" ]; then head -1 \"\$s\"; elif [ -n \"\$p\" ]; then echo empty; elif [ -e /sys/fs/cgroup/qemu.slice/io.weight ]; then echo scope_absent; else echo absent; fi" 2>/dev/null || echo "ssh-error")"
    printf '%s\t%s\t%s\t%s\n' "$vmid" "$node_ip" "$io_weight" "$io_stat" >> "$IO_SUMMARY"
    info "VM $vmid : node=${vm_node_name} io.weight=${io_weight}"
done

step "Métriques GPU proxy et micro-benchmark CUDA"
GPU_STATUS="$TMP_DIR/gpu-status.json"
GPU_NODES_STATUS="$TMP_DIR/gpu-nodes-status.jsonl"
GPU_METRICS="$TMP_DIR/gpu-metrics.prom"
GPU_RESULT="$TMP_DIR/gpu-benchmark.json"
: > "$GPU_NODES_STATUS"
if [[ -n "$GPU_NODES" ]]; then
    for gpu_node in ${GPU_NODES//,/ }; do
        [[ -n "$gpu_node" ]] || continue
        node_url="http://${gpu_node}:9400"
        if curl_gpu -fsS "$node_url/gpu/status" > "$TMP_DIR/gpu-status-${gpu_node}.json" 2>/dev/null; then
            python3 - "$gpu_node" "$node_url" "$TMP_DIR/gpu-status-${gpu_node}.json" <<'PY' >> "$GPU_NODES_STATUS"
import json, sys
d=json.load(open(sys.argv[3], encoding="utf-8"))
d["_node_addr"]=sys.argv[1]
d["_proxy_url"]=sys.argv[2]
print(json.dumps(d, sort_keys=True))
PY
            node_total="$(json_path_or "$TMP_DIR/gpu-status-${gpu_node}.json" total_vram_mib "0")"
            node_free="$(json_path_or "$TMP_DIR/gpu-status-${gpu_node}.json" free_vram_mib "0")"
            info "GPU node ${gpu_node}: proxy OK vram=${node_free}/${node_total}MiB"
        else
            warn "GPU node ${gpu_node}: proxy indisponible sur ${node_url}"
        fi
    done
fi
if curl_gpu -fsS "$GPU_PROXY_URL/gpu/status" > "$GPU_STATUS" 2>/tmp/omega-gpu-status.err; then
    curl_gpu -fsS "$GPU_PROXY_URL/metrics" > "$GPU_METRICS" 2>/dev/null || true
    gpu_total="$(json_path_or "$GPU_STATUS" total_vram_mib "0")"
    gpu_free="$(json_path_or "$GPU_STATUS" free_vram_mib "0")"
    gpu_concurrency="$(json_path_or "$GPU_STATUS" max_concurrent_jobs "?")"
    info "GPU proxy : url=${GPU_PROXY_URL} vram=${gpu_free}/${gpu_total}MiB concurrence=${gpu_concurrency}"

    bench_vmid=""
    for vmid in "${TEST_VMIDS_ARR[@]}"; do
        [[ "$vmid" =~ ^[0-9]+$ ]] || continue
        vm_tsv="$TMP_DIR/vm-${vmid}.tsv"
        [[ -s "$vm_tsv" ]] || continue
        [[ "$(cut -f3 "$vm_tsv")" == "running" ]] || continue
        bench_vmid="$vmid"
        break
    done
    bench_vmid="${bench_vmid:-${TEST_VMIDS_ARR[0]:-$TEST_VMID}}"
    bench_n="${OMEGA_PRODUCTION_GPU_BENCH_N:-1024}"
    bench_vram="${OMEGA_PRODUCTION_GPU_BENCH_VRAM_MIB:-128}"
    budget_resp="$(curl_gpu -sS -w '\n%{http_code}' -X POST "$GPU_PROXY_URL/v1/vm/${bench_vmid}/budget" \
        -H "Content-Type: application/json" \
        -d "{\"vram_budget_mib\":$((bench_vram * 2))}" 2>/tmp/omega-gpu-bench-budget.err || true)"
    budget_code="$(printf '%s\n' "$budget_resp" | tail -n1)"
    budget_body="$(printf '%s\n' "$budget_resp" | sed '$d')"
    printf '%s' "$budget_body" >/tmp/omega-gpu-bench-budget.json
    job_resp="$(curl_gpu -sS -w '\n%{http_code}' -X POST "$GPU_PROXY_URL/v1/jobs" \
        -H "Content-Type: application/json" \
        -d "{\"vm_id\":${bench_vmid},\"kind\":\"matrix_multiply\",\"vram_mib\":${bench_vram},\"payload\":{\"n\":${bench_n},\"seed\":33033,\"require_cuda\":true}}" 2>/tmp/omega-gpu-bench.err || true)"
    job_code="$(printf '%s\n' "$job_resp" | tail -n1)"
    job_json="$(printf '%s\n' "$job_resp" | sed '$d')"
    job_id="$(printf '%s' "$job_json" | python3 -c 'import json,sys; print(json.load(sys.stdin).get("job_id",""))' 2>/dev/null || true)"
    if [[ -n "$job_id" ]]; then
        deadline=$((SECONDS + ${OMEGA_PRODUCTION_GPU_BENCH_TIMEOUT_SECS:-240}))
        while [[ "$SECONDS" -lt "$deadline" ]]; do
            curl_gpu -fsS "$GPU_PROXY_URL/v1/jobs/${job_id}" > "$GPU_RESULT" 2>/dev/null || true
            state="$(json_path_or "$GPU_RESULT" state "?")"
            [[ "$state" == "succeeded" || "$state" == "failed" || "$state" == "cancelled" ]] && break
            sleep 1
        done
        backend="$(json_path_or "$GPU_RESULT" result.output.backend "?")"
        device="$(json_path_or "$GPU_RESULT" result.output.device "?")"
        duration_ms="$(json_path_or "$GPU_RESULT" result.output.duration_ms "?")"
        total_ms="$(json_path_or "$GPU_RESULT" duration_ms "?")"
        info "GPU bench : n=${bench_n} state=${state:-?} backend=${backend} device=${device} worker_ms=${duration_ms} total_ms=${total_ms}"
    else
        warn "GPU bench non soumis: budget_http=${budget_code:-?} budget_body=${budget_body:-<vide>} budget_err=$(tr '\n' ' ' </tmp/omega-gpu-bench-budget.err 2>/dev/null || true) submit_http=${job_code:-?} submit_err=$(tr '\n' ' ' </tmp/omega-gpu-bench.err 2>/dev/null || true) response=${job_json:-<vide>}"
        : > "$GPU_RESULT"
    fi
else
    warn "GPU proxy indisponible: ${GPU_PROXY_URL}"
    : > "$GPU_STATUS"
    : > "$GPU_METRICS"
    : > "$GPU_RESULT"
fi

step "Rapport JSON"
python3 - "$REPORT_PATH" "$TMP_DIR" <<'PY'
import json, pathlib, sys
report = pathlib.Path(sys.argv[1])
tmp = pathlib.Path(sys.argv[2])

def read_json(name):
    p = tmp / name
    try:
        return json.loads(p.read_text(encoding="utf-8"))
    except Exception:
        return None

def read_tsv(name):
    p = tmp / name
    if not p.exists():
        return []
    return [line.rstrip("\n").split("\t") for line in p.read_text(encoding="utf-8").splitlines() if line.strip()]

data = {
    "kind": "omega-production-metrics",
    "nodes": read_tsv("nodes.tsv"),
    "vms": read_tsv("vms.tsv"),
    "io": read_tsv("io.tsv"),
    "ceph_status": read_json("ceph-status.json"),
    "ceph_df": read_json("ceph-df.json"),
    "gpu_nodes_status": [json.loads(line) for line in (tmp / "gpu-nodes-status.jsonl").read_text(encoding="utf-8").splitlines() if line.strip()] if (tmp / "gpu-nodes-status.jsonl").exists() else [],
    "gpu_status": read_json("gpu-status.json"),
    "gpu_benchmark": read_json("gpu-benchmark.json"),
}
report.write_text(json.dumps(data, indent=2, sort_keys=True), encoding="utf-8")
print(report)
PY
pass "Métriques production collectées: ${REPORT_PATH}"
