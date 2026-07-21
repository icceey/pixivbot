pub mod archive_download;
pub mod client;
pub mod error;
pub mod models;
pub mod parser;
pub mod telegraph;

pub use archive_download::{ArchiveArtifacts, ArchiveDownloadOptions};
pub use client::{EhClient, EhClientBuilder};
pub use error::{Error, Result};
pub use models::{EhCategory, EhCookies, EhGallery, EhGalleryRef};
pub use telegraph::{
    rewrite_ipfs_gateway_nodes, CatboxUploader, CatboxUploaderConfig, ImageUploadConfig,
    ImageUploadInput, ImageUploadProvider, ImageUploader, IpfS3PreviewRewriteConfig, IpfS3Uploader,
    IpfS3UploaderConfig, PixiUploader, S3Uploader, S3UploaderConfig, TelegraphClient,
    TelegraphGalleryPageResult, TelegraphImageUrlPair, TelegraphRewriteData, TelegraphRewritePage,
    ZipArchiveUploadInput,
};
