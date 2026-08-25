.PHONY: build build-job-image build-job-images build-environments build-all dev-up dev-down bootstrap-vikunja run-local-agent

build:
	cargo build

build-job-image:
	$(MAKE) build-job-images

build-job-images:
	cargo run -p agent-hub -- build --config-root ../agent-hub-configs images

build-environments:
	cargo run -p agent-hub -- build --config-root ../agent-hub-configs environments

build-all:
	cargo run -p agent-hub -- build --config-root ../agent-hub-configs

dev-up:
	docker compose -f docker-compose.dev.yml up -d

dev-down:
	docker compose -f docker-compose.dev.yml down

bootstrap-vikunja:
	./scripts/bootstrap-vikunja.sh

run-local-agent:
	CONFIG_ROOT=../agent-hub-configs cargo run -p local-agent -- \
		--worker-id local-dev \
		--hostname localhost \
		--server http://127.0.0.1:8180 \
		--mode docker
