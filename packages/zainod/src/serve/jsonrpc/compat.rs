//! zcashd compatibility for the JSON-RPC server, as zebrad applies it: at
//! the HTTP level (authentication, content type, JSON-RPC 1.0 and
//! bitcoind-style requests) and per call (zcashd's invalid-params code).

use std::future::Future;
use std::pin::Pin;

use base64::{engine::general_purpose::STANDARD, Engine as _};
use futures::FutureExt as _;
use http::header;
use http_body_util::{BodyExt as _, Limited};
use jsonrpsee::core::BoxError;
use jsonrpsee::server::middleware::rpc::{layer::ResponseFuture, RpcServiceT};
use jsonrpsee::server::{HttpBody, HttpRequest, HttpResponse};
use jsonrpsee::types::{ErrorCode, ErrorObject, ErrorObjectOwned};
use jsonrpsee::MethodResponse;
use serde::{Deserialize, Serialize};

use super::cookie::Cookie;

/// zcashd's `RPC_MISC_ERROR`, which it answers for invalid parameters.
const MISC_ERROR_CODE: i32 = -1;

/// The HTTP layer: refuses unauthenticated requests, reads a missing or
/// `text/plain` content type as JSON, and speaks each client's JSON-RPC
/// version back to it.
#[derive(Clone)]
pub(super) struct HttpCompatLayer {
    cookie: Option<Cookie>,
    max_request_body_size: usize,
}

impl HttpCompatLayer {
    pub(super) fn new(cookie: Option<Cookie>, max_request_body_size: usize) -> Self {
        Self {
            cookie,
            max_request_body_size,
        }
    }
}

impl<S> tower::Layer<S> for HttpCompatLayer {
    type Service = HttpCompat<S>;

    fn layer(&self, service: S) -> Self::Service {
        HttpCompat {
            service,
            cookie: self.cookie.clone(),
            max_request_body_size: self.max_request_body_size,
        }
    }
}

/// The service [`HttpCompatLayer`] wraps a server in.
#[derive(Clone)]
pub(super) struct HttpCompat<S> {
    service: S,
    cookie: Option<Cookie>,
    max_request_body_size: usize,
}

/// Whether `headers` carry the cookie as a Basic password; always, with
/// authentication off.
fn authenticated(cookie: Option<&Cookie>, headers: &header::HeaderMap) -> bool {
    cookie.is_none_or(|cookie| {
        headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split_whitespace().nth(1))
            .and_then(|encoded| STANDARD.decode(encoded).ok())
            .and_then(|decoded| String::from_utf8(decoded).ok())
            .is_some_and(|pair| {
                pair.split(':')
                    .nth(1)
                    .is_some_and(|password| cookie.authenticate(password))
            })
    })
}

/// A missing or `text/plain` content type becomes `application/json`; any
/// other (a browser form's, say) is left for the server to refuse.
fn json_content_type(headers: &mut header::HeaderMap) {
    let plain_or_absent = headers.get(header::CONTENT_TYPE).is_none_or(|value| {
        value
            .to_str()
            .is_ok_and(|value| value.starts_with("text/plain"))
    });
    if plain_or_absent {
        headers.insert(
            header::CONTENT_TYPE,
            header::HeaderValue::from_static("application/json"),
        );
    }
}

/// `request` as JSON-RPC 2.0, and the version its client spoke.
async fn request_to_2(
    request: HttpRequest<HttpBody>,
    max_request_body_size: usize,
) -> Result<(Version, HttpRequest<HttpBody>), BoxError> {
    let (parts, body) = request.into_parts();
    let bytes = Limited::new(body, max_request_body_size)
        .collect()
        .await?
        .to_bytes();
    let rewritten = serde_json::from_slice::<Request>(&bytes)
        .ok()
        .and_then(|request| {
            let version = request.version();
            let body = match version {
                Version::Unknown => return None,
                _ => serde_json::to_vec(&request.into_2()).ok()?,
            };
            Some((version, body))
        });
    let (version, body) = rewritten.unwrap_or_else(|| (Version::Unknown, Vec::from(bytes)));
    Ok((
        version,
        HttpRequest::from_parts(parts, HttpBody::from(body)),
    ))
}

/// A JSON-RPC 2.0 `response` in the client's `version`.
async fn response_from_2(
    version: Version,
    response: HttpResponse<HttpBody>,
) -> Result<HttpResponse<HttpBody>, BoxError> {
    let (parts, body) = response.into_parts();
    let bytes = body.collect().await?.to_bytes();
    let body = serde_json::from_slice::<Response>(&bytes)
        .ok()
        .and_then(|response| serde_json::to_vec(&response.into_version(version)).ok())
        .unwrap_or_else(|| bytes.to_vec());
    Ok(HttpResponse::from_parts(parts, HttpBody::from(body)))
}

