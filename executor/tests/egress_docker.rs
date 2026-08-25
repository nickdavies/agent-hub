use std::path::{Path, PathBuf};

use agent_hub_executor::egress::{
    PreparationBoundary, ProxyRuntimeSpec, ResolverRuntimeSpec, RestrictedEgressRequest,
    RestrictedPolicy,
};
use agent_hub_executor::{Cancellation, Deadline};

const JOB_IMAGE: &str = "agent-hub/job-code:2026-08-24";
const SQUID_IMAGE: &str =
    "ubuntu/squid@sha256:6a097f68bae708cedbabd6188d68c7e2e7a38cedd05a176e1cc0ba29e3bbe029";
const FIXTURE_LABEL: &str = "agent-hub.reddit.com/executor-fixture";
const FIXTURE_NAMESPACE_LABEL: &str = "agent-hub.reddit.com/executor-fixture-namespace";
const FIXTURE_KIND_LABEL: &str = "agent-hub.reddit.com/executor-fixture-kind";

#[tokio::test]
#[ignore = "requires the pinned local Docker images and daemon"]
async fn real_guarded_public_https_and_direct_bypass_smoke() {
    let docker = Path::new("docker");
    let root = test_root("public-smoke");
    let namespace = format!("smoke-{}", &uuid::Uuid::new_v4().simple().to_string()[..16]);
    let (_sender, receiver) = tokio::sync::watch::channel(false);
    let deadline = Deadline::new(Some(120)).unwrap();
    let mut prepared = RestrictedEgressRequest {
        namespace: namespace.clone(),
        state_dir: root.clone(),
        proxy: ProxyRuntimeSpec {
            image: SQUID_IMAGE.to_string(),
            listen_port: 3128,
            uid: 13,
            gid: 13,
        },
        resolver: ResolverRuntimeSpec::docker_embedded(JOB_IMAGE, 100, 101),
        policies: vec![RestrictedPolicy::GuardedWeb],
        cancellation: Cancellation::new(receiver),
        external_subnets: Vec::new(),
        failure_after: None,
    }
    .prepare(docker, &deadline)
    .await
    .expect("prepare guarded executor egress");

    let args = vec![
        "run".to_string(),
        "--rm".to_string(),
        format!("--network={}", prepared.runtime.network_name),
        "--dns=127.0.0.1".to_string(),
        format!(
            "--add-host={}:{}",
            prepared.runtime.proxy_host, prepared.runtime.proxy_ip
        ),
        format!("-e=HTTPS_PROXY={}", prepared.runtime.proxy_url),
        format!("-e=HTTP_PROXY={}", prepared.runtime.proxy_url),
        format!("-e=https_proxy={}", prepared.runtime.proxy_url),
        format!("-e=http_proxy={}", prepared.runtime.proxy_url),
        "-e=ALL_PROXY=".to_string(),
        "-e=all_proxy=".to_string(),
        "-e=NO_PROXY=localhost,127.0.0.1,::1,.local".to_string(),
        "--entrypoint=sh".to_string(),
        JOB_IMAGE.to_string(),
        "-ceu".to_string(),
        r#"
python3 - <<'PY'
import socket
s = socket.create_connection(("agent-hub-proxy", 3128), 3)
s.close()
PY
curl --fail --verbose --connect-timeout 15 --max-time 45 -o /dev/null https://example.com/
if getent hosts example.com >/dev/null 2>&1; then exit 21; fi
if env -u HTTP_PROXY -u HTTPS_PROXY -u ALL_PROXY -u http_proxy -u https_proxy -u all_proxy curl -ksS --connect-timeout 3 --max-time 8 -o /dev/null https://1.1.1.1/; then exit 22; fi
for url in http://127.0.0.1/ http://2130706433/ http://0x7f000001/ http://169.254.169.254/latest/meta-data/ 'http://[::ffff:127.0.0.1]/'; do
  if NO_PROXY= no_proxy= curl --fail -ksS --connect-timeout 3 --max-time 8 -o /dev/null "$url"; then exit 23; fi
done
"#
        .to_string(),
    ];
    let output = tokio::process::Command::new(docker)
        .args(&args)
        .output()
        .await
        .expect("run guarded public smoke");
    let proxy_logs = tokio::process::Command::new(docker)
        .args(["logs", &format!("agent-hub-executor-{namespace}-proxy")])
        .output()
        .await
        .unwrap();
    let resolver_logs = tokio::process::Command::new(docker)
        .args(["logs", &format!("agent-hub-executor-{namespace}-resolver")])
        .output()
        .await
        .unwrap();
    let cleanup = prepared.cleanup(&Deadline::new(Some(30)).unwrap()).await;
    assert!(
        output.status.success(),
        "guarded public smoke failed: stdout={} stderr={} proxy={} resolver_stdout={} resolver_stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&proxy_logs.stderr),
        String::from_utf8_lossy(&resolver_logs.stdout),
        String::from_utf8_lossy(&resolver_logs.stderr),
    );
    cleanup.expect("cleanup guarded public smoke");
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
#[ignore = "requires the pinned local Docker images and daemon"]
async fn real_exact_resource_host_subset_smoke() {
    let docker = Path::new("docker");
    let root = test_root("exact-hosts");
    let namespace = format!("hosts-{}", &uuid::Uuid::new_v4().simple().to_string()[..16]);
    let (_sender, receiver) = tokio::sync::watch::channel(false);
    let mut prepared = RestrictedEgressRequest {
        namespace,
        state_dir: root.clone(),
        proxy: ProxyRuntimeSpec {
            image: SQUID_IMAGE.to_string(),
            listen_port: 3128,
            uid: 13,
            gid: 13,
        },
        resolver: ResolverRuntimeSpec::docker_embedded(JOB_IMAGE, 100, 101),
        policies: vec![RestrictedPolicy::Hosts(vec!["example.com".to_string()])],
        cancellation: Cancellation::new(receiver),
        external_subnets: Vec::new(),
        failure_after: None,
    }
    .prepare(docker, &Deadline::new(Some(120)).unwrap())
    .await
    .expect("prepare exact resource host egress");
    let operation = run_model(
        docker,
        &prepared,
        r#"
curl -fsS --connect-timeout 10 --max-time 30 -o /dev/null https://example.com/
if curl -fsS --connect-timeout 3 --max-time 8 -o /dev/null https://www.cloudflare.com/; then exit 50; fi
"#,
    )
    .await;
    let cleanup = prepared.cleanup(&Deadline::new(Some(30)).unwrap()).await;
    let _ = std::fs::remove_dir_all(root);
    operation.unwrap();
    cleanup.unwrap();
}

#[tokio::test]
#[ignore = "requires the pinned local Docker images and daemon"]
async fn deterministic_guarded_dns_rebinding_and_route_fixtures() {
    let docker = Path::new("docker");
    let root = test_root("deterministic");
    let evidence = root.join("evidence");
    std::fs::create_dir(&evidence).unwrap();
    std::fs::write(evidence.join("trap.log"), b"").unwrap();
    let namespace = format!(
        "fixture-{}",
        &uuid::Uuid::new_v4().simple().to_string()[..12]
    );
    let subnet_id = 20 + u8::from_str_radix(&namespace[8..10], 16).unwrap() % 100;
    let public_subnet = format!("23.{subnet_id}.0.0/24");
    let private_subnet = format!("10.{subnet_id}.0.0/24");
    let dns_ip = format!("23.{subnet_id}.0.53");
    let public_ip = format!("23.{subnet_id}.0.80");
    let private_ip = format!("10.{subnet_id}.0.80");
    let external_network = format!("agent-hub-executor-{namespace}-external");
    let trap_network = format!("agent-hub-fixture-{namespace}-trap-network");
    let dns_name = format!("agent-hub-fixture-{namespace}-dns");
    let public_name = format!("agent-hub-fixture-{namespace}-public");
    let trap_name = format!("agent-hub-fixture-{namespace}-trap");
    let fixture_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");

    run_docker(
        docker,
        &[
            "run",
            "-d",
            "--name",
            &dns_name,
            "--label",
            &format!("{FIXTURE_LABEL}=true"),
            "--label",
            &format!("{FIXTURE_NAMESPACE_LABEL}={namespace}"),
            "--label",
            &format!("{FIXTURE_KIND_LABEL}=dns"),
            "--network=bridge",
            "--read-only",
            "--security-opt=no-new-privileges",
            "--cap-drop=ALL",
            "--cap-add=NET_BIND_SERVICE",
            "--tmpfs=/tmp:rw,nosuid,nodev,size=16m",
            "-e",
            &format!("FIXTURE_PUBLIC_IP={public_ip}"),
            "-e",
            &format!("FIXTURE_PRIVATE_IP={private_ip}"),
            "-v",
            &format!("{}:/fixture:ro", fixture_dir.display()),
            "--entrypoint=python3",
            JOB_IMAGE,
            "/fixture/dns_server.py",
        ],
        "start DNS fixture",
    )
    .await
    .unwrap();

    let watcher_docker = docker.to_path_buf();
    let watcher_network = external_network.clone();
    let watcher_dns = dns_name.clone();
    let watcher_dns_ip = dns_ip.clone();
    let watcher = tokio::spawn(async move {
        for _ in 0..100 {
            let present = tokio::process::Command::new(&watcher_docker)
                .args(["network", "inspect", &watcher_network])
                .output()
                .await?;
            if present.status.success() {
                let connected = tokio::process::Command::new(&watcher_docker)
                    .args([
                        "network",
                        "connect",
                        "--ip",
                        &watcher_dns_ip,
                        &watcher_network,
                        &watcher_dns,
                    ])
                    .output()
                    .await?;
                anyhow::ensure!(
                    connected.status.success(),
                    "connect DNS fixture failed: {}",
                    String::from_utf8_lossy(&connected.stderr)
                );
                return Ok::<(), anyhow::Error>(());
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        anyhow::bail!("executor external network did not appear")
    });

    let (_sender, receiver) = tokio::sync::watch::channel(false);
    let mut prepared = RestrictedEgressRequest {
        namespace: namespace.clone(),
        state_dir: root.clone(),
        proxy: ProxyRuntimeSpec {
            image: SQUID_IMAGE.to_string(),
            listen_port: 3128,
            uid: 13,
            gid: 13,
        },
        resolver: ResolverRuntimeSpec {
            image: JOB_IMAGE.to_string(),
            uid: 100,
            gid: 101,
            upstream: format!("{dns_ip}@53"),
            readiness_host: "public.fixture.net".to_string(),
        },
        policies: vec![RestrictedPolicy::GuardedWeb],
        cancellation: Cancellation::new(receiver),
        external_subnets: vec![public_subnet],
        failure_after: None,
    }
    .prepare(docker, &Deadline::new(Some(120)).unwrap())
    .await
    .expect("prepare deterministic guarded egress");
    watcher.await.unwrap().unwrap();

    run_docker(
        docker,
        &[
            "network",
            "create",
            "--subnet",
            &private_subnet,
            "--label",
            &format!("{FIXTURE_LABEL}=true"),
            "--label",
            &format!("{FIXTURE_NAMESPACE_LABEL}={namespace}"),
            "--label",
            &format!("{FIXTURE_KIND_LABEL}=trap-network"),
            &trap_network,
        ],
        "create private trap network",
    )
    .await
    .unwrap();
    for (name, network, ip, mode) in [
        (
            &public_name,
            &external_network,
            public_ip.as_str(),
            "public",
        ),
        (&trap_name, &trap_network, private_ip.as_str(), "trap"),
    ] {
        run_docker(
            docker,
            &[
                "run",
                "-d",
                "--name",
                name,
                "--label",
                &format!("{FIXTURE_LABEL}=true"),
                "--label",
                &format!("{FIXTURE_NAMESPACE_LABEL}={namespace}"),
                "--label",
                &format!("{FIXTURE_KIND_LABEL}={mode}-origin"),
                "--network",
                network,
                "--ip",
                ip,
                "--read-only",
                "--security-opt=no-new-privileges",
                "--cap-drop=ALL",
                "--cap-add=NET_BIND_SERVICE",
                "--tmpfs=/tmp:rw,exec,nosuid,nodev,size=32m",
                "-e",
                &format!("FIXTURE_MODE={mode}"),
                "-v",
                &format!("{}:/fixture:ro", fixture_dir.display()),
                "-v",
                &format!("{}:/evidence", evidence.display()),
                "--entrypoint=python3",
                JOB_IMAGE,
                "/fixture/origin_server.py",
            ],
            "start origin fixture",
        )
        .await
        .unwrap();
    }
    let proxy_name = format!("agent-hub-executor-{namespace}-proxy");
    run_docker(
        docker,
        &["network", "connect", &trap_network, &proxy_name],
        "attach proxy to adversarial private route",
    )
    .await
    .unwrap();
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;

    let model_script = r#"
set -eu
ok() { curl -kfsS --connect-timeout 4 --max-time 12 -o /dev/null "https://$1/"; }
deny() { if curl -kfsS --connect-timeout 2 --max-time 5 -o /dev/null "https://$1/"; then exit 30; fi; }
ok public.fixture.net
ok dual.fixture.net
ok mixed-a.fixture.net
ok mixed-aaaa.fixture.net
ok truncated.fixture.net
deny private.fixture.net
deny private-v6.fixture.net
deny mapped.fixture.net
deny cname-private.fixture.net
deny malformed.fixture.net
deny timeout.fixture.net
ok rebind.fixture.net
sleep 3
deny rebind.fixture.net
if curl -LkfsS --connect-timeout 2 --max-time 6 -o /dev/null https://public.fixture.net/redirect-private; then exit 31; fi
if getent hosts public.fixture.net >/dev/null 2>&1; then exit 32; fi
if env -u HTTP_PROXY -u HTTPS_PROXY -u ALL_PROXY -u http_proxy -u https_proxy -u all_proxy curl -kfsS --connect-timeout 2 --max-time 5 -o /dev/null https://__PUBLIC_IP__/; then exit 33; fi
for url in http://127.0.0.1/ http://2130706433/ http://0x7f000001/ 'http://[::ffff:__PRIVATE_IP__]/'; do
  if curl -kfsS --connect-timeout 2 --max-time 5 -o /dev/null "$url"; then exit 34; fi
done
"#
    .replace("__PUBLIC_IP__", &public_ip)
    .replace("__PRIVATE_IP__", &private_ip);
    let model = run_model(docker, &prepared, &model_script).await;

    let resolver_name = format!("agent-hub-executor-{namespace}-resolver");
    let resolver_stop =
        run_docker(docker, &["stop", &resolver_name], "stop resolver fixture").await;
    let resolver_failure = run_model(
        docker,
        &prepared,
        "if curl -kfsS --connect-timeout 2 --max-time 5 -o /dev/null https://fresh.fixture.net/; then exit 40; fi",
    )
    .await;
    let proxy_stop = run_docker(docker, &["stop", &proxy_name], "stop proxy fixture").await;
    let proxy_failure = run_model(
        docker,
        &prepared,
        "if curl -kfsS --connect-timeout 2 --max-time 5 -o /dev/null https://public.fixture.net/; then exit 41; fi",
    )
    .await;

    let mut fixture_cleanup = Vec::new();
    for (name, kind) in [
        (&public_name, "public-origin"),
        (&trap_name, "trap-origin"),
        (&dns_name, "dns"),
    ] {
        fixture_cleanup
            .push(remove_fixture_owned(docker, "container", name, &namespace, kind).await);
    }
    let egress_cleanup = prepared.cleanup(&Deadline::new(Some(30)).unwrap()).await;
    fixture_cleanup.push(
        remove_fixture_owned(docker, "network", &trap_network, &namespace, "trap-network").await,
    );
    let trap_evidence = std::fs::read_to_string(evidence.join("trap.log")).unwrap();
    let _ = std::fs::remove_dir_all(&root);

    model.unwrap();
    resolver_stop.unwrap();
    resolver_failure.unwrap();
    proxy_stop.unwrap();
    proxy_failure.unwrap();
    egress_cleanup.unwrap();
    for cleanup in fixture_cleanup {
        cleanup.unwrap();
    }
    assert!(
        trap_evidence.is_empty(),
        "private trap received connections: {trap_evidence}"
    );
}

#[tokio::test]
#[ignore = "requires the pinned local Docker images and daemon"]
async fn partial_prepare_boundaries_cleanup_exact_resources() {
    let docker = Path::new("docker");
    for boundary in PreparationBoundary::ALL {
        let root = test_root("failure-boundary");
        let namespace = format!(
            "failure-{}",
            &uuid::Uuid::new_v4().simple().to_string()[..12]
        );
        let (_sender, receiver) = tokio::sync::watch::channel(false);
        let result = RestrictedEgressRequest {
            namespace: namespace.clone(),
            state_dir: root.clone(),
            proxy: ProxyRuntimeSpec {
                image: SQUID_IMAGE.to_string(),
                listen_port: 3128,
                uid: 13,
                gid: 13,
            },
            resolver: ResolverRuntimeSpec::docker_embedded(JOB_IMAGE, 100, 101),
            policies: vec![RestrictedPolicy::GuardedWeb],
            cancellation: Cancellation::new(receiver),
            external_subnets: Vec::new(),
            failure_after: Some(boundary),
        }
        .prepare(docker, &Deadline::new(Some(120)).unwrap())
        .await;
        assert!(result.is_err(), "boundary {boundary:?} did not fail");
        for (kind, name) in [
            ("container", format!("agent-hub-executor-{namespace}-proxy")),
            (
                "container",
                format!("agent-hub-executor-{namespace}-resolver"),
            ),
            (
                "network",
                format!("agent-hub-executor-{namespace}-internal"),
            ),
            (
                "network",
                format!("agent-hub-executor-{namespace}-external"),
            ),
        ] {
            let output = tokio::process::Command::new(docker)
                .args([kind, "inspect", &name])
                .output()
                .await
                .unwrap();
            assert!(
                !output.status.success(),
                "boundary {boundary:?} leaked {kind} {name}"
            );
        }
        assert!(std::fs::read_dir(&root).unwrap().next().is_none());
        std::fs::remove_dir_all(root).unwrap();
    }
}

async fn run_model(
    docker: &Path,
    prepared: &agent_hub_executor::egress::PreparedRestrictedEgress,
    script: &str,
) -> anyhow::Result<()> {
    let args = vec![
        "run".to_string(),
        "--rm".to_string(),
        format!("--network={}", prepared.runtime.network_name),
        "--dns=127.0.0.1".to_string(),
        format!(
            "--add-host={}:{}",
            prepared.runtime.proxy_host, prepared.runtime.proxy_ip
        ),
        format!("-e=HTTP_PROXY={}", prepared.runtime.proxy_url),
        format!("-e=HTTPS_PROXY={}", prepared.runtime.proxy_url),
        format!("-e=http_proxy={}", prepared.runtime.proxy_url),
        format!("-e=https_proxy={}", prepared.runtime.proxy_url),
        "-e=ALL_PROXY=".to_string(),
        "-e=all_proxy=".to_string(),
        "-e=NO_PROXY=localhost,127.0.0.1,::1,.local".to_string(),
        "-e=no_proxy=localhost,127.0.0.1,::1,.local".to_string(),
        "--entrypoint=sh".to_string(),
        JOB_IMAGE.to_string(),
        "-ceu".to_string(),
        script.to_string(),
    ];
    let output = tokio::process::Command::new(docker)
        .args(&args)
        .output()
        .await?;
    anyhow::ensure!(
        output.status.success(),
        "model fixture failed: status={} stdout={} stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

async fn run_docker(docker: &Path, args: &[&str], operation: &str) -> anyhow::Result<()> {
    let output = tokio::process::Command::new(docker)
        .args(args)
        .output()
        .await?;
    anyhow::ensure!(
        output.status.success(),
        "{operation} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

async fn remove_fixture_owned(
    docker: &Path,
    resource_type: &str,
    name: &str,
    namespace: &str,
    fixture_kind: &str,
) -> anyhow::Result<()> {
    let inspect = tokio::process::Command::new(docker)
        .args([resource_type, "inspect", name])
        .output()
        .await?;
    anyhow::ensure!(
        inspect.status.success(),
        "fixture '{name}' disappeared before cleanup"
    );
    let values: Vec<serde_json::Value> = serde_json::from_slice(&inspect.stdout)?;
    let labels = values
        .first()
        .and_then(|value| {
            if resource_type == "container" {
                value.pointer("/Config/Labels")
            } else {
                value.get("Labels")
            }
        })
        .and_then(serde_json::Value::as_object);
    anyhow::ensure!(
        labels
            .and_then(|labels| labels.get(FIXTURE_LABEL))
            .and_then(serde_json::Value::as_str)
            == Some("true")
            && labels
                .and_then(|labels| labels.get(FIXTURE_NAMESPACE_LABEL))
                .and_then(serde_json::Value::as_str)
                == Some(namespace)
            && labels
                .and_then(|labels| labels.get(FIXTURE_KIND_LABEL))
                .and_then(serde_json::Value::as_str)
                == Some(fixture_kind),
        "fixture '{name}' ownership labels do not match"
    );
    let remove = if resource_type == "container" {
        vec!["rm", "-f", name]
    } else {
        vec!["network", "rm", name]
    };
    run_docker(docker, &remove, "remove owned fixture").await?;
    for _ in 0..2 {
        let absent = tokio::process::Command::new(docker)
            .args([resource_type, "inspect", name])
            .output()
            .await?;
        anyhow::ensure!(!absent.status.success(), "owned fixture '{name}' remains");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    Ok(())
}

fn test_root(name: &str) -> PathBuf {
    let root =
        std::env::temp_dir().join(format!("executor-egress-{name}-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    root
}
