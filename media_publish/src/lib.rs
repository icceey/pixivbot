pub mod error;
pub mod telegraph;
pub mod uploader;

use std::sync::Arc;

pub use error::{Error, Result, UploadStateError};
pub use telegraph::{
    estimate_content_size, node_attr_str, rewrite_ipfs_gateway_nodes, split_for_pages, Node,
    TelegraphClient, TelegraphGalleryPageResult, TelegraphImageUrlPair, TelegraphRewriteData,
    TelegraphRewritePage,
};
pub use uploader::{
    CatboxUploader, CatboxUploaderConfig, ImageUploadConfig, ImageUploadInput, ImageUploadProvider,
    ImageUploader, IpfS3PreviewRewriteConfig, IpfS3Uploader, IpfS3UploaderConfig, PixiUploader,
    S3Uploader, S3UploaderConfig, UploadResumeContext, ZipArchiveUploadInput,
};

/// Source-agnostic Telegraph publishing service shared across Pixiv, Booru, and EH flows.
#[derive(Clone)]
pub struct TelegraphPublisher {
    pub client: Arc<TelegraphClient>,
    pub image_uploader: Arc<dyn ImageUploader>,
}

impl TelegraphPublisher {
    pub fn new(client: Arc<TelegraphClient>, image_uploader: Arc<dyn ImageUploader>) -> Self {
        Self {
            client,
            image_uploader,
        }
    }
}
