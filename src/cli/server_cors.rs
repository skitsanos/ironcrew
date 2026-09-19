use axum::http::{self, HeaderName, HeaderValue, Method};
use tower_http::cors::{AllowOrigin, Any, CorsLayer};

use crate::api;
use crate::utils::error::{IronCrewError, Result};

fn restricted_layer() -> CorsLayer {
    CorsLayer::new()
        .allow_methods([Method::GET, Method::POST, Method::DELETE, Method::OPTIONS])
        .allow_headers([
            HeaderName::from_static("authorization"),
            HeaderName::from_static("content-type"),
            api::idempotency::IDEMPOTENCY_KEY_HEADER,
            api::idempotency::IDEMPOTENCY_RECOVERY_KEY_HEADER,
        ])
        .expose_headers([
            api::idempotency::IDEMPOTENCY_REPLAYED_HEADER,
            api::lifecycle::INSTANCE_ID_HEADER,
            http::header::RETRY_AFTER,
        ])
}

fn from_value(origins: Option<&str>) -> Result<CorsLayer> {
    let Some(origins) = origins else {
        return Ok(CorsLayer::new());
    };
    let entries = origins
        .split(',')
        .map(str::trim)
        .filter(|origin| !origin.is_empty())
        .collect::<Vec<_>>();
    if entries == ["*"] {
        return Ok(restricted_layer().allow_origin(Any));
    }
    if entries.contains(&"*") {
        return Err(IronCrewError::Validation(
            "IRONCREW_CORS_ORIGINS cannot combine '*' with named origins".into(),
        ));
    }
    let allowed = entries
        .into_iter()
        .map(|origin| {
            origin.parse::<HeaderValue>().map_err(|error| {
                IronCrewError::Validation(format!(
                    "Invalid IRONCREW_CORS_ORIGINS entry {origin:?}: {error}"
                ))
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(restricted_layer().allow_origin(AllowOrigin::list(allowed)))
}

pub(super) fn from_env() -> Result<CorsLayer> {
    match std::env::var("IRONCREW_CORS_ORIGINS") {
        Ok(origins) => from_value(Some(&origins)),
        Err(std::env::VarError::NotPresent) => from_value(None),
        Err(std::env::VarError::NotUnicode(_)) => Err(IronCrewError::Validation(
            "IRONCREW_CORS_ORIGINS must contain valid UTF-8".into(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;

    use axum::{Router, routing::get};

    use super::from_value;

    async fn spawn(layer: tower_http::cors::CorsLayer) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = Router::new()
            .route("/", get(|| async { "ok" }))
            .layer(layer);
        tokio::spawn(async move {
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .unwrap();
        });
        format!("http://{address}")
    }

    #[tokio::test]
    async fn wildcard_origin_keeps_methods_and_headers_restricted() {
        let base = spawn(from_value(Some("*")).unwrap()).await;
        let allowed = reqwest::Client::new()
            .request(reqwest::Method::OPTIONS, &base)
            .header("Origin", "https://example.com")
            .header("Access-Control-Request-Method", "POST")
            .header("Access-Control-Request-Headers", "authorization")
            .send()
            .await
            .unwrap();
        assert_eq!(allowed.headers()["access-control-allow-origin"], "*");
        assert!(
            allowed.headers()["access-control-allow-methods"]
                .to_str()
                .unwrap()
                .contains("POST")
        );
        assert!(
            allowed.headers()["access-control-allow-headers"]
                .to_str()
                .unwrap()
                .contains("authorization")
        );

        let denied = reqwest::Client::new()
            .request(reqwest::Method::OPTIONS, &base)
            .header("Origin", "https://example.com")
            .header("Access-Control-Request-Method", "PATCH")
            .header("Access-Control-Request-Headers", "x-arbitrary")
            .send()
            .await
            .unwrap();
        assert!(
            !denied.headers()["access-control-allow-methods"]
                .to_str()
                .unwrap()
                .contains("PATCH")
        );
        assert!(
            !denied.headers()["access-control-allow-headers"]
                .to_str()
                .unwrap()
                .contains("x-arbitrary")
        );
    }

    #[test]
    fn wildcard_cannot_be_combined_with_named_origins() {
        let Err(error) = from_value(Some("https://example.com, *")) else {
            panic!("mixed wildcard policy must fail");
        };
        assert!(error.to_string().contains("cannot combine"));
    }
}
