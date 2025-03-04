use std::fmt;

use aws_config::{meta::credentials::CredentialsProviderChain, SdkConfig};
use aws_sdk_s3::config::ProvideCredentials;
use aws_types::region::Region;

use crate::Error;

pub async fn get_config(region: Option<Region>, no_input: bool) -> Result<SdkConfig, Error> {
    let sso = aws_sso_flow::SsoFlow::builder().verification_prompt(move |url| async move {
        if no_input {
            Err(NonInteractiveSsoError)
        } else {
            eprintln!("Using SSO an profile – go to {url} to authenticate");
            Ok(())
        }
    });
    let credentials = CredentialsProviderChain::first_try("SsoFlow", sso)
        .or_default_provider()
        .await;

    // Pre-warm the credentials. This ensures any interaction required by aws-sso-flow doesn't
    // interrupt connections.
    credentials
        .provide_credentials()
        .await
        .map_err(Error::other)?;

    let mut loader = aws_config::from_env().credentials_provider(credentials);
    if let Some(region) = region {
        loader = loader.region(region);
    }

    let config = loader.load().await;
    Ok(config)
}

pub async fn get_client<C>(
    ctor: impl FnOnce(&SdkConfig) -> C,
    region: Option<Region>,
    no_input: bool,
) -> Result<C, Error> {
    let config = get_config(region, no_input).await?;
    Ok(ctor(&config))
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
