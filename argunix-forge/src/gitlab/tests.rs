use super::*;
use crate::events::{NormalizedEvent, PullRequestAction};
use crate::{CheckPost, CheckState};
use base64::Engine;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Helper: compute the same signature GitLab would send.
fn signing_token_signature(secret: &[u8], id: &str, ts: &str, body: &[u8]) -> String {
    type HmacSha256 = Hmac<Sha256>;
    let mut mac = HmacSha256::new_from_slice(secret).unwrap();
    mac.update(id.as_bytes());
    mac.update(b".");
    mac.update(ts.as_bytes());
    mac.update(b".");
    mac.update(body);
    let bytes = mac.finalize().into_bytes();
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

#[tokio::test]
async fn verify_signature_accepts_matching_token() {
    let p = GitlabProvider::new("http://unused".into(), "tok".into(), "https://m".into());
    let headers = vec![("X-Gitlab-Token".to_string(), "shh".to_string())];
    p.verify_signature(&headers, b"any body", b"shh")
        .await
        .unwrap();
}

#[tokio::test]
async fn verify_signature_rejects_mismatched_token() {
    let p = GitlabProvider::new("http://unused".into(), "tok".into(), "https://m".into());
    let headers = vec![("X-Gitlab-Token".to_string(), "wrong".to_string())];
    let err = p
        .verify_signature(&headers, b"any body", b"shh")
        .await
        .unwrap_err();
    assert!(matches!(err, ForgeError::BadSignature));
}

#[tokio::test]
async fn verify_signature_rejects_missing_header() {
    let p = GitlabProvider::new("http://unused".into(), "tok".into(), "https://m".into());
    let err = p.verify_signature(&[], b"x", b"shh").await.unwrap_err();
    assert!(matches!(err, ForgeError::MissingHeader(_)));
}

#[tokio::test]
async fn verify_signing_token_accepts_valid_signature() {
    let p = GitlabProvider::new("http://unused".into(), "tok".into(), "https://m".into());
    let body = br#"{"hello":"world"}"#;
    let id = "msg_2x9f";
    let ts = "1714742400";
    let secret = b"signing-token-bytes";
    let sig = signing_token_signature(secret, id, ts, body);
    let headers = vec![
        ("webhook-id".to_string(), id.to_string()),
        ("webhook-timestamp".to_string(), ts.to_string()),
        ("webhook-signature".to_string(), format!("v1,{sig}")),
    ];
    p.verify_signature(&headers, body, secret).await.unwrap();
}

#[tokio::test]
async fn verify_signing_token_accepts_one_of_multiple_v1_entries() {
    // Operators can rotate signing keys; GitLab then sends multiple
    // signatures separated by spaces. Any one matching is enough.
    let p = GitlabProvider::new("http://unused".into(), "tok".into(), "https://m".into());
    let body = b"body";
    let id = "abc";
    let ts = "1";
    let real = b"real-secret";
    let stale = b"stale-secret";
    let real_sig = signing_token_signature(real, id, ts, body);
    let stale_sig = signing_token_signature(stale, id, ts, body);
    let headers = vec![
        ("webhook-id".to_string(), id.to_string()),
        ("webhook-timestamp".to_string(), ts.to_string()),
        (
            "webhook-signature".to_string(),
            format!("v1,{stale_sig} v1,{real_sig}"),
        ),
    ];
    p.verify_signature(&headers, body, real).await.unwrap();
}

#[tokio::test]
async fn verify_signing_token_rejects_wrong_secret() {
    let p = GitlabProvider::new("http://unused".into(), "tok".into(), "https://m".into());
    let body = b"body";
    let id = "abc";
    let ts = "1";
    let sig = signing_token_signature(b"correct", id, ts, body);
    let headers = vec![
        ("webhook-id".to_string(), id.to_string()),
        ("webhook-timestamp".to_string(), ts.to_string()),
        ("webhook-signature".to_string(), format!("v1,{sig}")),
    ];
    let err = p
        .verify_signature(&headers, body, b"wrong")
        .await
        .unwrap_err();
    assert!(matches!(err, ForgeError::BadSignature));
}

#[tokio::test]
async fn verify_signing_token_rejects_when_companion_headers_missing() {
    let p = GitlabProvider::new("http://unused".into(), "tok".into(), "https://m".into());
    let headers = vec![("webhook-signature".to_string(), "v1,xxx".to_string())];
    let err = p
        .verify_signature(&headers, b"x", b"shh")
        .await
        .unwrap_err();
    assert!(matches!(err, ForgeError::MissingHeader(_)));
}

#[tokio::test]
async fn verify_signing_token_decodes_whsec_prefix() {
    // Standard Webhooks (and GitLab's signing token UI) format:
    // the secret arrives as `whsec_<base64>`; the actual HMAC key is
    // the base64-decoded suffix.
    let p = GitlabProvider::new("http://unused".into(), "tok".into(), "https://m".into());
    let body = b"some webhook body";
    let id = "msg_42";
    let ts = "1714742400";
    let raw_key: &[u8] = b"this-is-the-real-32-byte-hmac-ke";
    let prefixed = format!(
        "whsec_{}",
        base64::engine::general_purpose::STANDARD.encode(raw_key)
    );
    let sig = signing_token_signature(raw_key, id, ts, body);
    let headers = vec![
        ("webhook-id".to_string(), id.to_string()),
        ("webhook-timestamp".to_string(), ts.to_string()),
        ("webhook-signature".to_string(), format!("v1,{sig}")),
    ];
    p.verify_signature(&headers, body, prefixed.as_bytes())
        .await
        .unwrap();
}

#[tokio::test]
async fn verify_signing_token_ignores_unknown_schemes() {
    // A future GitLab might emit a `v2,...` entry alongside `v1,...`;
    // we should still validate as long as one v1 entry matches.
    let p = GitlabProvider::new("http://unused".into(), "tok".into(), "https://m".into());
    let body = b"body";
    let id = "abc";
    let ts = "1";
    let secret = b"shh";
    let sig = signing_token_signature(secret, id, ts, body);
    let headers = vec![
        ("webhook-id".to_string(), id.to_string()),
        ("webhook-timestamp".to_string(), ts.to_string()),
        (
            "webhook-signature".to_string(),
            format!("v2,unknown-format v1,{sig}"),
        ),
    ];
    p.verify_signature(&headers, body, secret).await.unwrap();
}

#[tokio::test]
async fn parses_push_event() {
    let p = GitlabProvider::new("http://unused".into(), "tok".into(), "https://m".into());
    let body = serde_json::json!({
        "ref": "refs/heads/main",
        "after": "0123456789abcdef0123456789abcdef01234567",
        "project": { "path_with_namespace": "alice/myrepo", "default_branch": "main" },
        "user_username": "alice"
    })
    .to_string();
    let headers = vec![("X-Gitlab-Event".to_string(), "Push Hook".to_string())];
    let evt = p
        .parse_event(&headers, body.as_bytes())
        .await
        .unwrap()
        .unwrap();
    let NormalizedEvent::Push(push) = evt else {
        panic!("expected push")
    };
    assert_eq!(push.slug.as_str(), "alice/myrepo");
    assert_eq!(push.pusher.as_deref(), Some("alice"));
    assert_eq!(push.repo_default_branch.as_deref(), Some("main"));
}

#[tokio::test]
async fn parses_subgroup_push_slug() {
    let p = GitlabProvider::new("http://unused".into(), "tok".into(), "https://m".into());
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
    let NormalizedEvent::Push(push) = evt else {
        panic!("expected push")
    };
    assert_eq!(push.slug.as_str(), "myorg/marketing/site");
}

#[tokio::test]
async fn parses_merge_request_event() {
    let p = GitlabProvider::new("http://unused".into(), "tok".into(), "https://m".into());
    let body = serde_json::json!({
        "project": { "path_with_namespace": "alice/myrepo", "default_branch": "main" },
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
    let NormalizedEvent::PullRequest(pr) = evt else {
        panic!("expected MR")
    };
    assert_eq!(pr.pr_number, 42);
    assert_eq!(pr.author, "stranger");
    assert_eq!(pr.action, PullRequestAction::Opened);
    assert!(pr.is_fork);
    assert_eq!(pr.repo_default_branch.as_deref(), Some("main"));
}

#[tokio::test]
async fn merge_request_update_maps_to_synchronize() {
    let p = GitlabProvider::new("http://unused".into(), "tok".into(), "https://m".into());
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
    let NormalizedEvent::PullRequest(pr) = evt else {
        panic!("expected MR")
    };
    assert_eq!(pr.action, PullRequestAction::Synchronize);
}

#[tokio::test]
async fn pipeline_event_is_dropped() {
    let p = GitlabProvider::new("http://unused".into(), "tok".into(), "https://m".into());
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
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([{ "id": 7 }])))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/projects/alice%2Fmyrepo/members/all/7"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": 7,
            "access_level": 30
        })))
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
            "name": "argunix: evaluation",
            "description": "evaluating…",
            "target_url": "https://m/r/gitlab/myorg/marketing/site/eval/1"
        })))
        .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({ "id": 99 })))
        .mount(&server)
        .await;

    let p = GitlabProvider::new(server.uri(), "tok".into(), "https://m".into());
    let handle = p
        .post_check(CheckPost {
            slug: Slug::new("myorg/marketing/site").unwrap(),
            sha: Sha::new("0123456789abcdef0123456789abcdef01234567").unwrap(),
            context: "argunix: evaluation".to_string(),
            state: CheckState::Pending,
            description: Some("evaluating…".to_string()),
            target_url: Some("https://m/r/gitlab/myorg/marketing/site/eval/1".to_string()),
        })
        .await
        .unwrap();
    assert_eq!(handle.0, "99");
}

