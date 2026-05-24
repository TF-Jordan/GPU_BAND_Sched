#!/usr/bin/env bash
# Test 36 — Concurrence CUDA stricte via omega-gpu-proxy.

source "$(dirname "$0")/lib.sh"

header "Test 36 — Concurrence GPU CUDA"
info "Ce test réutilise le test 32 en mode strict: CUDA obligatoire, plusieurs VMs, concurrence observable."

export OMEGA_GPU_PROXY_REQUIRE_CUDA="${OMEGA_GPU_PROXY_REQUIRE_CUDA:-1}"
export OMEGA_GPU_PROXY_REQUIRE_PARALLEL="${OMEGA_GPU_PROXY_REQUIRE_PARALLEL:-1}"
export OMEGA_GPU_PROXY_TEST_VM_COUNT="${OMEGA_GPU_PROXY_TEST_VM_COUNT:-3}"
export OMEGA_GPU_PROXY_TEST_MATMUL_N="${OMEGA_GPU_PROXY_TEST_MATMUL_N:-2048}"
export OMEGA_GPU_PROXY_TEST_TIMEOUT_SECS="${OMEGA_GPU_PROXY_TEST_TIMEOUT_SECS:-300}"

exec "$(dirname "$0")/32-gpu-proxy.sh" "${1:-$TEST_VMID}"
