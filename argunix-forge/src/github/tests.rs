use super::*;
use crate::events::{NormalizedEvent, PullRequestAction};
use crate::{CheckPost, CheckState};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn sign(secret: &[u8], body: &[u8]) -> String {
    type HmacSha256 = Hmac<Sha256>;
    let mut mac = HmacSha256::new_from_slice(secret).unwrap();
    mac.update(body);
    format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
}

#[tokio::test]
async fn verify_signature_accepts_valid_hmac() {
    let provider = GithubProvider::new(
        "http://unused".into(),
        "tok".into(),
        "https://argunix.example".into(),
    );
    let body = b"hello body";
    let secret = b"shh";
    let sig = sign(secret, body);
    let headers = vec![("X-Hub-Signature-256".to_string(), sig)];
    provider
        .verify_signature(&headers, body, secret)
        .await
        .unwrap();
}

#[tokio::test]
async fn verify_signature_rejects_wrong_secret() {
    let provider = GithubProvider::new(
        "http://unused".into(),
        "tok".into(),
        "https://argunix.example".into(),
    );
    let body = b"hello";
    let sig = sign(b"correct", body);
    let headers = vec![("X-Hub-Signature-256".to_string(), sig)];
    let err = provider
        .verify_signature(&headers, body, b"wrong")
        .await
        .unwrap_err();
    assert!(matches!(err, ForgeError::BadSignature));
}

#[tokio::test]
async fn verify_signature_rejects_missing_header() {
    let provider = GithubProvider::new(
        "http://unused".into(),
        "tok".into(),
        "https://argunix.example".into(),
    );
    let err = provider
        .verify_signature(&[], b"x", b"secret")
        .await
        .unwrap_err();
    assert!(matches!(err, ForgeError::MissingHeader(_)));
}

#[tokio::test]
async fn verify_signature_rejects_missing_prefix() {
    let provider = GithubProvider::new(
        "http://unused".into(),
        "tok".into(),
        "https://argunix.example".into(),
    );
    let body = b"x";
    let secret = b"s";
    let mut sig = sign(secret, body);
    sig = sig.strip_prefix("sha256=").unwrap().to_string();
    let headers = vec![("X-Hub-Signature-256".to_string(), sig)];
    let err = provider
        .verify_signature(&headers, body, secret)
        .await
        .unwrap_err();
    assert!(matches!(err, ForgeError::InvalidHeader { .. }));
}

#[tokio::test]
async fn parses_push_event() {
    let provider = GithubProvider::new(
        "http://unused".into(),
        "tok".into(),
        "https://argunix.example".into(),
    );
    let body = serde_json::json!({
        "ref": "refs/heads/main",
        "after": "0123456789abcdef0123456789abcdef01234567",
        "repository": { "full_name": "myorg/myrepo" },
        "pusher": { "name": "alice" }
    })
    .to_string();
    let headers = vec![("X-GitHub-Event".to_string(), "push".to_string())];
    let evt = provider
        .parse_event(&headers, body.as_bytes())
        .await
        .unwrap()
        .unwrap();
    let NormalizedEvent::Push(push) = evt else {
        panic!("expected push, got {evt:?}");
    };
    assert_eq!(push.slug.as_str(), "myorg/myrepo");
    assert_eq!(push.git_ref, "refs/heads/main");
    assert_eq!(
        push.sha.as_str(),
        "0123456789abcdef0123456789abcdef01234567"
    );
    assert_eq!(push.pusher.as_deref(), Some("alice"));
}

