use super::{pool_image_batch, BooruSiteRegistry};
use crate::config::BooruSiteConfig;
use booru_client::{BooruClient, BooruEngineType, Error};
use serde_json::json;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn danbooru_pool_images_follow_metadata_order_across_pages() {
    let server = MockServer::start().await;
    let client = BooruClient::new(&server.uri(), BooruEngineType::Danbooru).unwrap();
    let expected: Vec<u64> = [201, 1].into_iter().chain(2..201).collect();

    Mock::given(method("GET"))
        .and(path("/pools/9.json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": 9, "post_count": expected.len(), "post_ids": expected,
        })))
        .expect(1)
        .mount(&server)
        .await;

    for (page, ids) in [("1", (2..=201).rev().collect::<Vec<_>>()), ("2", vec![1])] {
        let posts: Vec<_> = ids.into_iter().map(|id| json!({"id": id})).collect();
        Mock::given(method("GET"))
            .and(path("/posts.json"))
            .and(query_param("tags", "pool:9 status:any"))
            .and(query_param("page", page))
            .respond_with(ResponseTemplate::new(200).set_body_json(posts))
            .expect(1)
            .mount(&server)
            .await;
    }

    let posts = client.pool_images(9).await.unwrap();
    assert_eq!(
        posts.into_iter().map(|post| post.id).collect::<Vec<_>>(),
        expected
    );
}

#[tokio::test]
async fn danbooru_pool_images_fail_when_a_later_page_makes_no_progress() {
    let server = MockServer::start().await;
    let client = BooruClient::new(&server.uri(), BooruEngineType::Danbooru).unwrap();

    Mock::given(method("GET"))
        .and(path("/pools/9.json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": 9, "post_count": 2, "post_ids": [31, 22],
        })))
        .mount(&server)
        .await;
    for page in ["1", "2"] {
        Mock::given(method("GET"))
            .and(path("/posts.json"))
            .and(query_param("page", page))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([{"id": 22}])))
            .expect(1)
            .mount(&server)
            .await;
    }

    assert!(matches!(
        client.pool_images(9).await,
        Err(Error::Api { status: 0, .. })
    ));
}

#[tokio::test]
async fn moebooru_pool_images_follow_server_sequence_until_post_count() {
    let server = MockServer::start().await;
    let client = BooruClient::new(&server.uri(), BooruEngineType::Moebooru).unwrap();
    let first_page: Vec<_> = (1..=24)
        .rev()
        .map(|id| json!({"id": id, "created_at": "2025-01-01T00:00:00.000Z"}))
        .collect();
    let tail =
        json!({"id": 50, "created_at": 1735689600, "file_url": "https://media.invalid/end.webm"});
    for (page, posts) in [("1", first_page), ("2", vec![tail])] {
        Mock::given(method("GET"))
            .and(path("/pool/show.json"))
            .and(query_param("id", "9"))
            .and(query_param("page", page))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": 9, "post_count": 25, "posts": posts,
            })))
            .expect(1)
            .mount(&server)
            .await;
    }

    let posts = client.pool_images(9).await.unwrap();
    let expected: Vec<_> = (1..=24).rev().chain([50]).collect();
    assert_eq!(
        posts.iter().map(|post| post.id).collect::<Vec<_>>(),
        expected
    );
    assert_eq!(posts[0].created_at.unwrap().timestamp(), 1735689600);
    assert_eq!(posts[24].created_at, posts[0].created_at);
}

