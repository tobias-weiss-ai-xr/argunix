use super::*;
use crate::events::{NormalizedEvent, PullRequestAction};
use crate::{CheckPost, CheckState};
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn verify_signature_accepts_matching_token() {
    let p = GitlabProvider::new(
        "http://unused".into(),
        "tok".into(),
        "https://m".into(),
    );
    let headers = vec![("X-Gitlab-Token".to_string(), "shh".to_string())];
    p.verify_signature(&headers, b"any body", b"shh").await.unwrap();
}

#[tokio::test]
async fn verify_signature_rejects_mismatched_token() {
    let p = GitlabProvider::new(
        "http://unused".into(),
        "tok".into(),
        "https://m".into(),
    );
    let headers = vec![("X-Gitlab-Token".to_string(), "wrong".to_string())];
    let err = p
        .verify_signature(&headers, b"any body", b"shh")
        .await
        .unwrap_err();
    assert!(matches!(err, ForgeError::BadSignature));
}

#[tokio::test]
async fn verify_signature_rejects_missing_header() {
    let p = GitlabProvider::new(
        "http://unused".into(),
        "tok".into(),
        "https://m".into(),
    );
    let err = p
        .verify_signature(&[], b"x", b"shh")
        .await
        .unwrap_err();
    assert!(matches!(err, ForgeError::MissingHeader(_)));
}

#[tokio::test]
async fn parses_push_event() {
    let p = GitlabProvider::new(
        "http://unused".into(),
        "tok".into(),
        "https://m".into(),
    );
    let body = serde_json::json!({
        "ref": "refs/heads/main",
        "after": "0123456789abcdef0123456789abcdef01234567",
        "project": { "path_with_namespace": "alice/myrepo" },
        "user_username": "alice"
    })
    .to_string();
    let headers = vec![("X-Gitlab-Event".to_string(), "Push Hook".to_string())];
    let evt = p
        .parse_event(&headers, body.as_bytes())
        .await
        .unwrap()
        .unwrap();
    let NormalizedEvent::Push(push) = evt else { panic!("expected push") };
    assert_eq!(push.slug.as_str(), "alice/myrepo");
    assert_eq!(push.pusher.as_deref(), Some("alice"));
}

#[tokio::test]
async fn parses_subgroup_push_slug() {
    let p = GitlabProvider::new(
        "http://unused".into(),
        "tok".into(),
        "https://m".into(),
    );
    let body = serde_json::json!({
        "ref": "refs/heads/main",
        "after": "0123456789abcdef0123456789abcdef01234567",
        "project": { "path_with_namespace": "myorg/marketing/site" },
        "user_username": "alice"
    })
    .to_string();
    let headers = vec![("X-Gitlab-Event".to_string(), "Push Hook".to_string())];
    let evt = p
        .parse_event(&headers, body.as_bytes())
        .await
        .unwrap()
        .unwrap();
    let NormalizedEvent::Push(push) = evt else { panic!("expected push") };
    assert_eq!(push.slug.as_str(), "myorg/marketing/site");
}

#[tokio::test]
async fn parses_merge_request_event() {
    let p = GitlabProvider::new(
        "http://unused".into(),
        "tok".into(),
        "https://m".into(),
    );
    let body = serde_json::json!({
        "project": { "path_with_namespace": "alice/myrepo" },
        "user": { "username": "stranger" },
        "object_attributes": {
            "iid": 42,
            "action": "open",
            "last_commit": { "id": "1111111111111111111111111111111111111111" },
            "source_branch": "feature-x",
            "target_branch": "main",
            "source": { "path_with_namespace": "stranger/myrepo-fork" },
            "target": { "path_with_namespace": "alice/myrepo" }
        }
    })
    .to_string();
    let headers = vec![(
        "X-Gitlab-Event".to_string(),
        "Merge Request Hook".to_string(),
    )];
    let evt = p
        .parse_event(&headers, body.as_bytes())
        .await
        .unwrap()
        .unwrap();
    let NormalizedEvent::PullRequest(pr) = evt else { panic!("expected MR") };
    assert_eq!(pr.pr_number, 42);
    assert_eq!(pr.author, "stranger");
    assert_eq!(pr.action, PullRequestAction::Opened);
    assert!(pr.is_fork);
}

