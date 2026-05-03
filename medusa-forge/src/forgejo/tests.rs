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
    hex::encode(mac.finalize().into_bytes())
}

#[tokio::test]
async fn verify_signature_accepts_x_gitea_signature() {
    let p = ForgejoProvider::new(
        "http://unused".into(),
        "tok".into(),
        "https://m".into(),
    );
    let body = b"hello";
    let secret = b"shh";
    let sig = sign(secret, body);
    let headers = vec![("X-Gitea-Signature".to_string(), sig)];
    p.verify_signature(&headers, body, secret).await.unwrap();
}

#[tokio::test]
async fn verify_signature_accepts_x_forgejo_signature() {
    let p = ForgejoProvider::new(
        "http://unused".into(),
        "tok".into(),
        "https://m".into(),
    );
    let body = b"hello";
    let secret = b"shh";
    let sig = sign(secret, body);
    let headers = vec![("X-Forgejo-Signature".to_string(), sig)];
    p.verify_signature(&headers, body, secret).await.unwrap();
}

#[tokio::test]
async fn verify_signature_rejects_wrong_secret() {
    let p = ForgejoProvider::new(
        "http://unused".into(),
        "tok".into(),
        "https://m".into(),
    );
    let body = b"hello";
    let sig = sign(b"correct", body);
    let headers = vec![("X-Gitea-Signature".to_string(), sig)];
    let err = p
        .verify_signature(&headers, body, b"wrong")
        .await
        .unwrap_err();
    assert!(matches!(err, ForgeError::BadSignature));
}

#[tokio::test]
async fn parses_push_event() {
    let p = ForgejoProvider::new(
        "http://unused".into(),
        "tok".into(),
        "https://m".into(),
    );
    let body = serde_json::json!({
        "ref": "refs/heads/main",
        "after": "0123456789abcdef0123456789abcdef01234567",
        "repository": { "full_name": "alice/myrepo" },
        "pusher": { "username": "alice" }
    })
    .to_string();
    let headers = vec![("X-Gitea-Event".to_string(), "push".to_string())];
    let evt = p
        .parse_event(&headers, body.as_bytes())
        .await
        .unwrap()
        .unwrap();
    let NormalizedEvent::Push(push) = evt else { panic!("expected push") };
    assert_eq!(push.slug.as_str(), "alice/myrepo");
    assert_eq!(push.git_ref, "refs/heads/main");
    assert_eq!(push.pusher.as_deref(), Some("alice"));
}

#[tokio::test]
async fn parses_pull_request_event() {
    let p = ForgejoProvider::new(
        "http://unused".into(),
        "tok".into(),
        "https://m".into(),
    );
    let body = serde_json::json!({
        "action": "opened",
        "number": 7,
        "repository": { "full_name": "alice/myrepo" },
        "pull_request": {
            "user": { "username": "alice" },
            "head": {
                "ref": "feature-x",
                "sha": "1111111111111111111111111111111111111111",
                "repo": { "full_name": "alice/myrepo" }
            },
            "base": {
                "ref": "main",
                "sha": "2222222222222222222222222222222222222222",
                "repo": { "full_name": "alice/myrepo" }
            }
        }
    })
    .to_string();
    let headers = vec![("X-Gitea-Event".to_string(), "pull_request".to_string())];
    let evt = p
        .parse_event(&headers, body.as_bytes())
        .await
        .unwrap()
        .unwrap();
    let NormalizedEvent::PullRequest(pr) = evt else { panic!("expected PR") };
    assert_eq!(pr.author, "alice");
    assert_eq!(pr.action, PullRequestAction::Opened);
    assert!(!pr.is_fork);
}

#[tokio::test]
async fn ping_event_is_dropped() {
    let p = ForgejoProvider::new(
        "http://unused".into(),
        "tok".into(),
        "https://m".into(),
    );
    let headers = vec![("X-Gitea-Event".to_string(), "ping".to_string())];
    assert!(
        p.parse_event(&headers, b"{}").await.unwrap().is_none()
    );
}

#[tokio::test]
async fn query_user_permission_resolves_write() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/alice/myrepo/collaborators/alice/permission"))
        .and(header("Authorization", "token tok"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "permission": "write",
            "user": { "login": "alice" }
        })))
        .mount(&server)
        .await;

    let p = ForgejoProvider::new(server.uri(), "tok".into(), "https://m".into());
    let perm = p
        .query_user_permission(&Slug::new("alice/myrepo").unwrap(), "alice")
        .await
        .unwrap();
    assert_eq!(perm, Permission::Write);
}

#[tokio::test]
async fn post_check_succeeds() {
    use wiremock::matchers::body_json;
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(
            "/repos/alice/myrepo/statuses/0123456789abcdef0123456789abcdef01234567",
        ))
        .and(body_json(serde_json::json!({
            "state": "pending",
            "context": "medusa: evaluation",
            "description": "evaluating…",
            "target_url": "https://m/r/forgejo/alice/myrepo/eval/1"
        })))
        .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
            "id": 42,
            "state": "pending"
        })))
        .mount(&server)
        .await;

    let p = ForgejoProvider::new(server.uri(), "tok".into(), "https://m".into());
    let handle = p
        .post_check(CheckPost {
            slug: Slug::new("alice/myrepo").unwrap(),
            sha: Sha::new("0123456789abcdef0123456789abcdef01234567").unwrap(),
            context: "medusa: evaluation".to_string(),
            state: CheckState::Pending,
            description: Some("evaluating…".to_string()),
            target_url: Some("https://m/r/forgejo/alice/myrepo/eval/1".to_string()),
        })
        .await
        .unwrap();
    assert_eq!(handle.0, "42");
}

#[test]
fn clone_url_strips_api_v1() {
    let p = ForgejoProvider::new(
        "https://forge.example.com/api/v1".into(),
        "tok".into(),
        "https://m".into(),
    );
    let url = p.clone_url(&Slug::new("alice/myrepo").unwrap());
    assert_eq!(url, "https://medusa:tok@forge.example.com/alice/myrepo.git");
}
