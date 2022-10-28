use std::fmt;

use aws_config::{meta::credentials::CredentialsProviderChain, SdkConfig};
use aws_types::region::Region;

pub async fn get_config(region: Option<Region>) -> SdkConfig {
    let sso = aws_sso_flow::SsoFlow::builder().verification_prompt(|url| async move {
        if atty::is(atty::Stream::Stdin) && atty::is(atty::Stream::Stdout) {
            eprintln!("Using SSO an profile – go to {url} to authenticate");
            Ok(())
        } else {
            Err(NonInteractiveSsoError)
        }
    });
    let credentials = CredentialsProviderChain::first_try("SsoFlow", sso)
        .or_default_provider()
        .await;

    let mut config = aws_config::from_env().credentials_provider(credentials);
    if let Some(region) = region {
        config = config.region(region);
    }

    config.load().await
}

pub async fn get_client<C>(ctor: impl FnOnce(&SdkConfig) -> C, region: Option<Region>) -> C {
    let config = get_config(region).await;
    ctor(&config)
}

#[derive(Debug)]
pub struct NonInteractiveSsoError;

impl fmt::Display for NonInteractiveSsoError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "can't complete SSO authentication in a non-interactive context"
        )
    }
}

impl std::error::Error for NonInteractiveSsoError {}
