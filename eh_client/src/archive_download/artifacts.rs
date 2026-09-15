use crate::error::Result;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveArtifacts {
    final_zip: PathBuf,
    assembly_scratch: PathBuf,
    parts_dir: PathBuf,
    uploads_dir: PathBuf,
    original_fallback_marker: PathBuf,
}

impl ArchiveArtifacts {
    pub fn new(final_zip: impl Into<PathBuf>) -> Self {
        let final_zip = final_zip.into();
        Self {
            assembly_scratch: final_zip.with_extension("zip.part"),
            parts_dir: final_zip.with_extension("zip.parts"),
            uploads_dir: final_zip.with_extension("zip.uploads"),
            original_fallback_marker: final_zip.with_extension("zip.original"),
            final_zip,
        }
    }

    pub fn from_member(path: &Path) -> Option<Self> {
        let name = path.file_name()?.to_str()?;
        let final_name = if let Some(name) = name.strip_suffix(".zip.original") {
            format!("{name}.zip")
        } else if let Some(name) = name.strip_suffix(".zip.parts") {
            format!("{name}.zip")
        } else if let Some(name) = name.strip_suffix(".zip.uploads") {
            format!("{name}.zip")
        } else if let Some(name) = name.strip_suffix(".zip.part") {
            format!("{name}.zip")
        } else if name.ends_with(".zip") {
            name.to_owned()
        } else {
            return None;
        };
        Some(Self::new(path.with_file_name(final_name)))
    }

    pub fn final_zip(&self) -> &Path {
        &self.final_zip
    }

    pub fn assembly_scratch(&self) -> &Path {
        &self.assembly_scratch
    }

    pub fn parts_dir(&self) -> &Path {
        &self.parts_dir
    }

    pub fn uploads_dir(&self) -> &Path {
        &self.uploads_dir
    }

    /// Present when the archive was selected by automatic original fallback.
    /// Kept across transfer resets so a later resample cannot reuse its prefix.
    pub fn original_fallback_marker(&self) -> &Path {
        &self.original_fallback_marker
    }

    pub async fn remove_assembly_scratch(&self) -> Result<()> {
        remove_file_if_present(&self.assembly_scratch).await
    }

    pub async fn remove_parts_dir(&self) -> Result<()> {
        remove_dir_if_present(&self.parts_dir).await
    }

    pub async fn remove_upload_state(&self) -> Result<()> {
        remove_dir_if_present(&self.uploads_dir).await
    }

    pub async fn remove_multipart_state(&self) -> Result<()> {
        let assembly_result = self.remove_assembly_scratch().await;
        let parts_result = self.remove_parts_dir().await;
        assembly_result?;
        parts_result
    }

    pub async fn remove_all(&self) -> Result<()> {
        let final_result = remove_file_if_present(&self.final_zip).await;
        let assembly_result = self.remove_assembly_scratch().await;
        let parts_result = self.remove_parts_dir().await;
        let uploads_result = self.remove_upload_state().await;
        final_result?;
        assembly_result?;
        parts_result?;
        uploads_result?;
        remove_file_if_present(&self.original_fallback_marker).await
    }
}

async fn remove_file_if_present(path: &Path) -> Result<()> {
    match tokio::fs::remove_file(path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

async fn remove_dir_if_present(path: &Path) -> Result<()> {
    match tokio::fs::remove_dir_all(path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}