#[tokio::test]
async fn merge_request_update_maps_to_synchronize() {
    let p = GitlabProvider::new(
        "http://unused".into(),
        "tok".into(),
        "https://m".into(),
    );
    let body = serde_json::json!({
        "project": { "path_with_namespace": "alice/myrepo" },
        "user": { "username": "alice" },
        "object_attributes": {
            "iid": 7,
            "action": "update",
            "last_commit": { "id": "1111111111111111111111111111111111111111" },
            "source_branch": "feature",
            "target_branch": "main"
        }
    })
    .to_string();
    let headers = vec![(
        "X-Gitlab-Event".to_string(),
        "Merge Request Hook".to_string(),
    )];
    let evt = p
        .parse_event(&headers, body.as_bytes())
        .await
        .unwrap()
        .unwrap();
    let NormalizedEvent::PullRequest(pr) = evt else { panic!("expected MR") };
    assert_eq!(pr.action, PullRequestAction::Synchronize);
}

#[tokio::test]
async fn pipeline_event_is_dropped() {
    let p = GitlabProvider::new(
        "http://unused".into(),
        "tok".into(),
        "https://m".into(),
    );
    let headers = vec![("X-Gitlab-Event".to_string(), "Pipeline Hook".to_string())];
    assert!(p.parse_event(&headers, b"{}").await.unwrap().is_none());
}

#[tokio::test]
async fn query_user_permission_resolves_developer_to_write() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/users"))
        .and(query_param("username", "alice"))
        .and(header("PRIVATE-TOKEN", "tok"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!([{ "id": 7 }])),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/projects/alice%2Fmyrepo/members/all/7"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": 7,
                "access_level": 30
            })),
        )
        .mount(&server)
        .await;

    let p = GitlabProvider::new(server.uri(), "tok".into(), "https://m".into());
    let perm = p
        .query_user_permission(&Slug::new("alice/myrepo").unwrap(), "alice")
        .await
        .unwrap();
    assert_eq!(perm, Permission::Write);
    assert!(perm.can_trigger_ci());
}

#[tokio::test]
async fn query_user_permission_unknown_user_is_none() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/users"))
        .and(query_param("username", "ghost"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
        .mount(&server)
        .await;

    let p = GitlabProvider::new(server.uri(), "tok".into(), "https://m".into());
    let perm = p
        .query_user_permission(&Slug::new("alice/myrepo").unwrap(), "ghost")
        .await
        .unwrap();
    assert_eq!(perm, Permission::None);
}

#[tokio::test]
async fn post_check_succeeds_for_subgroup_slug() {
    use wiremock::matchers::body_json;
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(
            "/projects/myorg%2Fmarketing%2Fsite/statuses/0123456789abcdef0123456789abcdef01234567",
        ))
        .and(body_json(serde_json::json!({
            "state": "pending",
            "name": "medusa: evaluation",
            "description": "evaluating…",
            "target_url": "https://m/r/gitlab/myorg/marketing/site/eval/1"
        })))
        .respond_with(
            ResponseTemplate::new(201).set_body_json(serde_json::json!({ "id": 99 })),
        )
        .mount(&server)
        .await;

    let p = GitlabProvider::new(server.uri(), "tok".into(), "https://m".into());
    let handle = p
        .post_check(CheckPost {
            slug: Slug::new("myorg/marketing/site").unwrap(),
            sha: Sha::new("0123456789abcdef0123456789abcdef01234567").unwrap(),
            context: "medusa: evaluation".to_string(),
            state: CheckState::Pending,
            description: Some("evaluating…".to_string()),
            target_url: Some("https://m/r/gitlab/myorg/marketing/site/eval/1".to_string()),
        })
        .await
        .unwrap();
    assert_eq!(handle.0, "99");
}

#[test]
fn clone_url_uses_oauth2_prefix() {
    let p = GitlabProvider::new(
        "https://gitlab.example.com/api/v4".into(),
        "glpat-xxx".into(),
        "https://m".into(),
    );
    let url = p.clone_url(&Slug::new("myorg/marketing/site").unwrap());
    assert_eq!(
        url,
        "https://oauth2:glpat-xxx@gitlab.example.com/myorg/marketing/site.git"
    );
}

#[test]
fn url_encode_segment_handles_slashes() {
    assert_eq!(url_encode_segment("alice/myrepo"), "alice%2Fmyrepo");
    assert_eq!(
        url_encode_segment("myorg/sub/marketing/site"),
        "myorg%2Fsub%2Fmarketing%2Fsite"
    );
    assert_eq!(url_encode_segment("plain"), "plain");
}
