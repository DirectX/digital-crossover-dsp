# Makefile

# Reading binary name from Cargo.toml
BIN_NAME := $(shell grep -m1 '^name' Cargo.toml | sed 's/.*= *"\(.*\)"/\1/')

PREFIX      ?= /usr/local
BINDIR      := $(PREFIX)/bin
SYSTEMD_DIR := /etc/systemd/system
SERVICE     := $(BIN_NAME).service

SERVICE_USER  ?= root
SERVICE_GROUP ?= root

.PHONY: all build install uninstall clean

all: build

build:
	cargo build --release
	@echo "✅ Build complete: target/release/$(BIN_NAME)"

install:
	@if [ ! -f target/release/$(BIN_NAME) ]; then \
		echo "❌ Binary not found. Run 'make build' first."; \
		exit 1; \
	fi
	@echo "📦 Installing $(BIN_NAME) to $(BINDIR)..."
	install -Dm755 target/release/$(BIN_NAME) $(BINDIR)/$(BIN_NAME)
	@echo "⚙️  Installing systemd service to $(SYSTEMD_DIR)/$(SERVICE)..."
	@printf '[Unit]\nDescription=%s service\nAfter=network.target shairport-sync.service\nRequires=shairport-sync.service\n\n[Service]\nType=simple\nUser=%s\nGroup=%s\nExecStart=%s/%s serve\nRestart=on-failure\nRestartSec=5\n\n[Install]\nWantedBy=multi-user.target\n' \
		"$(BIN_NAME)" "$(SERVICE_USER)" "$(SERVICE_GROUP)" "$(BINDIR)" "$(BIN_NAME)" \
		> $(SYSTEMD_DIR)/$(SERVICE)
	systemctl daemon-reload
	systemctl enable $(SERVICE)
	systemctl start $(SERVICE)
	@echo "✅ Service $(SERVICE) installed and started."
	@echo "   Use: systemctl status $(SERVICE)"

uninstall:
	@echo "🛑 Stopping and disabling $(SERVICE)..."
	-systemctl stop $(SERVICE)
	-systemctl disable $(SERVICE)
	rm -f $(SYSTEMD_DIR)/$(SERVICE)
	rm -f $(BINDIR)/$(BIN_NAME)
	systemctl daemon-reload
	@echo "✅ Uninstalled."

clean:
	cargo clean
	@echo "🧹 Cleaned."
