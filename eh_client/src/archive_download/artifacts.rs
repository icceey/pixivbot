use crate::error::Result;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveArtifacts {
    final_zip: PathBuf,
    assembly_scratch: PathBuf,
    parts_dir: PathBuf,
}

impl ArchiveArtifacts {
    pub fn new(final_zip: impl Into<PathBuf>) -> Self {
        let final_zip = final_zip.into();
        Self {
            assembly_scratch: final_zip.with_extension("zip.part"),
            parts_dir: final_zip.with_extension("zip.parts"),
            final_zip,
        }
    }

    pub fn from_member(path: &Path) -> Option<Self> {
        let name = path.file_name()?.to_str()?;
        let final_name = if let Some(name) = name.strip_suffix(".zip.parts") {
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

    pub async fn remove_assembly_scratch(&self) -> Result<()> {
        remove_file_if_present(&self.assembly_scratch).await
    }

    pub async fn remove_parts_dir(&self) -> Result<()> {
        remove_dir_if_present(&self.parts_dir).await
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
        final_result?;
        assembly_result?;
        parts_result
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

#[cfg(test)]
mod tests {
    use super::ArchiveArtifacts;

    #[test]
    fn archive_artifacts_derive_stable_family_paths_and_members() {
        let final_zip = std::path::Path::new("cache/12_token.zip");
        let artifacts = ArchiveArtifacts::new(final_zip);

        assert_eq!(artifacts.final_zip(), final_zip);
        assert_eq!(
            artifacts.assembly_scratch(),
            std::path::Path::new("cache/12_token.zip.part")
        );
        assert_eq!(
            artifacts.parts_dir(),
            std::path::Path::new("cache/12_token.zip.parts")
        );

        for member in [
            "cache/12_token.zip",
            "cache/12_token.zip.part",
            "cache/12_token.zip.parts",
        ] {
            assert_eq!(
                ArchiveArtifacts::from_member(std::path::Path::new(member)),
                Some(artifacts.clone())
            );
        }

        assert_eq!(
            ArchiveArtifacts::from_member(std::path::Path::new("cache/note.txt")),
            None
        );
    }

    #[tokio::test]
    async fn archive_artifacts_remove_all_recursively_and_idempotently() {
        let temp = tempfile::tempdir().unwrap();
        let artifacts = ArchiveArtifacts::new(temp.path().join("12_token.zip"));

        tokio::fs::write(artifacts.final_zip(), b"zip")
            .await
            .unwrap();
        tokio::fs::write(artifacts.assembly_scratch(), b"partial")
            .await
            .unwrap();
        tokio::fs::create_dir_all(artifacts.parts_dir())
            .await
            .unwrap();
        tokio::fs::write(artifacts.parts_dir().join("part-0000000000000000"), b"part")
            .await
            .unwrap();

        artifacts.remove_all().await.unwrap();
        artifacts.remove_all().await.unwrap();

        assert!(!artifacts.final_zip().exists());
        assert!(!artifacts.assembly_scratch().exists());
        assert!(!artifacts.parts_dir().exists());
    }
}
