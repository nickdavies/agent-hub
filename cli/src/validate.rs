use crate::client::Client;

/// Validate subcommand — checks config, rules, delegate commands, and server connectivity.
/// Returns the process exit code (0 = pass, 1 = failures).
pub async fn run(config_path: String, server: Option<String>, token: Option<String>) -> i32 {
    let mut failures = 0u32;
    let mut warnings = 0u32;

    // --- Check 1: Config file found and valid ---

    let expanded_path = config::expand_tilde(&config_path);

    let tool_config = match config::validate_tool_config(&expanded_path) {
        Ok((cfg, warns)) => {
            let default_str = format!("{:?}", cfg.default).to_lowercase();
            let rule_count = cfg.rule_summaries().len();
            println!(
                "[OK]   Config loaded: {} (default={}, {} rules)",
                config_path, default_str, rule_count
            );
            for w in &warns {
                println!("[WARN] Config: {}", w);
                warnings += 1;
            }
            Some(cfg)
        }
        Err(e) => {
            println!("[FAIL] Config: {}", e);
            failures += 1;
            None
        }
    };

    // --- Check 2: Rules well-formed + Check 3: Delegate commands resolvable ---

    if let Some(cfg) = &tool_config {
        let summaries = cfg.rule_summaries();
        for s in &summaries {
            let tools_display = s.tools.join(", ");
            if let Some(cmd) = &s.command {
                println!(
                    "[OK]   Rule {}: tools=[{}] action={} command={:?}",
                    s.index + 1,
                    tools_display,
                    s.action,
                    cmd
                );
            } else {
                println!(
                    "[OK]   Rule {}: tools=[{}] action={}",
                    s.index + 1,
                    tools_display,
                    s.action
                );
            }
        }

        // Check 3: Delegate commands
        for s in &summaries {
            if s.action == "delegate"
                && let Some(cmd) = &s.command
            {
                let executable = cmd.split_whitespace().next().unwrap_or(cmd);
                match which::which(executable) {
                    Ok(path) => {
                        println!("[OK]   Delegate {:?} found: {}", executable, path.display());
                    }
                    Err(_) => {
                        println!("[FAIL] Delegate {:?} not found on PATH", executable);
                        failures += 1;
                    }
                }
            }
        }
    }

    // --- Check 4: Server connectivity and auth ---

    match &server {
        Some(server_url) => {
            let server_url = normalize_server_url(server_url);
            let client = Client::new(server_url.clone(), token.clone());

            // Phase A: health check (unauthenticated)
            match client.health_check().await {
                Ok(()) => {
                    println!("[OK]   Server reachable: {}/health -> 200", server_url);

                    // Phase B: auth check (only if --token provided)
                    match &token {
                        Some(_) => match client.check_auth().await {
                            Ok(status) if (200..300).contains(&status) => {
                                println!(
                                    "[OK]   Auth valid: {}/api/v1/sessions -> {}",
                                    server_url, status
                                );
                            }
                            Ok(status) => {
                                println!(
                                    "[FAIL] Auth rejected: {}/api/v1/sessions -> {}",
                                    server_url, status
                                );
                                failures += 1;
                            }
                            Err(e) => {
                                println!("[FAIL] Auth check failed: {}", e);
                                failures += 1;
                            }
                        },
                        None => {
                            println!("[SKIP] Auth check: --token not provided");
                        }
                    }
                }
                Err(e) => {
                    println!("[FAIL] Server unreachable: {}", e);
                    failures += 1;
                    // Skip auth check if server is unreachable
                    if token.is_some() {
                        println!("[SKIP] Auth check: server unreachable");
                    }
                }
            }
        }
        None => {
            println!("[SKIP] Server check: --server not provided");
        }
    }

    // --- Summary ---

    println!();
    if failures == 0 {
        println!("All checks passed.");
        0
    } else {
        println!(
            "Validation failed: {} error(s), {} warning(s).",
            failures, warnings
        );
        1
    }
}

/// Normalize a server URL: default to `http://` if no scheme is present,
/// and strip any trailing path slash.
fn normalize_server_url(input: &str) -> String {
    // url::Url requires a scheme. Inputs like "localhost:8080" parse successfully but
    // misinterpret the hostname as a scheme, so we also check for a valid host.
    let with_scheme = match url::Url::parse(input) {
        Ok(u) if u.host().is_some() => input.to_string(),
        _ => format!("http://{input}"),
    };
    let mut url = url::Url::parse(&with_scheme)
        .unwrap_or_else(|_| url::Url::parse("http://localhost").unwrap());
    // Strip trailing slash from the path (Url always normalises to at least "/").
    let trimmed_path = url.path().trim_end_matches('/').to_string();
    url.set_path(&trimmed_path);
    // Return without trailing slash; Url::to_string() may re-add one for bare origins,
    // so we trim again for safety.
    url.as_str().trim_end_matches('/').to_string()
}