#[tokio::test]
async fn post_check_swallows_gitlab_no_op_transition() {
    // GitLab's commit-status state machine 400s a no-op transition
    // (e.g. re-posting `pending` for an already-`pending` status, as
    // happens when an eval resumes after a coordinator restart). That
    // is not an argunix-side failure — `post_check` must treat it as
    // success rather than surfacing a `ForgeError`.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(
            "/projects/myorg%2Fmyrepo/statuses/0123456789abcdef0123456789abcdef01234567",
        ))
        .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
            "message": "Cannot transition status via :enqueue from :pending \
                        (Reason(s): Status cannot transition via \"enqueue\")"
        })))
        .mount(&server)
        .await;

    let p = GitlabProvider::new(server.uri(), "tok".into(), "https://m".into());
    let result = p
        .post_check(CheckPost {
            slug: Slug::new("myorg/myrepo").unwrap(),
            sha: Sha::new("0123456789abcdef0123456789abcdef01234567").unwrap(),
            context: "argunix: evaluation".to_string(),
            state: CheckState::Pending,
            description: None,
            target_url: None,
        })
        .await;
    assert!(
        result.is_ok(),
        "a GitLab no-op status transition must not surface as an error: {result:?}",
    );
}

#[test]
fn clone_url_uses_oauth2_prefix() {
    let p = GitlabProvider::new(
        "https://gitlab.example.com/api/v4".into(),
        "glpat-xxx".into(),
        "https://m".into(),
    );
    let url = p.clone_url(&Slug::new("myorg/marketing/site").unwrap());
    assert_eq!(url, "https://gitlab.example.com/myorg/marketing/site.git");
    let creds = p.clone_credentials().unwrap();
    assert_eq!(creds.username, "oauth2");
    assert_eq!(creds.token, "glpat-xxx");
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
