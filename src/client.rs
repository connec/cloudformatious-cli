use std::{env, fmt, time::Duration};

use rusoto_cloudformation::CloudFormationClient;
use rusoto_core::{HttpClient, Region};
use rusoto_credential::{
    AutoRefreshingProvider, ChainProvider, ProfileProvider, ProvideAwsCredentials as _,
};

use crate::Error;

const MISSING_REGION: &str = "Unable CompletionsArgs, DeleteStack
You can set it in your profile, assign `AWS_REGION`, or supply `--region`.";

pub async fn get_client(region: Option<Region>) -> Result<CloudFormationClient, Error> {
    let region = region
        .map(Ok)
        .or_else(get_region)
        .ok_or_else(|| Error::other(MISSING_REGION))?
        .map_err(Error::other)?;
    let client = HttpClient::new().map_err(Error::other)?;

    let mut credentials = ChainProvider::new();
    credentials.set_timeout(Duration::from_secs(1));

    let credentials =
        AutoRefreshingProvider::new(aws_sso_flow::ChainProvider::new().push(credentials).push(
            aws_sso_flow::SsoFlow::builder().verification_prompt(|url| async move {
                if atty::is(atty::Stream::Stdin) && atty::is(atty::Stream::Stdout) {
                    eprintln!("Using SSO an profile – go to {url} to authenticate");
                    Ok(())
                } else {
                    Err(NonInteractiveSsoError)
                }
            }),
        ))
        .map_err(Error::other)?;

    // Proactively fetch credentials so we get earlier errors.
    credentials.credentials().await.map_err(Error::other)?;

    Ok(CloudFormationClient::new_with(client, credentials, region))
}

fn get_region() -> Option<Result<Region, Box<dyn std::error::Error>>> {
    // rusoto_cloudformation::Region::default implements a similar algorithm but falls back to
    // us-east-1, which we don't want.
    match env::var("AWS_DEFAULT_REGION").or_else(|_| env::var("AWS_REGION")) {
        Ok(region) => Some(region.parse().map_err(Into::into)),
        Err(_) => match ProfileProvider::region() {
            Ok(Some(region)) => Some(region.parse().map_err(Into::into)),
            Ok(None) => None,
            Err(error) => Some(Err(error.into())),
        },
    }
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
