// Real download test against e-hentai.org
// Run: cargo test --test real_download_test -- --nocapture --ignored

use eh_client::{EhClientBuilder, EhCookies};
use std::path::PathBuf;

#[tokio::test]
#[ignore]
async fn test_download_gallery_images_real() {
    let gid: u64 = 4006958;
    let token = "586ff41111";

    // Not logged in — will use direct image download path
    let client = EhClientBuilder::new()
        .base_url("https://e-hentai.org")
        .api_url("https://api.e-hentai.org/api.php")
        .cookies(EhCookies {
            nw: true,
            ..Default::default()
        })
        .build();

    println!("Not logged in, using direct image download");
    let dest = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("data")
        .join(format!("{}_images.zip", gid));

    // Ensure data dir exists
    if let Some(parent) = dest.parent() {
        let _ = tokio::fs::create_dir_all(parent).await;
    }

    match client.download_gallery_images(gid, token, &dest).await {
        Ok(bytes) => {
            println!("Downloaded {} bytes to {}", bytes, dest.display());
            // Verify the file exists and has reasonable size
            let metadata = tokio::fs::metadata(&dest).await.unwrap();
            println!("File size on disk: {} bytes", metadata.len());
            assert!(bytes > 0, "Should have downloaded some bytes");
        }
        Err(e) => {
            eprintln!("Download failed: {:#}", e);
            panic!("Download failed: {}", e);
        }
    }
}

#[tokio::test]
#[ignore]
async fn test_get_metadata_real() {
    let gid: u64 = 4006958;
    let token = "586ff41111";

    let client = EhClientBuilder::new()
        .base_url("https://e-hentai.org")
        .api_url("https://api.e-hentai.org/api.php")
        .cookies(EhCookies {
            nw: true,
            ..Default::default()
        })
        .build();

    let galleries = client.get_metadata(&[(gid, token)]).await.unwrap();
    assert_eq!(galleries.len(), 1);
    let g = &galleries[0];
    println!("Title: {}", g.title);
    println!("Title JPN: {:?}", g.title_jpn);
    println!("Category: {}", g.category);
    println!("Uploader: {}", g.uploader);
    println!("Posted: {}", g.posted);
    println!("Filecount: {}", g.filecount);
    println!("Filesize: {} bytes", g.filesize);
    println!("Rating: {}", g.rating);
    println!("Tags: {:?}", g.tags);
    println!("Thumb: {}", g.thumb);
}

#[tokio::test]
#[ignore]
async fn test_get_archiver_key_real() {
    let gid: u64 = 4006958;
    let token = "586ff41111";

    let client = EhClientBuilder::new()
        .base_url("https://e-hentai.org")
        .api_url("https://api.e-hentai.org/api.php")
        .cookies(EhCookies {
            nw: true,
            ..Default::default()
        })
        .build();

    match client.get_archiver_key(gid, token).await {
        Ok(key) => {
            println!("Archiver key: {}", key);
        }
        Err(e) => {
            eprintln!(
                "get_archiver_key failed (expected if not logged in): {:#}",
                e
            );
        }
    }
}
