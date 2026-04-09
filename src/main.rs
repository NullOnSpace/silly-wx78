use qq_bot::{AuthClient, AuthConfig};
use tracing::{error, info};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "qq_bot=debug".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    dotenvy::dotenv()?;

    let config = AuthConfig::from_env()?;
    let auth_client = AuthClient::new(config);

    match auth_client.get_access_token().await {
        Ok(token) => {
            info!("成功获取access_token");
            info!(
                authorization = %AuthClient::authorization_header(&token),
                "Authorization header已生成"
            );
        }
        Err(e) => {
            error!(error = %e, "获取access_token失败");
        }
    }

    Ok(())
}
