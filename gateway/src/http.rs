use reqwest::{Client, Url, redirect::Policy};

pub(crate) fn authenticated_client(server: &str) -> Result<(Client, Url), String> {
    let mut endpoint =
        Url::parse(server).map_err(|error| format!("server URL is invalid: {error}"))?;
    if !matches!(endpoint.scheme(), "http" | "https") {
        return Err("server URL must use HTTP or HTTPS".to_string());
    }
    if !endpoint.username().is_empty() || endpoint.password().is_some() {
        return Err("server URL must not contain credentials".to_string());
    }
    if endpoint.query().is_some() || endpoint.fragment().is_some() {
        return Err("server URL must not contain query or fragment".to_string());
    }
    if endpoint.scheme() == "http" {
        let loopback = endpoint
            .host_str()
            .map(|host| host.trim_matches(['[', ']']))
            .and_then(|host| host.parse::<std::net::IpAddr>().ok())
            .is_some_and(|address| address.is_loopback());
        if !loopback {
            return Err(
                "bearer authentication requires HTTPS or a literal loopback HTTP address"
                    .to_string(),
            );
        }
    }
    if !endpoint.path().ends_with('/') {
        endpoint.set_path(&format!("{}/", endpoint.path()));
    }
    let client = Client::builder()
        .redirect(Policy::none())
        .build()
        .map_err(|error| format!("cannot build HTTP client: {error}"))?;
    Ok((client, endpoint))
}

pub(crate) fn endpoint(base: &Url, path: &str) -> Result<Url, String> {
    base.join(path)
        .map_err(|error| format!("cannot build server URL: {error}"))
}

#[cfg(test)]
mod tests {
    use super::authenticated_client;

    #[test]
    fn bearer_endpoints_require_https_or_literal_loopback_http() {
        assert!(authenticated_client("http://example.test").is_err());
        assert!(authenticated_client("http://localhost:8080").is_err());
        authenticated_client("http://127.0.0.1:8080").unwrap();
        authenticated_client("http://[::1]:8080").unwrap();
        authenticated_client("https://example.test/base").unwrap();
    }
}
