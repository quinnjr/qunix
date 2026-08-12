use std::path::PathBuf;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("network: {0}")]
    Network(String),

    #[error("index: {0}")]
    Index(String),

    #[error("{name} does not build for x86_64 (arch = {arches:?})")]
    UnsupportedArch { name: String, arches: Vec<String> },

    #[error("source handling: {0}")]
    Extraction(String),

    #[error("checksum mismatch for {file}: expected {expected}, got {got}")]
    ChecksumMismatch {
        file: String,
        expected: String,
        got: String,
    },

    #[error("{stage}() failed; full log at {}", log.display())]
    Build { stage: &'static str, log: PathBuf },

    #[error("package() output escapes $pkgdir: {}", .0.display())]
    Containment(PathBuf),

    #[error("required tool not on the host: {0}")]
    MissingTool(String),

    #[error(transparent)]
    Io(#[from] std::io::Error),
}
