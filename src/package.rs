use std::{
    collections::HashMap,
    fmt,
    iter::FromIterator,
    path::{Path, PathBuf},
};

use async_zip::{write::ZipFileWriter, Compression, ZipEntryBuilder};
use chrono::{DateTime, Utc};
use futures_util::{stream, TryStreamExt};
use serde_yaml::Value as YamlValue;
use tokio::{
    fs::{self, File},
    io::{self, AsyncSeekExt},
};

use crate::{s3, template, Error, Template};

#[derive(Debug)]
pub struct PackageableProperty {
    resource_type: &'static str,
    path: &'static [&'static str],
    s3_ref: fn(String, s3::UploadOutput) -> serde_yaml::Value,
}

const PACKAGEABLE_PROPERTIES: &[PackageableProperty] = &[
    PackageableProperty {
        resource_type: "AWS::Lambda::Function",
        path: &["Code"],
        s3_ref: |bucket, upload| {
            serde_yaml::Mapping::from_iter([
                (
                    serde_yaml::Value::String("S3Bucket".to_string()),
                    serde_yaml::Value::String(bucket),
                ),
                (
                    serde_yaml::Value::String("S3Key".to_string()),
                    serde_yaml::Value::String(upload.key),
                ),
            ])
            .into()
        },
    },
    PackageableProperty {
        resource_type: "AWS::Serverless::Function",
        path: &["CodeUri"],
        s3_ref: |bucket, upload| {
            serde_yaml::Mapping::from_iter([
                (
                    serde_yaml::Value::String("Bucket".to_string()),
                    serde_yaml::Value::String(bucket),
                ),
                (
                    serde_yaml::Value::String("Key".to_string()),
                    serde_yaml::Value::String(upload.key),
                ),
            ])
            .into()
        },
    },
];

pub struct Target<'y> {
    resource_id: &'y str,
    property: &'static PackageableProperty,
    target: &'y mut YamlValue,
    path: PathBuf,
}

pub fn targets(template: &mut Template) -> impl Iterator<Item = Target<'_>> + '_ {
    // Build a map of packageable property for easy lookup
    let packageable_properties: HashMap<_, _> = PACKAGEABLE_PROPERTIES
        .iter()
        .map(|prop| (prop.resource_type, prop))
        .collect();

    let package_dir = match template.source() {
        template::Source::Path(path) => path
            .parent()
            .expect("file path must have a parent")
            .to_path_buf(),
        template::Source::Stdin => PathBuf::from(""),
    };

    template.resources_mut().filter_map(move |resource| {
        let property = packageable_properties.get(resource.resource_type())?;
        let (resource_id, _, properties) = resource.into_parts_mut();
        let target = property
            .path
            .iter()
            .try_fold(properties, |props, key| props.get_mut(key))?;
        let path = package_dir.join(target.as_str()?);

        Some(Target {
            resource_id,
            property,
            target,
            path,
        })
    })
}

pub async fn process(
    client: &s3::Client,
    s3_bucket: &str,
    s3_prefix: Option<&str>,
    targets: impl IntoIterator<Item = Target<'_>>,
) -> Result<(), Error> {
    stream::iter(targets.into_iter().map(Ok::<_, Error>))
        .try_for_each_concurrent(None, |target| async move {
            let file = package_zip(&target).await?;

            let upload = client
                .upload(s3::UploadRequest {
                    bucket: s3_bucket,
                    prefix: s3_prefix,
                    file,
                })
                .await
                .or_else(|error| upload_err(&target, error))?;

            *target.target = (target.property.s3_ref)(s3_bucket.to_string(), upload);

            Ok(())
        })
        .await?;

    Ok(())
}

async fn package_zip(target: &Target<'_>) -> Result<File, Error> {
    let metadata = match fs::metadata(&target.path).await {
        Ok(metadata) => metadata,
        Err(error) => return upload_err(target, error),
    };

    let mut zip = File::from_std(
        tokio::task::spawn_blocking(tempfile::tempfile)
            .await
            .unwrap_or_else(|error| std::panic::resume_unwind(error.into_panic()))
            .or_else(|error| {
                upload_err(target, format!("couldn't create temporary file: {error}"))
            })?,
    );
    let mut writer = ZipFileWriter::new(&mut zip);

    let paths = if metadata.is_file() {
        vec![Ok(target.path.clone())]
    } else if metadata.is_dir() {
        let path = target.path.clone();
        tokio::task::spawn_blocking(move || scandir(&path))
            .await
            .or_else(|error| upload_err(target, format!("couldn't read: {error}")))?
    } else {
        return upload_err(target, "not a file or directory");
    };

    for path in paths {
        let path = path.or_else(|error| upload_err(target, format!("couldn't read: {error}")))?;

        let file_name = path
            .strip_prefix(&target.path)
            .expect("file must have file name")
            .to_string_lossy()
            .into_owned();
        let mut entry_writer = writer
            .write_entry_stream(
                ZipEntryBuilder::new(file_name, Compression::Deflate)
                    .last_modification_date(DateTime::<Utc>::MIN_UTC)
                    .build(),
            )
            .await
            .or_else(|error| upload_err(target, format!("couldn't write: {error}",)))?;

        let mut file = io::BufReader::new(
            File::open(path)
                .await
                .or_else(|error| upload_err(target, format!("couldn't open: {error}")))?,
        );
        io::copy_buf(&mut file, &mut entry_writer)
            .await
            .or_else(|error| upload_err(target, format!("write error: {error}",)))?;
        entry_writer
            .close()
            .await
            .or_else(|error| upload_err(target, format!("couldn't write: {error}")))?;
    }

    writer
        .close()
        .await
        .or_else(|error| upload_err(target, format!("couldn't write: {error}")))?;

    zip.rewind()
        .await
        .or_else(|error| upload_err(target, format!("read error: {error}")))?;
    Ok(zip)
}

fn scandir(path: &Path) -> Vec<io::Result<PathBuf>> {
    let entries = match std::fs::read_dir(path) {
        Ok(entries) => entries,
        Err(error) => return vec![Err(error)],
    };

    entries.fold(Vec::new(), |mut paths, entry| {
        let entry_path = match entry {
            Ok(entry) => entry.path(),
            Err(error) => {
                paths.push(Err(error));
                return paths;
            }
        };

        let metadata = match std::fs::metadata(&entry_path) {
            Ok(metadata) => metadata,
            Err(error) => {
                paths.push(Err(error));
                return paths;
            }
        };

        if metadata.is_dir() {
            paths.extend(scandir(&entry_path));
        } else {
            paths.push(Ok(entry_path));
        }

        paths
    })
}

fn upload_err<T>(target: &Target, error: impl fmt::Display) -> Result<T, Error> {
    Err(Error::other(format!(
        "couldn't upload `{}` for `{}`: {error}",
        target.path.display(),
        target.resource_id
    )))
}
