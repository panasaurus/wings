use crate::State;
use anyhow::{Context, Result};
use std::sync::Arc;
pub async fn serve(state: Arc<State>) -> Result<()> {
    let config = state.config.read().await.clone();
    let addr = std::net::SocketAddr::new(config.api.host.parse()?, config.api.port);
    let router = crate::api::router(state.clone())?;
    tracing::info!(address=%addr,servers=state.servers.read().await.len(),"Panasaurus node is ready");
    let handle = axum_server::Handle::new();
    let shutdown_handle = handle.clone();
    let shutdown_state = state.clone();
    tokio::spawn(async move {
        shutdown(shutdown_state).await;
        shutdown_handle.graceful_shutdown(Some(std::time::Duration::from_secs(5)));
    });
    if config.api.ssl.enabled {
        if let Some(domain) = config.api.ssl.acme_domain.clone() {
            use tokio_stream::StreamExt;
            let mut acme = rustls_acme::AcmeConfig::new([domain])
                .contact(
                    config
                        .api
                        .ssl
                        .acme_email
                        .clone()
                        .map(|e| format!("mailto:{e}")),
                )
                .cache(rustls_acme::caches::DirCache::new(
                    config.private_root().join("acme"),
                ))
                .directory_lets_encrypt(config.api.ssl.acme_production)
                .state();
            let acceptor = acme.axum_acceptor(acme.default_rustls_config());
            tokio::spawn(async move {
                while let Some(event) = acme.next().await {
                    if event.is_err() {
                        tracing::warn!(
                            "ACME certificate provisioning failed; verify DNS and TLS-ALPN access"
                        );
                    }
                }
            });
            axum_server::bind(addr)
                .acceptor(acceptor)
                .handle(handle.clone())
                .serve(router.into_make_service())
                .await?;
        } else {
            let tls = axum_server::tls_rustls::RustlsConfig::from_pem_file(
                config.api.ssl.cert,
                config.api.ssl.key,
            )
            .await?;
            axum_server::bind_rustls(addr, tls)
                .handle(handle.clone())
                .serve(router.into_make_service())
                .await?;
        }
    } else {
        axum_server::bind(addr)
            .handle(handle)
            .serve(router.into_make_service())
            .await?;
    }
    Ok(())
}
pub async fn cli(args: &[String]) -> Result<bool> {
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!(
            "Panasaurus Wings\n\nUsage: panasaurus-wings --config /absolute/path/config.yml\n\nCommands:\n  --version\n  --diagnostics --config PATH\n  --systemd-unit --config PATH\n  --configure --panel-url URL --node-id ID --token-env ENV_NAME --node-url URL --config PATH"
        );
        return Ok(true);
    }

    let option = |flag: &str| {
        args.iter()
            .position(|a| a == flag)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    let path = std::path::PathBuf::from(option("--config").unwrap_or_else(|| "config.yml".into()));
    if args.iter().any(|a| a == "--diagnostics") {
        let config = crate::config::Config::load(&path)?;
        let engine = crate::engine::Engine {
            config: config.clone(),
        };
        let docker = engine
            .json(&crate::engine::strings(&[
                "version",
                "--format",
                "{{json .Server}}",
            ]))
            .await?;
        println!(
            "{}",
            serde_json::json!({"version":env!("CARGO_PKG_VERSION"),"os":std::env::consts::OS,"architecture":std::env::consts::ARCH,"docker_version":docker["Version"],"tls":config.api.ssl.enabled,"sftp":config.system.sftp.enabled,"data_exists":config.system.data.is_dir()})
        );
        return Ok(true);
    }
    if args.iter().any(|a| a == "--configure") {
        let panel = option("--panel-url").context("--panel-url required")?;
        let node = option("--node-id")
            .context("--node-id required")?
            .parse::<u64>()?;
        let env = option("--token-env")
            .context("--token-env required; keep API credentials out of command arguments")?;
        let token = std::env::var(&env).context("API token environment variable missing")?;
        let base = reqwest::Url::parse(&panel)?;
        anyhow::ensure!(
            base.scheme() == "https"
                || base.host_str().is_some_and(|h| h == "localhost"
                    || h.parse::<std::net::IpAddr>()
                        .is_ok_and(|ip| ip.is_loopback())),
            "Panel configuration requires HTTPS or loopback"
        );
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(std::time::Duration::from_secs(30))
            .build()?;
        let value = client
            .get(format!(
                "{}/api/application/nodes/{node}/configuration",
                panel.trim_end_matches('/')
            ))
            .bearer_auth(token)
            .send()
            .await?
            .error_for_status()?
            .json::<serde_json::Value>()
            .await?;
        let mut configuration = value;
        configuration["jwt_audience"] =
            serde_json::json!(option("--node-url").context("--node-url required")?);
        configuration["api"]["host"] = serde_json::json!("127.0.0.1");
        configuration["api"]["ssl"]["enabled"] = serde_json::json!(false);
        let config: crate::config::Config = serde_json::from_value(configuration)?;
        anyhow::ensure!(
            !path.exists(),
            "Configuration exists; refusing to overwrite node credentials"
        );
        config.validate()?;
        config.save(&path)?;
        println!("Node configuration saved with private permissions.");
        return Ok(true);
    }
    if args.iter().any(|a| a == "--systemd-unit") {
        let executable = std::env::current_exe()?;
        let path = path.canonicalize()?;
        anyhow::ensure!(
            !executable
                .to_string_lossy()
                .contains(['\n', '\r', '"', '%', '\\'])
                && !path
                    .to_string_lossy()
                    .contains(['\n', '\r', '"', '%', '\\']),
            "Invalid service path"
        );
        println!(
            "[Unit]\nDescription=Panasaurus Wings\nAfter=docker.service network-online.target\nWants=network-online.target\n[Service]\nExecStart=\"{}\" --config \"{}\"\nRestart=on-failure\nRestartSec=5\nLimitNOFILE=65536\nUMask=0077\n[Install]\nWantedBy=multi-user.target",
            executable.display(),
            path.display()
        );
        return Ok(true);
    }
    Ok(false)
}

async fn shutdown(state: Arc<State>) {
    #[cfg(unix)]
    {
        if let Ok(mut terminate) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            tokio::select! {_=tokio::signal::ctrl_c()=>{},_=terminate.recv()=>{}}
        } else {
            let _ = tokio::signal::ctrl_c().await;
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
    if crate::lifecycle::persist(&state).await.is_err() {
        tracing::warn!("Could not persist recovery state on shutdown");
    }
}
