//! A busy synchronous request must not stall unrelated HTTP work, even with
//! one async worker. A held SQLite transaction supplies deterministic pressure
//! without a model download or a machine-dependent expensive graph fixture.
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use graphicle_runtime::{
    api::{App, router},
    model,
    store::{Store, s},
};
use serde_json::json;
use std::{future::Future, task::Poll, time::Duration};
use tower::ServiceExt;

async fn health_during_busy_request(mcp: bool) {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).unwrap();
    let workspace = store.create_workspace("Async isolation").unwrap();
    let key = s(&workspace, "capability");
    let workspace_id = s(&workspace, "id");
    let repository = store
        .register(
            workspace_id,
            key,
            &json!({"project_key":"async","name":"Async"}),
        )
        .unwrap();
    let repository_id = s(&repository, "id");
    let app = router(
        App::new(
            store.clone(),
            model::Config::default(),
            vec!["localhost".into()],
        )
        .unwrap(),
    );
    let (path, body) = if mcp {
        (
            format!("/mcp/{workspace_id}/{repository_id}"),
            json!({"jsonrpc":"2.0","id":1,"method":"tools/call",
                "params":{"name":"repository_status","arguments":{}}}),
        )
    } else {
        (
            format!("/api/workspaces/{workspace_id}/repositories/command"),
            json!({"operation":"status","arguments":{"repository_id":repository_id}}),
        )
    };
    let request = Request::builder()
        .method("POST")
        .uri(path)
        .header("host", "localhost")
        .header("authorization", format!("Bearer {key}"))
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();

    let database = store.connect().unwrap();
    database.execute_batch("BEGIN IMMEDIATE").unwrap();
    let (release, held) = std::sync::mpsc::channel();
    let holder = std::thread::spawn(move || {
        // The independent watchdog also makes a regression terminate rather
        // than deadlock the single-thread runtime that must release the lock.
        let released_by_health = held.recv_timeout(Duration::from_secs(5)).is_ok();
        database.execute_batch("ROLLBACK").unwrap();
        released_by_health
    });

    let mut busy = std::pin::pin!(app.clone().oneshot(request));
    let first_poll = std::future::poll_fn(|cx| Poll::Ready(busy.as_mut().poll(cx))).await;
    let health = app
        .oneshot(
            Request::builder()
                .uri("/health")
                .header("host", "localhost")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let _ = release.send(());
    let responsive = holder.join().unwrap();
    assert_eq!(health.status(), StatusCode::OK);
    assert!(responsive, "the busy request blocked the async worker");
    assert!(
        first_poll.is_pending(),
        "the request must await the held lock"
    );
    assert_eq!(busy.await.unwrap().status(), StatusCode::OK);
}

#[tokio::test(flavor = "current_thread")]
async fn command_does_not_block_the_http_worker() {
    health_during_busy_request(false).await;
}

#[tokio::test(flavor = "current_thread")]
async fn mcp_does_not_block_the_http_worker() {
    health_during_busy_request(true).await;
}
