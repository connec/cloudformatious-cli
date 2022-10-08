use std::{convert::TryInto, path::Path};

use futures_util::{StreamExt, TryStreamExt};
use rusoto_core::Region;
use rusoto_s3::{HeadObjectRequest, PutObjectRequest, S3Client, S3};
use tokio::{
    fs::File,
    io::{AsyncSeekExt, BufReader},
};
use tokio_util::codec::{BytesCodec, FramedRead};

use crate::{client::get_client, Error};

pub struct Client {
    inner: rusoto_s3::S3Client,
}

impl Client {
    pub async fn new(region: Option<Region>) -> Result<Self, Error> {
        let inner = get_client(S3Client::new_with, region).await?;
        Ok(Self { inner })
    }

    pub async fn upload<'a>(&self, request: UploadRequest<'a>) -> Result<UploadOutput, Error> {
        let meta = request
            .file
            .metadata()
            .await
            .map_err(|error| Error::other(format!("couldn't stat upload package: {error}",)))?;

        let mut reader = FramedRead::new(BufReader::new(request.file), BytesCodec::new());

        let context = reader
            .by_ref()
            .try_fold(md5::Context::new(), |mut context, chunk| async move {
                context.consume(&chunk);
                Ok(context)
            })
            .await
            .map_err(|error| Error::other(format!("couldn't read upload package: {error}",)))?;
        let content_md5 = context.compute();

        let key = Path::new(request.prefix.unwrap_or(""))
            .join(format!("{:x}", content_md5))
            .to_string_lossy()
            .into_owned();

        let exists = self
            .inner
            .head_object(HeadObjectRequest {
                bucket: request.bucket.to_owned(),
                key: key.clone(),
                ..Default::default()
            })
            .await
            .map(|_| true)
            .or_else({
                let bucket = &request.bucket;
                let key = &key;
                move |error| match error {
                    rusoto_core::RusotoError::Unknown(res) if res.status.as_u16() == 404 => {
                        Ok(false)
                    }
                    error => Err(Error::other(format!(
                        "an error occurred when trying to read s3://{bucket}/{key}: {error}",
                    ))),
                }
            })?;
        if exists {
            return Ok(UploadOutput { key });
        }

        let mut file = reader.into_inner().into_inner();
        file.rewind()
            .await
            .map_err(|error| Error::other(format!("couldn't read upload package: {error}")))?;

        self.inner
            .put_object(PutObjectRequest {
                body: Some(rusoto_s3::StreamingBody::new_with_size(
                    FramedRead::new(BufReader::new(file), BytesCodec::new())
                        .map_ok(|chunk| chunk.freeze()),
                    meta.len()
                        .try_into()
                        .expect("file is too large for platform"),
                )),
                bucket: request.bucket.to_owned(),
                content_length: Some(meta.len().try_into().expect("file is insanely large")),
                content_md5: Some(base64::encode(&content_md5.0)),
                key: key.clone(),
                ..Default::default()
            })
            .await
            .map_err(|error| {
                Error::other(format!(
                    "an error occurred when uploading package to {key}: {error}",
                ))
            })?;

        Ok(UploadOutput { key })
    }
}

#[derive(Debug)]
pub struct UploadRequest<'a> {
    pub bucket: &'a str,
    pub prefix: Option<&'a str>,
    pub file: File,
}

#[derive(Debug)]
pub struct UploadOutput {
    pub key: String,
}