#[tokio::test]
async fn danbooru_pool_batches_bound_video_scans_and_resume_in_pool_order() {
    let server = MockServer::start().await;
    let site: BooruSiteConfig = serde_json::from_value(json!({
        "name": "danbooru", "engine_type": "danbooru", "base_url": server.uri(),
    }))
    .unwrap();
    let registry = BooruSiteRegistry::from_configs(&[site]);

    let videos = 101_u64..=197;
    let post_ids: Vec<u64> = [31, 8, 7]
        .into_iter()
        .chain(videos.clone())
        .chain([71, 70, 9])
        .collect();
    Mock::given(method("GET"))
        .and(path("/pools/9.json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": 9, "post_count": post_ids.len(), "post_ids": post_ids,
        })))
        .expect(1)
        .mount(&server)
        .await;
    let first_url = "https://media.invalid/first.jpg";
    let second_url = "https://media.invalid/second.png";
    let video_ids = videos
        .clone()
        .map(|id| id.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let video_posts: Vec<_> = videos
        .map(|id| json!({"id": id, "file_ext": "webm"}))
        .collect();
    for (ids, count, posts) in [
        (
            "31,8,7",
            "3",
            json!([
                {"id": 7, "file_url": second_url},
                {"id": 8, "file_ext": "webm"},
                {"id": 31, "is_deleted": true, "file_url": first_url, "file_ext": "JPG"},
            ]),
        ),
        (&video_ids, "97", json!(video_posts)),
    ] {
        Mock::given(method("GET"))
            .and(path("/posts.json"))
            .and(query_param("tags", format!("id:{ids} status:any")))
            .and(query_param("limit", count))
            .respond_with(ResponseTemplate::new(200).set_body_json(posts))
            .expect(1)
            .mount(&server)
            .await;
    }
    let (images, cursor) = pool_image_batch(&registry, "danbooru", 9, None, 3)
        .await
        .unwrap();
    assert_eq!(
        images,
        [
            ("31.jpg".to_owned(), first_url.to_owned()),
            ("7.png".to_owned(), second_url.to_owned()),
        ]
    );

    let third_url = "https://media.invalid/third.avif?original=1";
    let tail_url = "https://media.invalid/tail.webp";
    for (ids, posts) in [
        ("71", json!([{"id": 71, "file_ext": "mp4"}])),
        (
            "70,9",
            json!([
                {"id": 9, "file_url": tail_url},
                {"id": 70, "file_url": third_url},
            ]),
        ),
    ] {
        Mock::given(method("GET"))
            .and(path("/posts.json"))
            .and(query_param("tags", format!("id:{ids} status:any")))
            .respond_with(ResponseTemplate::new(200).set_body_json(posts))
            .expect(1)
            .mount(&server)
            .await;
    }
    let (images, cursor) = pool_image_batch(&registry, "danbooru", 9, Some(cursor.unwrap()), 1)
        .await
        .unwrap();
    assert_eq!(images, [("70.avif".to_owned(), third_url.to_owned())]);
    let saved_cursor = serde_json::to_string(&cursor.unwrap()).unwrap();
    let cursor = serde_json::from_str(&saved_cursor).unwrap();
    let (images, cursor) = pool_image_batch(&registry, "danbooru", 9, Some(cursor), 1)
        .await
        .unwrap();
    assert_eq!(images, [("9.webp".to_owned(), tail_url.to_owned())]);
    assert!(cursor.is_none());
}

#[tokio::test]
async fn moebooru_pool_batches_bound_page_scans_and_resume_buffered_images() {
    let server = MockServer::start().await;
    let site: BooruSiteConfig = serde_json::from_value(json!({
        "name": "moebooru", "engine_type": "moebooru", "base_url": server.uri(),
    }))
    .unwrap();
    let registry = BooruSiteRegistry::from_configs(&[site]);
    let jpeg_url = "https://media.invalid/first.jpeg";
    let gif_url = "https://media.invalid/second.gif";
    let bmp_url = "https://media.invalid/third.bmp";
    Mock::given(method("GET"))
        .and(path("/pool/show.json"))
        .and(query_param("id", "9"))
        .and(query_param("page", "1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": 9, "post_count": 12, "posts": [
                {"id": 12, "file_url": jpeg_url},
                {"id": 10, "file_url": "https://media.invalid/movie.webm"},
                {"id": 11, "file_url": gif_url},
                {"id": 9, "file_url": bmp_url},
            ],
        })))
        .expect(1)
        .mount(&server)
        .await;
    let (images, cursor) = pool_image_batch(&registry, "moebooru", 9, None, 2)
        .await
        .unwrap();
    assert_eq!(
        images,
        [
            ("12.jpeg".to_owned(), jpeg_url.to_owned()),
            ("11.gif".to_owned(), gif_url.to_owned())
        ]
    );
    let saved_cursor = serde_json::to_string(&cursor.unwrap()).unwrap();
    let cursor = serde_json::from_str(&saved_cursor).unwrap();
    let (images, cursor) = pool_image_batch(&registry, "moebooru", 9, Some(cursor), 1)
        .await
        .unwrap();
    assert_eq!(images, [("9.bmp".to_owned(), bmp_url.to_owned())]);

    for page in 2_u64..=6 {
        Mock::given(method("GET"))
            .and(path("/pool/show.json"))
            .and(query_param("id", "9"))
            .and(query_param("page", page.to_string()))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": 9, "post_count": 12, "posts": [{"id": page, "file_ext": "webm"}],
            })))
            .expect(1)
            .mount(&server)
            .await;
    }
    let (images, cursor) = pool_image_batch(&registry, "moebooru", 9, Some(cursor.unwrap()), 1)
        .await
        .unwrap();
    assert!(images.is_empty());

    let png_url = "https://media.invalid/fourth.png";
    let webp_url = "https://media.invalid/fifth.webp";
    Mock::given(method("GET"))
        .and(path("/pool/show.json"))
        .and(query_param("id", "9"))
        .and(query_param("page", "7"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": 9, "post_count": 12, "posts": [
                {"id": 21, "file_ext": "mp4"},
                {"id": 20, "file_url": png_url},
                {"id": 19, "file_url": webp_url},
            ],
        })))
        .expect(1)
        .mount(&server)
        .await;
    let (images, cursor) = pool_image_batch(&registry, "moebooru", 9, Some(cursor.unwrap()), 1)
        .await
        .unwrap();
    assert_eq!(images, [("20.png".to_owned(), png_url.to_owned())]);

    let (images, cursor) = pool_image_batch(&registry, "moebooru", 9, Some(cursor.unwrap()), 2)
        .await
        .unwrap();
    assert_eq!(images, [("19.webp".to_owned(), webp_url.to_owned())]);
    assert!(cursor.is_none());
}