#[tokio::test]
async fn parses_pull_request_event_from_fork() {
    let provider = GithubProvider::new(
        "http://unused".into(),
        "tok".into(),
        "https://argunix.example".into(),
    );
    let body = serde_json::json!({
        "action": "opened",
        "number": 42,
        "repository": { "full_name": "myorg/myrepo" },
        "pull_request": {
            "user": { "login": "stranger" },
            "head": {
                "ref": "feature-x",
                "sha": "1111111111111111111111111111111111111111",
                "repo": { "full_name": "stranger/myrepo-fork" }
            },
            "base": {
                "ref": "main",
                "sha": "2222222222222222222222222222222222222222",
                "repo": { "full_name": "myorg/myrepo" }
            }
        }
    })
    .to_string();
    let headers = vec![("X-GitHub-Event".to_string(), "pull_request".to_string())];
    let evt = provider
        .parse_event(&headers, body.as_bytes())
        .await
        .unwrap()
        .unwrap();
    let NormalizedEvent::PullRequest(pr) = evt else {
        panic!("expected pull_request, got {evt:?}");
    };
    assert_eq!(pr.slug.as_str(), "myorg/myrepo");
    assert_eq!(pr.pr_number, 42);
    assert_eq!(
        pr.head_sha.as_str(),
        "1111111111111111111111111111111111111111"
    );
    assert_eq!(pr.author, "stranger");
    assert_eq!(pr.action, PullRequestAction::Opened);
    assert!(pr.is_fork);
    assert!(pr.action.should_evaluate());
}

#[tokio::test]
async fn parses_pull_request_from_same_repo_branch_is_not_fork() {
    let provider = GithubProvider::new(
        "http://unused".into(),
        "tok".into(),
        "https://argunix.example".into(),
    );
    let body = serde_json::json!({
        "action": "synchronize",
        "number": 7,
        "repository": { "full_name": "myorg/myrepo" },
        "pull_request": {
            "user": { "login": "alice" },
            "head": {
                "ref": "feature",
                "sha": "1111111111111111111111111111111111111111",
                "repo": { "full_name": "myorg/myrepo" }
            },
            "base": {
                "ref": "main",
                "sha": "2222222222222222222222222222222222222222",
                "repo": { "full_name": "myorg/myrepo" }
            }
        }
    })
    .to_string();
    let headers = vec![("X-GitHub-Event".to_string(), "pull_request".to_string())];
    let evt = provider
        .parse_event(&headers, body.as_bytes())
        .await
        .unwrap()
        .unwrap();
    let NormalizedEvent::PullRequest(pr) = evt else {
        panic!("not a pr");
    };
    assert!(!pr.is_fork);
    assert_eq!(pr.action, PullRequestAction::Synchronize);
}

#[tokio::test]
async fn ping_event_is_dropped() {
    let provider = GithubProvider::new(
        "http://unused".into(),
        "tok".into(),
        "https://argunix.example".into(),
    );
    let headers = vec![("X-GitHub-Event".to_string(), "ping".to_string())];
    assert!(
        provider
            .parse_event(&headers, b"{}")
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn unknown_event_is_dropped() {
    let provider = GithubProvider::new(
        "http://unused".into(),
        "tok".into(),
        "https://argunix.example".into(),
    );
    let headers = vec![("X-GitHub-Event".to_string(), "deployment".to_string())];
    assert!(
        provider
            .parse_event(&headers, b"{}")
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn missing_event_header_errors() {
    let provider = GithubProvider::new(
        "http://unused".into(),
        "tok".into(),
        "https://argunix.example".into(),
    );
    let err = provider.parse_event(&[], b"{}").await.unwrap_err();
    assert!(matches!(err, ForgeError::MissingHeader(_)));
}

#[tokio::test]
async fn query_user_permission_resolves_write() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/myorg/myrepo/collaborators/alice/permission"))
        .and(header("Authorization", "Bearer tok"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "permission": "write",
            "user": { "login": "alice" }
        })))
        .mount(&server)
        .await;

    let provider = GithubProvider::new(server.uri(), "tok".into(), "https://m".into());
    let perm = provider
        .query_user_permission(&Slug::new("myorg/myrepo").unwrap(), "alice")
        .await
        .unwrap();
    assert_eq!(perm, Permission::Write);
    assert!(perm.can_trigger_ci());
}

#[tokio::test]
async fn query_user_permission_404_means_none() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(
            "/repos/myorg/myrepo/collaborators/stranger/permission",
        ))
        .respond_with(ResponseTemplate::new(404).set_body_json(serde_json::json!({})))
        .mount(&server)
        .await;

    let provider = GithubProvider::new(server.uri(), "tok".into(), "https://m".into());
    let perm = provider
        .query_user_permission(&Slug::new("myorg/myrepo").unwrap(), "stranger")
        .await
        .unwrap();
    assert_eq!(perm, Permission::None);
    assert!(!perm.can_trigger_ci());
}

