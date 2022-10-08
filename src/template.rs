use std::{
    fmt,
    path::{Path, PathBuf},
};

use serde_yaml::Value as YamlValue;
use tokio::{fs, io};

use crate::Error;

pub struct Template {
    path: PathBuf,
    content: YamlValue,
}

pub struct ResourceMut<'t> {
    resource_id: &'t str,
    resource_type: &'t mut str,
    properties: &'t mut YamlValue,
}

impl<'t> ResourceMut<'t> {
    pub fn into_parts_mut(self) -> (&'t str, &'t mut str, &'t mut YamlValue) {
        (self.resource_id, self.resource_type, self.properties)
    }

    pub fn resource_type(&self) -> &str {
        &*self.resource_type
    }
}

impl Template {
    pub async fn open(path: PathBuf) -> Result<Self, Error> {
        let meta = fs::metadata(&path)
            .await
            .map_err(ReadError::for_path(&path))?;
        if !meta.is_file() {
            return Err(Error::other(ReadError::new(
                &path,
                io::Error::new(io::ErrorKind::Other, "not a file"),
            )));
        }

        let yaml = fs::read(&path).await.map_err(ReadError::for_path(&path))?;
        let content = serde_yaml::from_slice(&yaml).map_err(ParseError::for_path(&path))?;

        Ok(Self { path, content })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn resources_mut(&mut self) -> impl Iterator<Item = ResourceMut<'_>> {
        self.content
            .get_mut("Resources")
            .and_then(YamlValue::as_mapping_mut)
            .map(|resources| resources.iter_mut())
            .into_iter()
            .flatten()
            .filter_map(|(key, val)| {
                let resource_id = key.as_str()?;
                let resource = val.as_mapping_mut()?;
                let (resource_type, properties) = resource.iter_mut().fold(
                    (None, None),
                    |(resource_type, properties), (key, value)| {
                        if let Some("Type") = key.as_str() {
                            if let YamlValue::String(resource_type) = value {
                                return (Some(resource_type), properties);
                            }
                        }
                        if let Some("Properties") = key.as_str() {
                            return (resource_type, Some(value));
                        }
                        (resource_type, properties)
                    },
                );
                let resource_type = resource_type?;
                let properties = properties?;
                Some(ResourceMut {
                    resource_id,
                    resource_type,
                    properties,
                })
            })
    }
}

impl fmt::Display for Template {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "{}",
            serde_yaml::to_string(&self.content).expect("template was parsed so must serialize")
        )
    }
}

#[derive(Debug)]
pub struct ReadError {
    path: PathBuf,
    error: io::Error,
}

impl ReadError {
    fn new(path: &Path, error: io::Error) -> Self {
        Self {
            path: path.to_path_buf(),
            error,
        }
    }

    fn for_path(path: &Path) -> impl FnOnce(io::Error) -> Self + '_ {
        move |error| Self::new(path, error)
    }
}

impl fmt::Display for ReadError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "couldn't read template `{}` due to: {}",
            self.path.display(),
            self.error
        )
    }
}

impl std::error::Error for ReadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.error)
    }
}

impl From<ReadError> for Error {
    fn from(error: ReadError) -> Self {
        Self::other(error)
    }
}

#[derive(Debug)]
pub struct ParseError {
    path: PathBuf,
    error: serde_yaml::Error,
}

impl ParseError {
    fn for_path(path: &Path) -> impl FnOnce(serde_yaml::Error) -> Self + '_ {
        move |error| Self {
            path: path.to_path_buf(),
            error,
        }
    }
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "invalid template `{}`: {}",
            self.path.display(),
            self.error
        )
    }
}

impl std::error::Error for ParseError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.error)
    }
}

impl From<ParseError> for Error {
    fn from(error: ParseError) -> Self {
        Self::other(error)
    }
}
