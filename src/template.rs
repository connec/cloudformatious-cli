use std::{
    borrow::Cow,
    fmt,
    path::{Path, PathBuf},
};

use serde_yaml::Value as YamlValue;
use tokio::{
    fs,
    io::{self, AsyncReadExt},
};

use crate::Error;

pub struct Template {
    path: Option<PathBuf>,
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
        let (yaml, path) = if path == Path::new("-") {
            let mut yaml = Vec::new();
            io::stdin()
                .read_to_end(&mut yaml)
                .await
                .map_err(ReadError::for_source(Path::new("STDIN")))?;
            (yaml, None)
        } else {
            let meta = fs::metadata(&path)
                .await
                .map_err(ReadError::for_source(path.as_path()))?;
            if !meta.is_file() {
                return Err(Error::other(ReadError::new(
                    path.as_path(),
                    io::Error::new(io::ErrorKind::Other, "not a file"),
                )));
            }

            (
                fs::read(&path)
                    .await
                    .map_err(ReadError::for_source(path.as_path()))?,
                Some(path),
            )
        };

        let content = serde_yaml::from_slice(&yaml).map_err(ParseError::for_source(
            path.as_deref().unwrap_or_else(|| Path::new("STDIN")),
        ))?;

        Ok(Self { path, content })
    }

    pub fn source(&self) -> Source {
        self.path
            .as_deref()
            .map_or_else(|| Source::Stdin, Source::from)
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
pub enum Source<'a> {
    Path(Cow<'a, Path>),
    Stdin,
}

impl Source<'_> {
    fn into_static(self) -> Source<'static> {
        match self {
            Source::Path(Cow::Borrowed(path)) => Source::Path(Cow::Owned(path.to_owned())),
            Source::Path(Cow::Owned(path)) => Source::Path(path.into()),
            Source::Stdin => Source::Stdin,
        }
    }
}

impl<'a> From<&'a Path> for Source<'a> {
    fn from(path: &'a Path) -> Self {
        Self::Path(path.into())
    }
}

impl fmt::Display for Source<'_> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Source::Path(path) => write!(f, "{}", path.display()),
            Source::Stdin => write!(f, "STDIN"),
        }
    }
}

#[derive(Debug)]
pub struct ReadError {
    template_source: Source<'static>,
    error: io::Error,
}

impl ReadError {
    fn new<'a>(template_source: impl Into<Source<'a>> + 'a, error: io::Error) -> Self {
        Self {
            template_source: template_source.into().into_static(),
            error,
        }
    }

    fn for_source<'a>(
        template_source: impl Into<Source<'a>> + 'a,
    ) -> impl FnOnce(io::Error) -> Self + 'a {
        move |error| Self::new(template_source, error)
    }
}

impl fmt::Display for ReadError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "couldn't read template `{}` due to: {}",
            self.template_source, self.error
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
    template_source: Source<'static>,
    error: serde_yaml::Error,
}

impl ParseError {
    fn for_source<'a>(
        template_source: impl Into<Source<'a>> + 'a,
    ) -> impl FnOnce(serde_yaml::Error) -> Self + 'a {
        move |error| Self {
            template_source: template_source.into().into_static(),
            error,
        }
    }
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "invalid template `{}`: {}",
            self.template_source, self.error
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