#[tokio::test]
async fn query_user_permission_401_surfaces_unauthorised() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/myorg/myrepo/collaborators/alice/permission"))
        .respond_with(ResponseTemplate::new(401).set_body_json(serde_json::json!({})))
        .mount(&server)
        .await;

    let provider = GithubProvider::new(server.uri(), "tok".into(), "https://m".into());
    let err = provider
        .query_user_permission(&Slug::new("myorg/myrepo").unwrap(), "alice")
        .await
        .unwrap_err();
    assert!(matches!(err, ForgeError::Unauthorised));
}

#[tokio::test]
async fn fetch_merge_ref_returns_sha_when_available() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/myorg/myrepo/pulls/42"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "merge_commit_sha": "0123456789abcdef0123456789abcdef01234567"
        })))
        .mount(&server)
        .await;

    let provider = GithubProvider::new(server.uri(), "tok".into(), "https://m".into());
    let sha = provider
        .fetch_merge_ref(&Slug::new("myorg/myrepo").unwrap(), 42)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(sha.as_str(), "0123456789abcdef0123456789abcdef01234567");
}

#[tokio::test]
async fn fetch_merge_ref_returns_none_when_pending() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/myorg/myrepo/pulls/42"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "merge_commit_sha": null
        })))
        .mount(&server)
        .await;

    let provider = GithubProvider::new(server.uri(), "tok".into(), "https://m".into());
    let sha = provider
        .fetch_merge_ref(&Slug::new("myorg/myrepo").unwrap(), 42)
        .await
        .unwrap();
    assert!(sha.is_none());
}

#[test]
fn clone_url_for_saas_github() {
    let p = GithubProvider::new(
        "https://api.github.com".into(),
        "ghp_xxx".into(),
        "https://m".into(),
    );
    let url = p.clone_url(&Slug::new("myorg/myrepo").unwrap());
    assert_eq!(
        url,
        "https://x-access-token:ghp_xxx@github.com/myorg/myrepo.git"
    );
}

#[test]
fn clone_url_for_enterprise_github() {
    let p = GithubProvider::new(
        "https://gh.example.com/api/v3".into(),
        "tok".into(),
        "https://m".into(),
    );
    let url = p.clone_url(&Slug::new("team/proj").unwrap());
    assert_eq!(
        url,
        "https://x-access-token:tok@gh.example.com/team/proj.git"
    );
}

#[tokio::test]
async fn post_check_succeeds() {
    use wiremock::matchers::body_json;
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(
            "/repos/myorg/myrepo/statuses/0123456789abcdef0123456789abcdef01234567",
        ))
        .and(body_json(serde_json::json!({
            "state": "pending",
            "context": "argunix: evaluating",
            "description": "kicking off eval",
            "target_url": "https://argunix.example/r/github/myorg/myrepo/eval/1"
        })))
        .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
            "id": 12345,
            "state": "pending"
        })))
        .mount(&server)
        .await;

    let provider = GithubProvider::new(server.uri(), "tok".into(), "https://m".into());
    let handle = provider
        .post_check(CheckPost {
            slug: Slug::new("myorg/myrepo").unwrap(),
            sha: Sha::new("0123456789abcdef0123456789abcdef01234567").unwrap(),
            context: "argunix: evaluating".to_string(),
            state: CheckState::Pending,
            description: Some("kicking off eval".to_string()),
            target_url: Some("https://argunix.example/r/github/myorg/myrepo/eval/1".to_string()),
        })
        .await
        .unwrap();
    assert_eq!(handle.0, "12345");
}
