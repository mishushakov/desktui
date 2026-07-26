# desktui
#
# `make desktop` brings up a VNC-served XFCE desktop in Docker and prints the
# command to connect to it. Everything else is the usual cargo work.

SHELL := /bin/bash

IMAGE       ?= desktui-desktop
CONTAINER   ?= desktui-desktop
VNC_PORT    ?= 5901
# 127.0.0.1 on purpose. VNC password authentication is DES with an eight character
# key and the session that follows is unencrypted, so this stays on the loopback
# interface; reach a remote one through an ssh tunnel instead.
BIND_ADDR   ?= 127.0.0.1
GEOMETRY    ?= 1280x800
VNC_PASSWORD ?= desktui
SERVER      ?= localhost::$(VNC_PORT)

.DEFAULT_GOAL := help

## help: list the targets
help:
	@printf '\ndesktui\n\n'
	@grep -E '^## ' $(MAKEFILE_LIST) | sed 's/^## /  /' | awk -F': *' '{printf "  \033[36m%-16s\033[0m %s\n", $$1, $$2}' | sed 's/^    //'
	@printf '\nConnecting:\n'
	@printf '  make desktop && make run\n\n'

# ------------------------------------------------------------------ the desktop

## desktop: build and start the VNC desktop container
desktop: desktop-build desktop-start

## desktop-build: build the container image
desktop-build:
	docker build -t $(IMAGE) docker/

## desktop-start: start the container and wait for the VNC port
desktop-start:
	@if docker ps -a --format '{{.Names}}' | grep -qx '$(CONTAINER)'; then \
		printf 'removing the previous container\n'; \
		docker rm -f $(CONTAINER) >/dev/null; \
	fi
	@docker run -d --name $(CONTAINER) \
		-p $(BIND_ADDR):$(VNC_PORT):5901 \
		-e VNC_PASSWORD='$(VNC_PASSWORD)' \
		-e VNC_GEOMETRY='$(GEOMETRY)' \
		--shm-size 256m \
		$(IMAGE) >/dev/null
	@printf 'waiting for the desktop'
	@for i in $$(seq 1 60); do \
		if docker logs $(CONTAINER) 2>&1 | grep -q 'vnc ready'; then \
			printf ' up\n'; \
			break; \
		fi; \
		if ! docker ps --format '{{.Names}}' | grep -qx '$(CONTAINER)'; then \
			printf '\n'; \
			printf 'the container exited; last output:\n'; \
			docker logs --tail 40 $(CONTAINER); \
			exit 1; \
		fi; \
		printf '.'; \
		sleep 1; \
	done
	@printf '\n  VNC on $(BIND_ADDR):$(VNC_PORT), password "$(VNC_PASSWORD)", desktop $(GEOMETRY)\n'
	@printf '  connect with:  make run\n'
	@printf '  stop it with:  make desktop-stop\n\n'

## desktop-stop: stop and remove the container
desktop-stop:
	@docker rm -f $(CONTAINER) >/dev/null 2>&1 && printf 'stopped\n' || printf 'not running\n'

## desktop-logs: follow the container's output
desktop-logs:
	docker logs -f $(CONTAINER)

## desktop-shell: a shell inside the container
desktop-shell:
	docker exec -it $(CONTAINER) bash

## desktop-status: is it up, and is the port answering?
desktop-status:
	@docker ps --filter name=$(CONTAINER) --format '  {{.Names}}  {{.Status}}  {{.Ports}}' || true
	@if nc -z $(BIND_ADDR) $(VNC_PORT) 2>/dev/null; then \
		printf '  port $(VNC_PORT) is open\n'; \
	else \
		printf '  port $(VNC_PORT) is not answering\n'; \
	fi

# ---------------------------------------------------------------------- the client

## run: connect to the desktop (needs Ghostty, kitty or WezTerm)
run:
	VNC_PASSWORD='$(VNC_PASSWORD)' cargo run --release -- --quality 6 --compression 2 $(SERVER)

## caps: report what the current terminal supports
caps:
	cargo run --release -- --print-caps

## pattern: render the test pattern, no server needed
pattern:
	cargo run --release -- --test-pattern

# ------------------------------------------------------------------------- checks

## test: every suite that needs neither a server nor a real terminal
test:
	cargo test
	@printf '\nNot run by the above, and how to run each:\n'
	@printf '  \033[36m%-16s\033[0m %s\n' 'make test-live'   'needs the desktop container (make desktop)'
	@printf '  \033[36m%-16s\033[0m %s\n\n' 'make perf'      'timings, which are not pass/fail'

## test-live: end-to-end against the running desktop container
test-live:
	@docker ps --format '{{.Names}}' | grep -qx '$(CONTAINER)' || { \
		printf 'the desktop is not running; start it with: make desktop\n' >&2; \
		exit 1; \
	}
	DESKTUI_TEST_SERVER='$(SERVER)' DESKTUI_TEST_PASSWORD='$(VNC_PASSWORD)' \
		cargo test --test live -- --ignored --test-threads=1

## perf: time the compose pipeline
perf:
	cargo test --release --test perf -- --ignored --nocapture

## check: fmt, clippy and tests
check:
	cargo fmt --check
	cargo clippy --all-targets -- -D warnings
	cargo test

## build: release binary
build:
	cargo build --release

.PHONY: help desktop desktop-build desktop-start desktop-stop desktop-logs \
	desktop-shell desktop-status run caps pattern test test-live perf \
	check build
