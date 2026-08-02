use std::time::Duration;

use agent_core::auxiliary::AuxiliaryError;
use agent_core::auxiliary::files::FileUploadRequest;
use agent_core::provider::AuxiliaryCapabilities;
use serde_json::{Value, json};
use tokio::net::TcpListener;

use super::super::AuxiliaryCase;
use super::support::{
    HttpResponse, assert_http_error, assert_timeout, assert_unsupported, auxiliary, make_provider,
    make_provider_with_timeout, one_response_server, read_http_request, stalling_server,
    write_http_response,
};

pub(super) async fn assert_fixture(name: &str, case: &AuxiliaryCase) {
    let disabled = make_provider("http://127.0.0.1:9/v1/", AuxiliaryCapabilities::default());
    assert_unsupported(
        auxiliary(&disabled)
            .upload_file(request())
            .await
            .unwrap_err(),
        "file_upload",
    );

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let responses = [
            HttpResponse {
                status: 200,
                headers: Vec::new(),
                body: json!({
                    "file_id": "file_123",
                    "upload_url": format!("http://{address}/blob?sig=signed-secret"),
                })
                .to_string()
                .into_bytes(),
            },
            HttpResponse {
                status: 200,
                headers: vec![("x-ms-request-id".into(), "azure-req".into())],
                body: Vec::new(),
            },
            HttpResponse {
                status: 200,
                headers: Vec::new(),
                body: json!({
                    "status": "success",
                    "download_url": "https://assets.example.test/file?sig=signed-secret",
                    "file_name": "fixture.txt",
                    "mime_type": "text/plain",
                })
                .to_string()
                .into_bytes(),
            },
        ];
        let mut captures = Vec::new();
        for response in responses {
            let (mut socket, _) = listener.accept().await.unwrap();
            let capture = read_http_request(&mut socket).await;
            write_http_response(&mut socket, &response).await;
            captures.push(capture);
        }
        captures
    });
    let provider = make_provider(&format!("http://{address}/v1/"), capabilities());
    let uploaded = auxiliary(&provider).upload_file(request()).await.unwrap();
    let captures = server.await.unwrap();
    let create_body: Value = serde_json::from_slice(&captures[0].body).unwrap();
    let actual = json!({
        "create_path": captures[0].target,
        "create_name": create_body["file_name"],
        "blob_path": captures[1].target.split('?').next().unwrap(),
        "blob_auth": captures[1].headers.contains_key("authorization"),
        "blob_provider_header": captures[1].headers.contains_key("x-tenant"),
        "blob_request_id": captures[1].headers.contains_key("x-ms-client-request-id"),
        "blob_size": captures[1].body.len(),
        "finalize_path": captures[2].target,
        "uri": uploaded.uri,
        "download_debug_redacted": !format!("{uploaded:?}").contains("signed-secret"),
    });
    assert_eq!(actual, case.success, "{name}: files success fixture");

    let (base, server) = one_response_server(HttpResponse {
        status: case.error.status,
        headers: Vec::new(),
        body: b"provider unavailable".to_vec(),
    })
    .await;
    let provider = make_provider(&base, capabilities());
    assert_http_error(
        auxiliary(&provider)
            .upload_file(request())
            .await
            .unwrap_err(),
        &case.error,
    );
    server.await.unwrap();

    let (base, server) = stalling_server().await;
    let provider = make_provider_with_timeout(&base, capabilities(), Duration::from_millis(20));
    let error = tokio::time::timeout(
        Duration::from_millis(case.timeout.max_ms),
        auxiliary(&provider).upload_file(request()),
    )
    .await
    .expect("files timeout fixture exceeded its bound")
    .unwrap_err();
    assert_timeout(error, &case.timeout);
    server.abort();
}

fn capabilities() -> AuxiliaryCapabilities {
    AuxiliaryCapabilities {
        files: true,
        ..AuxiliaryCapabilities::default()
    }
}

fn request() -> FileUploadRequest {
    FileUploadRequest {
        file_name: "fixture.txt".into(),
        file_size_bytes: 4,
        mime_type: Some("text/plain".into()),
        contents: b"file".to_vec(),
    }
}

#[tokio::test]
async fn invalid_sizes_fail_before_network() {
    let provider = make_provider("http://127.0.0.1:9/v1/", capabilities());
    let oversized = auxiliary(&provider)
        .upload_file(FileUploadRequest {
            file_name: "large.bin".into(),
            file_size_bytes: 536_870_913,
            mime_type: None,
            contents: Vec::new(),
        })
        .await;
    assert!(matches!(
        oversized,
        Err(AuxiliaryError::FileTooLarge { .. })
    ));
    let mismatch = auxiliary(&provider)
        .upload_file(FileUploadRequest {
            file_name: "wrong.bin".into(),
            file_size_bytes: 2,
            mime_type: None,
            contents: vec![1],
        })
        .await;
    assert!(matches!(
        mismatch,
        Err(AuxiliaryError::FileSizeMismatch { .. })
    ));
}

#[tokio::test]
async fn finalize_failure_never_publishes_a_partial_resource() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let responses = [
            HttpResponse {
                status: 200,
                headers: Vec::new(),
                body: json!({
                    "file_id": "file_failed",
                    "upload_url": format!("http://{address}/blob?sig=private"),
                })
                .to_string()
                .into_bytes(),
            },
            HttpResponse {
                status: 200,
                headers: Vec::new(),
                body: Vec::new(),
            },
            HttpResponse {
                status: 200,
                headers: Vec::new(),
                body: json!({
                    "status": "failed",
                    "error_message": "failed at https://uploads.invalid/a?X-Amz-Signature=secret",
                })
                .to_string()
                .into_bytes(),
            },
        ];
        for response in responses {
            let (mut socket, _) = listener.accept().await.unwrap();
            read_http_request(&mut socket).await;
            write_http_response(&mut socket, &response).await;
        }
    });
    let provider = make_provider(&format!("http://{address}/v1/"), capabilities());
    let error = auxiliary(&provider)
        .upload_file(request())
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        AuxiliaryError::UploadFailed { reason, .. }
            if !reason.contains("secret") && reason.contains("REDACTED")
    ));
    server.await.unwrap();
}
