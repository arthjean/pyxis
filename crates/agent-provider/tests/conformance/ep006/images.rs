use std::time::Duration;

use agent_core::auxiliary::images::{
    ImageBackground, ImageEditRequest, ImageGenerationRequest, ImageQuality, ImageUrl,
};
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
            .generate_image(&generation_request())
            .await
            .unwrap_err(),
        "image_generation",
    );

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let mut captures = Vec::new();
        for response in [
            HttpResponse {
                status: 200,
                headers: Vec::new(),
                body: json!({
                    "created": 1,
                    "data": [{"b64_json": "SU1BR0U="}],
                    "background": "opaque",
                    "quality": "medium",
                    "size": "1024x1024",
                    "output_format": "png",
                    "usage": {"total_tokens": 10},
                })
                .to_string()
                .into_bytes(),
            },
            HttpResponse {
                status: 200,
                headers: Vec::new(),
                body: json!({
                    "created": 2,
                    "data": [{"url": "https://assets.example.test/image.png?sig=signed-secret"}],
                    "output_format": "png",
                })
                .to_string()
                .into_bytes(),
            },
        ] {
            let (mut socket, _) = listener.accept().await.unwrap();
            let capture = read_http_request(&mut socket).await;
            write_http_response(&mut socket, &response).await;
            captures.push(capture);
        }
        captures
    });
    let provider = make_provider(&format!("http://{address}/v1/"), capabilities());
    let generated = auxiliary(&provider)
        .generate_image(&generation_request())
        .await
        .unwrap();
    let edited = auxiliary(&provider)
        .edit_image(&edit_request())
        .await
        .unwrap();
    let captures = server.await.unwrap();
    let generation_body: Value = serde_json::from_slice(&captures[0].body).unwrap();
    let edit_body: Value = serde_json::from_slice(&captures[1].body).unwrap();
    let actual = json!({
        "generation_path": captures[0].target,
        "generation_prompt": generation_body["prompt"],
        "generation_has_binary": generated.data[0].b64_json.is_some(),
        "metadata_marks_redaction": generated.metadata.was_redacted(),
        "format": generated.output_format,
        "edit_path": captures[1].target,
        "edit_image_count": edit_body["images"].as_array().unwrap().len(),
        "edit_has_mask": edit_body.get("mask").is_some(),
        "edit_has_signed_url": edited.data[0].url.is_some(),
        "debug_redacted": !format!("{generated:?} {edited:?}").contains("SU1BR0U="),
    });
    assert_eq!(actual, case.success, "{name}: images success fixture");

    let (base, server) = one_response_server(HttpResponse {
        status: case.error.status,
        headers: Vec::new(),
        body: b"provider unavailable".to_vec(),
    })
    .await;
    let provider = make_provider(&base, capabilities());
    assert_http_error(
        auxiliary(&provider)
            .generate_image(&generation_request())
            .await
            .unwrap_err(),
        &case.error,
    );
    server.await.unwrap();

    let (base, server) = stalling_server().await;
    let provider = make_provider_with_timeout(&base, capabilities(), Duration::from_millis(20));
    let error = tokio::time::timeout(
        Duration::from_millis(case.timeout.max_ms),
        auxiliary(&provider).generate_image(&generation_request()),
    )
    .await
    .expect("images timeout fixture exceeded its bound")
    .unwrap_err();
    assert_timeout(error, &case.timeout);
    server.abort();
}

fn capabilities() -> AuxiliaryCapabilities {
    AuxiliaryCapabilities {
        images: true,
        ..AuxiliaryCapabilities::default()
    }
}

fn generation_request() -> ImageGenerationRequest {
    ImageGenerationRequest {
        prompt: "a red fox".into(),
        background: Some(ImageBackground::Opaque),
        model: "gpt-image-1.5".into(),
        n: Some(1),
        quality: Some(ImageQuality::Medium),
        size: Some("1024x1024".into()),
        output_format: Some("png".into()),
    }
}

fn edit_request() -> ImageEditRequest {
    ImageEditRequest {
        images: vec![ImageUrl {
            image_url: "data:image/png;base64,SU1BR0U=".into(),
        }],
        mask: Some(ImageUrl {
            image_url: "data:image/png;base64,TUFTSw==".into(),
        }),
        prompt: "add a hat".into(),
        background: None,
        model: "gpt-image-1.5".into(),
        n: None,
        quality: None,
        size: None,
        output_format: None,
    }
}

#[test]
fn debug_omits_image_sources() {
    let image = ImageUrl {
        image_url: "https://example.test/image?sig=signed-secret".into(),
    };
    assert!(!format!("{image:?}").contains("signed-secret"));
}
