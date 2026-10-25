//! TOML and YAML documents: one loader and one writer for both formats.
//!
//! Every file is capped at 1 MiB before it is parsed. YAML goes through
//! `yaml_options()`, which refuses anchors, aliases, merge keys, duplicate keys,
//! unsupported tags and a second document in the stream.

use std::fmt;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::paths::DocPath;

/// Largest document anvil reads, in bytes.
pub const MAX_BYTES: u64 = 1 << 20;

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, serde::Deserialize, clap::ValueEnum,
)]
#[serde(rename_all = "lowercase")]
pub enum Format {
    #[default]
    Toml,
    Yaml,
}

impl Format {
    /// The format named by the path's extension: `.toml`, `.yaml` or `.yml`.
    pub fn from_extension(path: &Path) -> Option<Format> {
        match path.extension()?.to_str()? {
            "toml" => Some(Format::Toml),
            "yaml" | "yml" => Some(Format::Yaml),
            _ => None,
        }
    }

    /// The extension of this format's edit view (the editor's temp file).
    pub fn extension(self) -> &'static str {
        match self {
            Format::Toml => "toml",
            Format::Yaml => "yaml",
        }
    }

    /// The other format.
    pub fn other(self) -> Format {
        match self {
            Format::Toml => Format::Yaml,
            Format::Yaml => Format::Toml,
        }
    }
}

impl fmt::Display for Format {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.extension())
    }
}

#[derive(Debug)]
pub enum DocumentError {
    Read {
        path: PathBuf,
        source: io::Error,
    },
    TooLarge {
        path: PathBuf,
    },
    Parse {
        path: PathBuf,
        format: Format,
        message: String,
    },
    /// A document that parsed but breaks a schema rule; `message` names the
    /// offending host and field.
    Invalid {
        path: PathBuf,
        message: String,
    },
    Serialize {
        format: Format,
        message: String,
    },
}

impl DocumentError {
    pub fn invalid(path: &Path, message: impl Into<String>) -> Self {
        DocumentError::Invalid {
            path: path.to_path_buf(),
            message: message.into(),
        }
    }

    pub fn is_not_found(&self) -> bool {
        matches!(self, DocumentError::Read { source, .. } if source.kind() == io::ErrorKind::NotFound)
    }
}

impl fmt::Display for DocumentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DocumentError::Read { path, source } => {
                write!(f, "{}: cannot read: {source}", path.display())
            }
            DocumentError::TooLarge { path } => write!(
                f,
                "{}: larger than the {MAX_BYTES} byte limit",
                path.display()
            ),
            DocumentError::Parse {
                path,
                format,
                message,
            } => write!(f, "{}: invalid {format}: {message}", path.display()),
            DocumentError::Invalid { path, message } => {
                write!(f, "{}: {message}", path.display())
            }
            DocumentError::Serialize { format, message } => {
                write!(f, "cannot write {format}: {message}")
            }
        }
    }
}

impl std::error::Error for DocumentError {}

/// Reads and parses `doc` in its format.
pub fn load<T: DeserializeOwned>(doc: &DocPath) -> Result<T, DocumentError> {
    let path = doc.path.as_path();
    let read = |source| DocumentError::Read {
        path: path.to_path_buf(),
        source,
    };
    let bytes = read_capped(path).map_err(read)?;
    parse_text(&bytes, path, doc.format, |err| {
        read(io::Error::new(io::ErrorKind::InvalidData, err))
    })
}

/// [`load`]s `doc` and checks it with `validate`; `None` or a missing file is
/// `T::default()`, which is checked too.
pub fn load_or_default<T: DeserializeOwned + Default>(
    doc: Option<&DocPath>,
    validate: impl Fn(&T, &Path) -> Result<(), DocumentError>,
) -> Result<T, DocumentError> {
    let Some(doc) = doc else {
        return Ok(T::default());
    };
    let value = match load(doc) {
        Ok(value) => value,
        Err(err) if err.is_not_found() => T::default(),
        Err(err) => return Err(err),
    };
    validate(&value, &doc.path)?;
    Ok(value)
}

/// The first `MAX_BYTES + 1` bytes of `path`: one byte past the limit, so a
/// caller can tell an oversize file from one that just fits.
pub fn read_capped(path: &Path) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take(MAX_BYTES + 1)
        .read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// Parses `bytes` read from `path` as `format`: refused past [`MAX_BYTES`],
/// then decoded as UTF-8 (`utf8` maps that error), then parsed.
pub fn parse_text<T: DeserializeOwned>(
    bytes: &[u8],
    path: &Path,
    format: Format,
    utf8: impl FnOnce(std::str::Utf8Error) -> DocumentError,
) -> Result<T, DocumentError> {
    if bytes.len() as u64 > MAX_BYTES {
        return Err(DocumentError::TooLarge {
            path: path.to_path_buf(),
        });
    }
    let text = std::str::from_utf8(bytes).map_err(utf8)?;
    from_str(text, format).map_err(|message| DocumentError::Parse {
        path: path.to_path_buf(),
        format,
        message,
    })
}