impl<S> tower::Service<HttpRequest<HttpBody>> for HttpCompat<S>
where
    S: tower::Service<HttpRequest, Response = HttpResponse> + Clone + Send + 'static,
    S::Error: Into<BoxError> + 'static,
    S::Future: Send + 'static,
{
    type Response = S::Response;
    type Error = BoxError;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        self.service.poll_ready(cx).map_err(Into::into)
    }

    fn call(&mut self, mut request: HttpRequest<HttpBody>) -> Self::Future {
        if !authenticated(self.cookie.as_ref(), request.headers()) {
            let error = ErrorObject::borrowed(401, "unauthenticated method", None);
            return futures::future::err(BoxError::from(error)).boxed();
        }
        json_content_type(request.headers_mut());
        let mut service = self.service.clone();
        let max_request_body_size = self.max_request_body_size;
        async move {
            let (version, request) = request_to_2(request, max_request_body_size).await?;
            let response = service.call(request).await.map_err(Into::into)?;
            response_from_2(version, response).await
        }
        .boxed()
    }
}

/// The JSON-RPC dialect a client spoke.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Version {
    /// bitcoind's mix of 1.0, 1.1 and 2.0: no `jsonrpc` key.
    Bitcoind,
    /// lightwalletd's: the same mix, with `"jsonrpc": "1.0"`.
    Lightwalletd,
    /// Strict 2.0.
    TwoPointZero,
    /// Not recognised: passed through for the server to answer.
    Unknown,
}

/// A request in any version.
#[derive(Debug, Deserialize, Serialize)]
struct Request {
    #[serde(skip_serializing_if = "Option::is_none")]
    jsonrpc: Option<String>,
    method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    params: Option<serde_json::Value>,
    #[serde(
        default,
        deserialize_with = "present_id",
        skip_serializing_if = "Option::is_none"
    )]
    id: Option<serde_json::Value>,
}

/// An `id` key that is present, `null` included.
fn present_id<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<serde_json::Value>, D::Error> {
    serde_json::Value::deserialize(deserializer).map(Some)
}

impl Request {
    fn version(&self) -> Version {
        use serde_json::Value;
        match (self.jsonrpc.as_deref(), &self.params, &self.id) {
            (Some("2.0"), _, None | Some(Value::Null | Value::String(_) | Value::Number(_))) => {
                Version::TwoPointZero
            }
            (Some("1.0"), Some(_), Some(_)) => Version::Lightwalletd,
            (None, Some(_), Some(_)) => Version::Bitcoind,
            _ => Version::Unknown,
        }
    }

    fn into_2(mut self) -> Self {
        self.jsonrpc = Some("2.0".to_owned());
        self
    }
}

/// A response in any version.
#[derive(Debug, Deserialize, Serialize)]
struct Response {
    #[serde(skip_serializing_if = "Option::is_none")]
    jsonrpc: Option<String>,
    id: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Box<serde_json::value::RawValue>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<serde_json::Value>,
}

impl Response {
    /// The 1.x dialects carry both `result` and `error`, one of them
    /// `null`; 2.0 carries an explicit `null` result when there is no error.
    fn into_version(mut self, version: Version) -> Self {
        let null_result = |result: Option<Box<serde_json::value::RawValue>>| {
            result.or_else(|| serde_json::value::to_raw_value(&()).ok())
        };
        match version {
            Version::Bitcoind | Version::Lightwalletd => {
                self.jsonrpc = (version == Version::Lightwalletd).then(|| "1.0".to_owned());
                self.result = null_result(self.result);
                self.error = self.error.or(Some(serde_json::Value::Null));
            }
            Version::TwoPointZero if self.error.is_none() => {
                self.result = null_result(self.result);
            }
            Version::TwoPointZero | Version::Unknown => {}
        }
        self
    }
}

/// The per-call layer: jsonrpsee's invalid-params code becomes zcashd's,
/// keeping the error's message and data.
#[derive(Clone)]
pub(super) struct ZcashdErrorCodes<S> {
    service: S,
}

impl<S> ZcashdErrorCodes<S> {
    pub(super) fn new(service: S) -> Self {
        Self { service }
    }
}

impl<'a, S> RpcServiceT<'a> for ZcashdErrorCodes<S>
where
    S: RpcServiceT<'a> + Send + Sync + Clone + 'static,
{
    type Future = ResponseFuture<futures::future::BoxFuture<'a, MethodResponse>>;

    fn call(&self, request: jsonrpsee::types::Request<'a>) -> Self::Future {
        let service = self.service.clone();
        let id = request.id.clone().into_owned();
        ResponseFuture::future(Box::pin(async move {
            let response = service.call(request).await;
            if response.as_error_code() != Some(ErrorCode::InvalidParams.code()) {
                return response;
            }
            let error = serde_json::from_str::<serde_json::Value>(response.as_result())
                .ok()
                .and_then(|json| {
                    serde_json::from_value::<ErrorObjectOwned>(json.get("error")?.clone()).ok()
                });
            match error {
                Some(error) => MethodResponse::error(
                    id,
                    ErrorObject::owned(MISC_ERROR_CODE, error.message(), error.data()),
                ),
                None => response,
            }
        }))
    }
}

