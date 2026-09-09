pub mod artifact_store;
pub mod auth;
pub mod config;
pub mod environment;
pub mod github_poller;
pub mod job_events;
pub mod oauth;
pub mod orchestration;
pub mod repo_discovery;
pub mod scm;
pub mod task_adapter;
pub mod task_files;
#[allow(dead_code)]
pub mod task_vikunja;
pub mod web;
pub mod workflows;

use std::sync::Arc;

use axum::Router;
use axum::middleware::{from_fn, from_fn_with_state};
use axum::routing::{get, post};
use tower_sessions::{MemoryStore, SessionManagerLayer};

use artifact_store::{ArtifactStore, DirectoryStore};
use config::{AuthMode, ServerConfig};
use github_poller::GitHubPoller;
use job_events::JobEventHub;
use oauth::OAuthManager;
use orchestration::OrchestrationService;
use repo_discovery::discover_repositories_from_config;
use task_adapter::TaskAdapter;
use workflows::WorkflowCatalog;

pub(super) const DEFAULT_WEB_HOME: &str = "/orchestration";

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<ServerConfig>,
    pub artifact_store: Arc<dyn ArtifactStore>,
    pub orchestration: Arc<OrchestrationService>,
    pub job_events: Arc<JobEventHub>,
    pub oauth: Arc<Option<OAuthManager>>,
}

pub fn router(state: AppState) -> Router {
    let api_v1 = Router::new()
        .route(
            "/orchestration/work-items",
            get(orchestration::handlers::list_work_items)
                .post(orchestration::handlers::create_work_item),
        )
        .route(
            "/orchestration/workflows",
            get(orchestration::handlers::list_workflows),
        )
        .route(
            "/orchestration/workflows/validate",
            get(orchestration::handlers::validate_workflows),
        )
        .route(
            "/orchestration/github/webhook",
            post(orchestration::webhook::github_webhook),
        )
        .route(
            "/orchestration/work-items/{id}/events",
            post(orchestration::handlers::fire_work_item_event),
        )
        .route(
            "/orchestration/work-items/{id}/event",
            post(orchestration::handlers::fire_work_item_event),
        )
        .route(
            "/orchestration/jobs",
            get(orchestration::handlers::list_jobs),
        )
        .route(
            "/orchestration/jobs/{id}",
            get(orchestration::handlers::get_job).put(orchestration::handlers::update_job),
        )
        .route(
            "/orchestration/jobs/{id}/events",
            get(orchestration::handlers::job_events_sse),
        )
        .route(
            "/orchestration/jobs/{id}/cancellation-command",
            get(orchestration::handlers::get_job_cancellation_command),
        )
        .route(
            "/orchestration/workers",
            get(orchestration::handlers::list_workers)
                .post(orchestration::handlers::register_worker),
        )
        .route(
            "/orchestration/workers/{id}/heartbeat",
            post(orchestration::handlers::heartbeat_worker),
        )
        .route(
            "/orchestration/poll",
            post(orchestration::handlers::poll_job),
        )
        .route(
            "/orchestration/dispatch",
            post(orchestration::handlers::dispatch),
        );

    let api_v1 = if state.config.auth_mode == AuthMode::None {
        api_v1.with_state(state.clone())
    } else {
        api_v1
            .layer(from_fn_with_state(state.clone(), auth::require_auth))
            .with_state(state.clone())
    };

    let mut app = Router::new()
        .nest("/api/v1", api_v1)
        .route("/health", get(health))
        .route(
            "/",
            get(|| async { axum::response::Redirect::permanent(DEFAULT_WEB_HOME) }),
        );

    let mut web_routes = Router::new()
        .route("/orchestration", get(web::orchestration_dashboard))
        .route(
            "/orchestration/work-items",
            get(web::orchestration_work_items),
        )
        .route("/orchestration/jobs", get(web::orchestration_jobs))
        .route(
            "/orchestration/transcripts",
            get(web::orchestration_transcripts),
        )
        .route("/orchestration/workers", get(web::orchestration_workers))
        .route(
            "/orchestration/work-items/{id}",
            get(web::orchestration_work_item_detail),
        )
        .route(
            "/orchestration/jobs/{id}",
            get(web::orchestration_job_detail),
        )
        .route(
            "/orchestration/jobs/{id}/artifacts/{key}",
            get(web::orchestration_job_artifact),
        )
        .route(
            "/orchestration/work-items/{id}/action",
            post(web::orchestration_fire_work_item_action),
        );

    if state.config.auth_mode != AuthMode::None {
        let auth_routes = Router::new()
            .route("/auth/login", get(web::login_page))
            .route("/auth/login/basic", post(web::basic_auth_login))
            .route("/auth/start/{provider}", get(oauth::start_auth))
            .route("/auth/callback/{provider}", get(oauth::callback))
            .route("/auth/logout", post(oauth::logout))
            .with_state(state.clone());

        web_routes = web_routes.layer(from_fn(auth::require_web_auth));
        app = app.merge(auth_routes);
    }

    let session_layer = SessionManagerLayer::new(MemoryStore::default());
    app.merge(web_routes.with_state(state))
        .layer(session_layer)
        .layer(tower_http::trace::TraceLayer::new_for_http())
}

