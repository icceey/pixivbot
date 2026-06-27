// Debug: dump gallery page HTML to inspect archiver key location
#[tokio::test]
#[ignore]
async fn debug_dump_gallery_html() {
    let gid: u64 = 4006958;
    let token = "586ff41111";

    let client = reqwest::Client::builder()
        .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36")
        .build()
        .unwrap();

    let url = format!("https://e-hentai.org/g/{}/{}/", gid, token);
    let resp = client
        .get(&url)
        .header("Cookie", "nw=1")
        .send()
        .await
        .expect("fetch failed");

    let status = resp.status();
    println!("Status: {}", status);
    let html = resp.text().await.expect("read failed");

    // Print lines containing "archiver" or "archive" or "popUp"
    for (i, line) in html.lines().enumerate() {
        let lower = line.to_lowercase();
        if lower.contains("archiver") || lower.contains("archive") || lower.contains("popup") {
            println!("L{}: {}", i, line.trim());
        }
    }
    println!("\n--- Total HTML length: {} ---", html.len());
}