/// Parses `text` as `format`; the error is the parser's message.
pub fn from_str<T: DeserializeOwned>(text: &str, format: Format) -> Result<T, String> {
    match format {
        Format::Toml => toml::from_str(text).map_err(|err| err.to_string()),
        Format::Yaml => {
            serde_saphyr::from_str_with_options(text, yaml_options()).map_err(|err| err.to_string())
        }
    }
}

/// Serializes `value` as `format`.
pub fn to_string<T: Serialize>(value: &T, format: Format) -> Result<String, DocumentError> {
    let result = match format {
        Format::Toml => toml::to_string(value).map_err(|err| err.to_string()),
        Format::Yaml => serde_saphyr::to_string(value).map_err(|err| err.to_string()),
    };
    result.map_err(|message| DocumentError::Serialize { format, message })
}

fn yaml_options() -> serde_saphyr::Options {
    serde_saphyr::options! {
        budget: serde_saphyr::budget! {
            max_anchors: 0,
            max_aliases: 0,
            max_merge_keys: 0,
            max_depth: 32,
            max_documents: 1,
            max_total_scalar_bytes: 1 << 20,
        },
        merge_keys: serde_saphyr::MergeKeyPolicy::Error,
        duplicate_keys: serde_saphyr::DuplicateKeyPolicy::Error,
        reject_unsupported_tags: true,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    type Doc = BTreeMap<String, BTreeMap<String, String>>;

    fn yaml(text: &str) -> Result<Doc, String> {
        from_str(text, Format::Yaml)
    }

    #[test]
    fn format_from_extension() {
        assert_eq!(
            Format::from_extension(Path::new("a/inventory.toml")),
            Some(Format::Toml)
        );
        assert_eq!(
            Format::from_extension(Path::new("inventory.yaml")),
            Some(Format::Yaml)
        );
        assert_eq!(
            Format::from_extension(Path::new("inventory.yml")),
            Some(Format::Yaml)
        );
        assert_eq!(Format::from_extension(Path::new("inventory.json")), None);
        assert_eq!(Format::from_extension(Path::new("inventory")), None);
    }

    fn doc(path: &Path) -> DocPath {
        DocPath::from_extension(path).unwrap()
    }

    #[test]
    fn missing_file_is_not_found() {
        let err = load::<Doc>(&doc(Path::new("/nonexistent/inventory.toml"))).unwrap_err();
        assert!(err.is_not_found());
    }

    #[test]
    fn same_tree_in_toml_and_yaml() {
        let toml: Doc = from_str("[a]\nk = \"v\"\n", Format::Toml).unwrap();
        let from_yaml: Doc = yaml("a:\n  k: v\n").unwrap();
        assert_eq!(toml, from_yaml);
        let round: Doc = yaml(&to_string(&toml, Format::Yaml).unwrap()).unwrap();
        assert_eq!(round, toml);
        let round: Doc = from_str(&to_string(&toml, Format::Toml).unwrap(), Format::Toml).unwrap();
        assert_eq!(round, toml);
    }

    #[test]
    fn yaml_alias_is_refused() {
        let err = yaml("a: &x\n  k: v\nb: *x\n").unwrap_err();
        assert!(err.contains("anchor") || err.contains("alias"), "{err}");
    }

    #[test]
    fn yaml_merge_key_is_refused() {
        let err = yaml("a:\n  k: v\nb:\n  <<: {k: w}\n").unwrap_err();
        assert!(err.to_lowercase().contains("merge"), "{err}");
    }

    #[test]
    fn yaml_duplicate_key_is_refused() {
        let err = yaml("a:\n  k: v\n  k: w\n").unwrap_err();
        assert!(err.to_lowercase().contains("duplicate"), "{err}");
    }

    #[test]
    fn yaml_second_document_is_refused() {
        let err = yaml("a:\n  k: v\n---\nb:\n  k: w\n").unwrap_err();
        assert!(err.to_lowercase().contains("document"), "{err}");
    }

    #[test]
    fn yaml_unsupported_tag_is_refused() {
        assert!(yaml("a: !!python/object\n  k: v\n").is_err());
    }

    #[test]
    fn oversized_file_is_refused_in_both_formats() {
        let dir = crate::test_util::temp_dir("document");
        for name in ["big.toml", "big.yaml"] {
            let path = dir.join(name);
            let mut text = String::from("# padding\n");
            while (text.len() as u64) <= MAX_BYTES {
                text.push_str(
                    "# xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\n",
                );
            }
            fs::write(&path, text).unwrap();
            let err = load::<Doc>(&doc(&path)).unwrap_err();
            assert!(matches!(err, DocumentError::TooLarge { .. }), "{err}");
            assert!(err.to_string().contains(name));
        }
        fs::remove_dir_all(&dir).unwrap();
    }
}