async fn health() -> &'static str {
    "ok"
}

impl AppState {
    pub fn new(server_config: ServerConfig, oauth: Option<OAuthManager>) -> Self {
        let artifact_store: Arc<dyn ArtifactStore> = Arc::new(DirectoryStore::new(
            std::path::PathBuf::from(server_config.artifact_store_root.clone()),
        ));
        let loaded = config::load_config_tree(std::path::Path::new(&server_config.config_root))
            .unwrap_or_else(|error| {
                panic!(
                    "failed to load config tree from CONFIG_ROOT ({}): {error:?}",
                    server_config.config_root
                )
            });
        let system_operator_login = loaded.system.operator.as_str().trim().to_string();
        let system_operator_login =
            (!system_operator_login.is_empty()).then_some(system_operator_login);
        let installed_workflows = loaded
            .system
            .workflows
            .as_ref()
            .map(|workflows| workflows.installed.as_slice());
        let workflows = Arc::new(
            WorkflowCatalog::load_from_dir_with_installed(
                &server_config.workflows_dir,
                installed_workflows,
            )
            .unwrap_or_else(|error| panic!("failed to load installed workflows: {error:#}")),
        );
        let task_adapter: Arc<dyn TaskAdapter> = match server_config.task_adapter.as_str() {
            "file" => {
                let store = task_files::TaskFileStore::new_with_state_dir(
                    server_config.task_dir.clone(),
                    server_config.task_state_dir.clone(),
                );
                store.ensure_dir().expect("create task dir");
                Arc::new(store)
            }
            other => panic!("unsupported TASK_ADAPTER '{other}'; only exact 'file' is allowed"),
        };
        let orchestration_db_path = if server_config.orchestration_db_path == ":memory:" {
            std::env::temp_dir()
                .join(format!(
                    "orchestration-test-{}.sqlite3",
                    uuid::Uuid::new_v4().simple()
                ))
                .to_string_lossy()
                .into_owned()
        } else {
            server_config.orchestration_db_path.clone()
        };
        let scm_registry = scm::ScmHostRegistry::from_configs(&loaded.system.scm_hosts)
            .unwrap_or_else(|error| panic!("invalid [[system.scm_hosts]] config: {error}"));
        let github_poller = if server_config.github_polling_enabled && !scm_registry.is_empty() {
            Some(Arc::new(GitHubPoller::new(scm_registry.clone())))
        } else {
            None
        };
        let environments: Vec<protocol::EnvironmentDefinition> = loaded
            .environments
            .iter()
            .map(|environment| environment.to_definition())
            .collect();
        let overlays = loaded.overlays.clone();
        let triage_prompt = loaded.triage_prompt.clone();
        let runner_profiles = loaded.runner_profiles.clone();
        let environment_snapshots = loaded
            .environments
            .iter()
            .map(|environment| {
                let bundle = loaded
                    .environment_bundles
                    .get(&environment.toml.name)
                    .unwrap_or_else(|| {
                        panic!(
                            "validated environment bundle '{}' is unavailable",
                            environment.toml.name
                        )
                    });
                let mut policies = Vec::new();
                let mut policy_name = Some(environment.toml.network_policy.as_str());
                let mut visited = std::collections::HashSet::new();
                while let Some(name) = policy_name {
                    if !visited.insert(name.to_string()) {
                        panic!("network policy inheritance cycle at '{name}'");
                    }
                    let policy = loaded
                        .network_policies
                        .iter()
                        .find(|policy| policy.name == name)
                        .unwrap_or_else(|| panic!("environment references missing policy '{name}'"));
                    policies.push(policy.clone());
                    policy_name = policy.extends.as_deref();
                }
                (
                    environment.toml.name.clone(),
                    serde_json::json!({
                        "environment": environment.toml,
                        "network_policies": &policies,
                        "bundle_digest": bundle.digest,
                        "network_policy_bundle_digests": policies.iter().map(|policy| {
                            let digests = loaded
                                .network_policy_file_digests
                                .get(&policy.name)
                                .unwrap_or_else(|| panic!("validated digest for network policy '{}' is unavailable", policy.name))
                                .clone();
                            (policy.name.clone(), digests)
                        }).collect::<std::collections::BTreeMap<_, _>>(),
                    }),
                )
            })
            .collect();
        let discovered_repos = discover_repositories_from_config(
            loaded.system.repo_discovery.as_ref(),
            &loaded.triage_mounts,
        )
        .unwrap_or_else(|error| {
            panic!("failed to discover repositories from system.repo_discovery: {error:#}")
        });
        let approval_client = server_config
            .approvals_server
            .as_deref()
            .map(|endpoint| {
                agent_hub_approval_client::ApprovalClient::new(
                    endpoint,
                    server_config.approvals_token.clone(),
                )
                .unwrap_or_else(|error| panic!("invalid approvals service endpoint: {error}"))
            })
            .map(Arc::new);
        if approval_client.is_none()
            && environments
                .iter()
                .any(|environment: &protocol::EnvironmentDefinition| {
                    environment.approval_mode.eq_ignore_ascii_case("gateway")
                })
        {
            panic!(
                "AGENT_HUB_APPROVALS_SERVER is required by an installed gateway-approval environment"
            );
        }
        let orchestration = Arc::new(
            OrchestrationService::new_with_github_poller_and_interval(
                Arc::clone(&workflows),
                task_adapter,
                orchestration_db_path,
                github_poller,
                server_config.github_poll_interval_secs,
            )
            .with_environments(environments)
            .with_overlays(overlays)
            .with_discovered_repos(discovered_repos)
            .with_operator_login(system_operator_login)
            .with_runner_profiles(runner_profiles)
            .with_scm_hosts(scm_registry)
            .with_environment_snapshots(environment_snapshots)
            .with_triage_prompt(triage_prompt)
            .with_approval_client(approval_client),
        );

        Self {
            config: Arc::new(server_config),
            artifact_store,
            orchestration,
            job_events: Arc::new(JobEventHub::new()),
            oauth: Arc::new(oauth),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Method, Request, StatusCode};
    use tower::ServiceExt;

    async fn test_state() -> AppState {
        let root = std::env::temp_dir().join(format!("api-router-{}", uuid::Uuid::new_v4()));
        let tasks = root.join("tasks");
        std::fs::create_dir_all(&tasks).unwrap();
        let task_adapter = Arc::new(task_files::TaskFileStore::new(tasks.clone()))
            as Arc<dyn task_adapter::TaskAdapter>;
        let orchestration = Arc::new(OrchestrationService::new(
            Arc::new(WorkflowCatalog::empty_for_test()),
            task_adapter,
            root.join("orchestration.sqlite3")
                .to_string_lossy()
                .into_owned(),
        ));
        orchestration.init().await.unwrap();
        AppState {
            config: Arc::new(ServerConfig {
                auth_mode: AuthMode::None,
                tokens: Vec::new(),
                listen_addr: "127.0.0.1:0".into(),
                base_url: None,
                approvals_server: None,
                approvals_token: None,
                workflows_dir: root.join("workflows").to_string_lossy().into_owned(),
                config_root: root.join("config").to_string_lossy().into_owned(),
                task_dir: tasks.to_string_lossy().into_owned(),
                task_state_dir: root.join("task-state").to_string_lossy().into_owned(),
                task_adapter: "file".into(),
                vikunja_base_url: None,
                vikunja_token: None,
                vikunja_bot_username: None,
                vikunja_bot_password: None,
                vikunja_project_id: None,
                vikunja_label_prefix: None,
                orchestration_db_path: root
                    .join("orchestration.sqlite3")
                    .to_string_lossy()
                    .into_owned(),
                artifact_store_root: root.join("artifacts").to_string_lossy().into_owned(),
                grpc_listen_addr: None,
                github_webhook_secret: None,
                github_polling_enabled: false,
                github_poll_interval_secs: 30,
            }),
            artifact_store: Arc::new(DirectoryStore::new(root.join("artifacts"))),
            orchestration,
            job_events: Arc::new(JobEventHub::new()),
            oauth: Arc::new(None),
        }
    }

    #[tokio::test]
    async fn approval_service_routes_are_not_exposed() {
        let app = router(test_state().await);
        for (method, path) in [
            (Method::POST, "/api/v1/hooks/approval"),
            (Method::POST, "/api/v1/hooks/question"),
            (Method::GET, "/api/v1/approvals/pending"),
            (Method::GET, "/api/v1/questions/pending"),
            (Method::GET, "/api/v1/sessions"),
            (Method::GET, "/api/v1/config"),
            (Method::GET, "/api/v1/mcp"),
            (Method::GET, "/approvals"),
        ] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri(path)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
        }
    }

    #[tokio::test]
    async fn orchestration_api_and_admin_ui_remain_exposed() {
        let app = router(test_state().await);
        for path in [
            "/api/v1/orchestration/work-items",
            "/api/v1/orchestration/jobs",
            "/api/v1/orchestration/workers",
            "/orchestration",
        ] {
            let response = app
                .clone()
                .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{path}");
        }
    }
}
