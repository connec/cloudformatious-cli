use std::{
    convert::TryInto,
    path::Path,
    sync::{Arc, Mutex},
};

use aws_sdk_s3::primitives::ByteStream;
use aws_types::region::Region;
use futures_util::{StreamExt, TryStreamExt};
use tokio::{
    fs::File,
    io::{AsyncSeekExt, BufReader},
};
use tokio_util::codec::{BytesCodec, FramedRead};

use crate::{client::get_client, Error};

pub struct Client {
    inner: aws_sdk_s3::Client,
}

impl Client {
    pub async fn new(region: Option<Region>, no_input: bool) -> Result<Self, Error> {
        let inner = get_client(aws_sdk_s3::Client::new, region, no_input).await?;
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

        let uri: Arc<Mutex<Option<String>>> = Default::default();
        let exists = self
            .inner
            .head_object()
            .bucket(request.bucket)
            .key(&key)
            .customize()
            .mutate_request({
                let uri = uri.clone();
                move |req| {
                    *uri.lock().unwrap() = Some(req.uri().to_owned());
                }
            })
            .send()
            .await
            .map(|_| true)
            .or_else({
                let bucket = &request.bucket;
                let key = &key;
                move |error| match error {
                    aws_sdk_s3::error::SdkError::ServiceError(err) if err.err().is_not_found() => {
                        Ok(false)
                    }
                    error => Err(Error::other(format!(
                        "an error occurred when trying to read s3://{bucket}/{key}: {error}",
                    ))),
                }
            })?;
        let uri = uri
            .lock()
            .unwrap()
            .take()
            .expect("BUG: uri not set after request");
        if exists {
            return Ok(UploadOutput { uri, key });
        }

        let mut file = reader.into_inner().into_inner();
        file.rewind()
            .await
            .map_err(|error| Error::other(format!("couldn't read upload package: {error}")))?;

        let body =
            hyper::Body::wrap_stream(FramedRead::new(BufReader::new(file), BytesCodec::new()));

        self.inner
            .put_object()
            .body(ByteStream::from_body_0_4(body))
            .bucket(request.bucket)
            .content_length(meta.len().try_into().expect("file is insanely large"))
            .content_md5(base64::encode(content_md5.0))
            .key(&key)
            .send()
            .await
            .map_err(|error| {
                Error::other(format!(
                    "an error occurred when uploading package to {key}: {error:#?}",
                ))
            })?;

        Ok(UploadOutput { uri, key })
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
    pub uri: String,
    pub key: String,
}
