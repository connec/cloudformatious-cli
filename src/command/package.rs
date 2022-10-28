use std::path::PathBuf;

use aws_types::region::Region;

use crate::{package, s3, Error, Template};

/// Package local assets referenced in a CloudFormation template.
///
/// This scans specific properties for specific resource types for paths that exist locally. If
/// found, the referenced path is uploaded to an S3 bucket. Paths may be absolute or relative.
/// Relative paths are resolved relative to the template directory. If the path points to a file it
/// will be uploaded as-is. If it's a directory, it will be zipped and the `.zip` file will be
/// uploaded. Nothing is uploaded if a file already exists with the same name and MD5 checksum.
///
/// Local artifacts can be referenced in the following places:
///
/// - `AWS::Lambda::Function`: `Code` property.
#[derive(Debug, clap::Parser)]
pub struct Args {
    /// The name of the S3 bucket to which artifacts will be uploaded.
    #[clap(long)]
    s3_bucket: String,

    /// A prefix under which the uploaded artifacts will be stored.
    #[clap(long)]
    s3_prefix: Option<String>,

    /// Path to the template to scan for local artifacts.
    template_path: PathBuf,
}

pub async fn main(region: Option<Region>, args: Args) -> Result<(), Error> {
    let client = s3::Client::new(region).await;
    let mut template = Template::open(args.template_path).await?;

    let targets = package::targets(&mut template);

    package::process(&client, &args.s3_bucket, args.s3_prefix.as_deref(), targets)
        .await
        .map_err(Error::other)?;

    println!("{}", template);

    Ok(())
}
