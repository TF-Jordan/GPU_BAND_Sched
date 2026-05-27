# Makefile — omega-remote-paging
# Commandes principales pour compiler, tester et lancer les composants.

.PHONY: all build build-debug build-bridge deb test test-rust test-python test-integration \
        store-b store-c agent-demo controller-status controller-monitor \
        clean fmt clippy help

CARGO        := cargo
PYTHON       := python3
ROOT_DIR     := $(shell pwd)
TARGET_DIR   := $(ROOT_DIR)/target/release
TARGET_DBG   := $(ROOT_DIR)/target/debug

# Adresses par défaut (surchargeable via make store-b STORE_B_PORT=9200)
STORE_B_HOST ?= 127.0.0.1
STORE_B_PORT ?= 9100
STORE_C_HOST ?= 127.0.0.1
STORE_C_PORT ?= 9101
STORES       ?= $(STORE_B_HOST):$(STORE_B_PORT),$(STORE_C_HOST):$(STORE_C_PORT)
VM_ID        ?= 1
REGION_MIB   ?= 64
LOG_LEVEL    ?= info

# ─── Compilation ──────────────────────────────────────────────────────────────

all: build

## build : compile tous les binaires Rust et le bridge C en mode release
build: build-bridge
	@echo "==> Compilation release..."
	$(CARGO) build --release --workspace
	@echo "==> Binaires disponibles dans $(TARGET_DIR)/"

## build-bridge : compile omega-uffd-bridge.so (LD_PRELOAD pour QEMU)
build-bridge:
	@echo "==> Compilation omega-uffd-bridge.so..."
	$(MAKE) -C $(ROOT_DIR)/omega-uffd-bridge
	@echo "==> $(ROOT_DIR)/omega-uffd-bridge/omega-uffd-bridge.so"

## build-debug : compile en mode debug (plus rapide, avec symboles)
build-debug:
	@echo "==> Compilation debug..."
	$(CARGO) build --workspace

## deb : construit le paquet Debian release dans target/deb/
deb:
	@echo "==> Construction paquet Debian..."
	bash $(ROOT_DIR)/scripts/build-deb.sh

# ─── Tests ────────────────────────────────────────────────────────────────────

## test : lance tous les tests (Rust + Python)
test: test-rust test-python

## test-rust : tests unitaires Rust (protocole, store)
test-rust:
	@echo "==> Tests Rust..."
	$(CARGO) test --workspace

## test-python : tests unitaires du controller
test-python:
	@echo "==> Tests Python..."
	cd $(ROOT_DIR)/controller && \
		$(PYTHON) -m pytest tests/ -v

## test-integration : test d'intégration store (nécessite les binaires compilés)
test-integration: build
	@echo "==> Tests d'intégration..."
	bash $(ROOT_DIR)/tests/integration/test_full_scenario.sh

## test-protocol : tests du protocole binaire (Python)
test-protocol:
	@echo "==> Tests protocole..."
	cd $(ROOT_DIR) && $(PYTHON) -m pytest tests/protocol/ -v

# ─── Lancement des composants ─────────────────────────────────────────────────

## store-b : démarre node-bc-store sur le port STORE_B_PORT (défaut 9100)
store-b: build
	@echo "==> Démarrage store B sur $(STORE_B_HOST):$(STORE_B_PORT)"
	RUST_LOG=$(LOG_LEVEL) $(TARGET_DIR)/node-bc-store \
		--listen $(STORE_B_HOST):$(STORE_B_PORT) \
		--node-id node-b

## store-c : démarre node-bc-store sur le port STORE_C_PORT (défaut 9101)
store-c: build
	@echo "==> Démarrage store C sur $(STORE_C_HOST):$(STORE_C_PORT)"
	RUST_LOG=$(LOG_LEVEL) $(TARGET_DIR)/node-bc-store \
		--listen $(STORE_C_HOST):$(STORE_C_PORT) \
		--node-id node-c

## agent-demo : démarre l'agent en mode démo (scénario de validation)
agent-demo: build
	@echo "==> Démarrage agent en mode demo"
	@echo "    stores : $(STORES)"
	RUST_LOG=$(LOG_LEVEL) $(TARGET_DIR)/node-a-agent \
		--stores "$(STORES)" \
		--vm-id $(VM_ID) \
		--region-mib $(REGION_MIB) \
		--mode demo

## agent-daemon : démarre l'agent en mode daemon (attend SIGINT)
agent-daemon: build
	@echo "==> Démarrage agent en mode daemon"
	RUST_LOG=$(LOG_LEVEL) $(TARGET_DIR)/node-a-agent \
		--stores "$(STORES)" \
		--vm-id $(VM_ID) \
		--region-mib $(REGION_MIB) \
		--mode daemon

## controller-status : affiche le statut du cluster
controller-status:
	@echo "==> Controller status"
	cd $(ROOT_DIR)/controller && \
		$(PYTHON) -m controller.main status --stores "$(STORES)"

## controller-monitor : boucle de monitoring (10s par défaut)
controller-monitor:
	@echo "==> Controller monitoring (Ctrl+C pour arrêter)"
	cd $(ROOT_DIR)/controller && \
		$(PYTHON) -m controller.main monitor \
			--stores "$(STORES)" \
			--interval 10

## controller-policy : évalue la politique une fois
controller-policy:
	cd $(ROOT_DIR)/controller && \
		$(PYTHON) -m controller.main policy --dry-run

# ─── Scénario rapide (tout en un) ─────────────────────────────────────────────

## scenario : lance le scénario de test complet (stores + agent demo)
scenario: build
	bash $(ROOT_DIR)/scripts/test_scenario.sh

# ─── Qualité du code ──────────────────────────────────────────────────────────

## fmt : formate le code Rust
fmt:
	$(CARGO) fmt --all

## clippy : lint Rust
clippy:
	$(CARGO) clippy --workspace -- -D warnings

## fmt-python : formate le code Python (nécessite black)
fmt-python:
	cd $(ROOT_DIR)/controller && $(PYTHON) -m black controller/ tests/ 2>/dev/null || \
		echo "(black non installé — pip install black)"

# ─── Nettoyage ────────────────────────────────────────────────────────────────

## clean : supprime les artefacts de compilation
clean:
	$(CARGO) clean
	find $(ROOT_DIR)/controller -name "*.pyc" -delete
	find $(ROOT_DIR)/controller -name "__pycache__" -type d -exec rm -rf {} + 2>/dev/null || true

# ─── Installation des dépendances Python ──────────────────────────────────────

## install-python : installe les dépendances Python du controller
install-python:
	cd $(ROOT_DIR)/controller && $(PYTHON) -m pip install -r requirements.txt

# ─── Aide ─────────────────────────────────────────────────────────────────────

## help : affiche cette aide
help:
	@echo "omega-remote-paging — Commandes disponibles"
	@echo ""
	@grep -E '^## ' $(MAKEFILE_LIST) | sed 's/## /  /' | column -t -s ':'
	@echo ""
	@echo "Variables configurables :"
	@echo "  STORES=$(STORES)"
	@echo "  VM_ID=$(VM_ID)   REGION_MIB=$(REGION_MIB)   LOG_LEVEL=$(LOG_LEVEL)"
