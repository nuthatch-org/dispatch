use anyhow::Result;
use tracing_subscriber::{fmt, EnvFilter};

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("keygen") {
        let name = args.get(2).map(String::as_str).unwrap_or("consumer");
        let (key, hash) = dispatch_gateway::auth::generate();
        println!("key (give to the consumer, shown once): {key}\n");
        println!("[[auth.keys]]\nname = \"{name}\"\nkey_hash = \"{hash}\"");
        return Ok(());
    }

    fmt().with_env_filter(EnvFilter::from_default_env()).init();

    let config = dispatch_gateway::config::Config::load()?;
    dispatch_gateway::server::run(config).await
}
