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