#[cfg(test)]
mod tests {
    use jsonrpsee::types::Id;
    use serde_json::json;

    use super::*;

    fn headers(pairs: &[(header::HeaderName, &str)]) -> header::HeaderMap {
        pairs
            .iter()
            .map(|(name, value)| {
                (
                    name.clone(),
                    header::HeaderValue::from_str(value).expect("a header value"),
                )
            })
            .collect()
    }

    #[test]
    fn only_the_cookie_as_the_basic_password_authenticates() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let cookie = Cookie::generate();
        cookie.write_to(dir.path()).expect("written");
        let pair = std::fs::read_to_string(dir.path().join(".cookie")).expect("readable");
        let basic = |pair: &str| format!("Basic {}", STANDARD.encode(pair));

        let right = headers(&[(header::AUTHORIZATION, &basic(&pair))]);
        let wrong = headers(&[(header::AUTHORIZATION, &basic("__cookie__:nope"))]);
        assert!(authenticated(Some(&cookie), &right));
        assert!(!authenticated(Some(&cookie), &wrong));
        assert!(!authenticated(Some(&cookie), &header::HeaderMap::new()));
        assert!(authenticated(None, &header::HeaderMap::new()));
    }

    #[test]
    fn a_missing_or_plain_content_type_is_read_as_json() {
        let content_type = |given: Option<&str>| {
            let mut headers = given.map_or_else(header::HeaderMap::new, |given| {
                headers(&[(header::CONTENT_TYPE, given)])
            });
            json_content_type(&mut headers);
            headers
                .get(header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned)
        };
        let json = Some("application/json".to_owned());
        assert_eq!(content_type(None), json);
        assert_eq!(content_type(Some("text/plain; charset=utf-8")), json);
        assert_eq!(
            content_type(Some("application/x-www-form-urlencoded")).as_deref(),
            Some("application/x-www-form-urlencoded")
        );
    }

    #[test]
    fn each_dialect_is_recognised_and_answered_in_kind() {
        let version = |request: serde_json::Value| {
            serde_json::from_value::<Request>(request)
                .expect("a request")
                .version()
        };
        assert_eq!(
            version(json!({"jsonrpc": "2.0", "method": "m", "id": 1})),
            Version::TwoPointZero
        );
        assert_eq!(
            version(json!({"jsonrpc": "1.0", "method": "m", "params": [], "id": "x"})),
            Version::Lightwalletd
        );
        assert_eq!(
            version(json!({"method": "m", "params": [], "id": 1})),
            Version::Bitcoind
        );
        assert_eq!(version(json!({"method": "m"})), Version::Unknown);

        let answered = |version, response: serde_json::Value| {
            let response = serde_json::from_value::<Response>(response).expect("a response");
            serde_json::to_value(response.into_version(version)).expect("serialises")
        };
        let ok = json!({"jsonrpc": "2.0", "id": 1, "result": 5});
        assert_eq!(
            answered(Version::Bitcoind, ok.clone()),
            json!({"id": 1, "result": 5, "error": null})
        );
        assert_eq!(
            answered(Version::Lightwalletd, ok.clone()),
            json!({"jsonrpc": "1.0", "id": 1, "result": 5, "error": null})
        );
        assert_eq!(answered(Version::TwoPointZero, ok.clone()), ok);
        let failed = json!({"jsonrpc": "2.0", "id": 1, "error": {"code": -8, "message": "no"}});
        assert_eq!(
            answered(Version::Bitcoind, failed.clone()),
            json!({"id": 1, "result": null, "error": {"code": -8, "message": "no"}})
        );
        assert_eq!(answered(Version::TwoPointZero, failed.clone()), failed);
    }

    /// An inner service answering one error.
    #[derive(Clone)]
    struct Fails(i32);

    impl<'a> RpcServiceT<'a> for Fails {
        type Future = std::future::Ready<MethodResponse>;

        fn call(&self, request: jsonrpsee::types::Request<'a>) -> Self::Future {
            let error = ErrorObject::owned(self.0, "bad params", Some(json!({"detail": "x"})));
            std::future::ready(MethodResponse::error(request.id.into_owned(), error))
        }
    }

    #[tokio::test]
    async fn only_the_invalid_params_code_is_replaced() {
        let call = |code| {
            let request =
                jsonrpsee::types::Request::new("getinfo".into(), None, Id::Str("curl".into()));
            ZcashdErrorCodes::new(Fails(code)).call(request)
        };
        let replaced = call(ErrorCode::InvalidParams.code()).await;
        let json: serde_json::Value =
            serde_json::from_str(replaced.as_result()).expect("valid json");
        assert_eq!(json["id"], json!("curl"));
        assert_eq!(
            json["error"],
            json!({"code": MISC_ERROR_CODE, "message": "bad params", "data": {"detail": "x"}})
        );

        let kept = call(ErrorCode::MethodNotFound.code()).await;
        assert_eq!(kept.as_error_code(), Some(ErrorCode::MethodNotFound.code()));
    }
}
