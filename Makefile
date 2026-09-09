.PHONY: build build-job-image build-job-images build-environments build-all dev-up dev-down bootstrap-vikunja run-local-agent test-approval-client-fast test-approval-service-fast test-server-fast test-api-fast test-gateway-fast test-gateway-network test-executor-slow test-local-agent-faults

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

test-approval-client-fast:
	cargo test -p agent-hub-approval-client

test-approval-service-fast:
	cargo test -p agent-hub-approval-service

test-server-fast:
	cargo test -p agent-hub-server

test-api-fast:
	cargo test -p agent-hub-server server::tests

test-gateway-fast:
	cargo test -p agent-hub-gateway

test-gateway-network:
	cargo test -p agent-hub-gateway --test agent_hub_run_cli -- --ignored
	cargo test -p agent-hub-gateway --test http_outcomes -- --ignored

test-executor-slow:
	cargo test -p agent-hub-executor --test executor_contract -- --ignored

test-local-agent-faults:
	cargo test -p local-agent tests::cleanup_running_after_hook_observes_persisted_phase_before_cleanup -- --ignored --exact
	cargo test -p local-agent tests::reconciled_workspace_preparing_enters_persisted_cleanup_before_reporting -- --ignored --exact
	cargo test -p local-agent tests::event_publication_completion_fault_replays_after_acknowledged_delivery -- --ignored --exact
	cargo test -p local-agent tests::central_execution_fault_hook_targets_every_lifecycle_and_operation_boundary -- --ignored --exact
	cargo test -p local-agent tests::central_execution_pause_hook_uses_ready_and_release_files -- --ignored --exact
