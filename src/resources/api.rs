//! Shared transport and query lifecycle; each registered protocol owns its options and parsing.
use super::*;
use crate::registry::RegistryError;
use async_trait::async_trait;
use rust_decimal::Decimal;
use serde_json::Value;
use std::sync::Arc;

pub(super) type QueryResult<T> = Result<T, QueryFailure>;
pub(super) struct QueryFailure(pub ResourceState, pub &'static str);
pub(super) fn invalid() -> QueryFailure {
    QueryFailure(ResourceState::Failure, "资源接口返回的数据格式无效")
}

pub(super) fn validate_base_url(value: &str) -> Result<reqwest::Url, RegistryError> {
    let mut url = reqwest::Url::parse(value)
        .map_err(|_| RegistryError::Factory("资源 base_url 必须为绝对 HTTP(S) URL".into()))?;
    let loopback = url.host_str().is_some_and(|host| {
        host == "localhost"
            || host
                .trim_matches(['[', ']'])
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
    });
    if (url.scheme() != "https" && !(url.scheme() == "http" && loopback))
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(RegistryError::Factory(
            "资源 base_url 需使用 HTTPS（本机回环可用 HTTP），且不能包含凭据、查询参数或片段"
                .into(),
        ));
    }
    let path = format!("{}/", url.path().trim_end_matches('/'));
    url.set_path(&path);
    Ok(url)
}

pub(super) fn options<T: serde::de::DeserializeOwned>(value: &Value) -> Result<T, RegistryError> {
    serde_json::from_value(if value.is_null() {
        serde_json::json!({})
    } else {
        value.clone()
    })
    .map_err(|_| RegistryError::Factory("资源 options 缺少必需字段、包含未知字段或类型无效".into()))
}

#[async_trait]
pub(super) trait ApiProtocol: Send + Sync {
    const SERVICE: &'static str;
    const AUTHENTICATION_HINT: &'static str = "资源查询认证失败或权限不足，请检查对应类型的凭据";
    fn configure(value: &Value) -> Result<Self, RegistryError>
    where
        Self: Sized;
    fn scope(&self) -> ResourceScope;
    async fn query(&self, client: &ApiClient, token: &str) -> QueryResult<Vec<ResourceMetric>>;
}

pub(super) struct ApiClient {
    service: &'static str,
    binding: ResourceBinding,
    pub(super) connection: ApiConnection,
    base: reqwest::Url,
    authentication_hint: &'static str,
}
struct ApiResource<P> {
    client: ApiClient,
    protocol: P,
}

pub(super) fn register<P: ApiProtocol + 'static>(registry: &mut ResourceRegistry) {
    registry
        .register(P::SERVICE, |context: ResourceContext| {
            let ResourceConnection::Api(connection) = context.connection else {
                return Err(RegistryError::Factory(
                    "API 资源需要独立的 API 连接配置".into(),
                ));
            };
            let base = validate_base_url(&connection.base_url)?;
            let protocol = P::configure(&connection.options)?;
            Ok(Arc::new(ApiResource {
                client: ApiClient {
                    service: P::SERVICE,
                    binding: context.binding,
                    connection,
                    base,
                    authentication_hint: P::AUTHENTICATION_HINT,
                },
                protocol,
            }) as Arc<dyn ResourceReader>)
        })
        .expect("内置 API 服务 ID 唯一");
}

impl ApiClient {
    pub(super) async fn credential(&self) -> QueryResult<String> {
        let resolver = self.connection.credentials.clone();
        let reference = self.connection.credential_ref.clone();
        let token = tokio::task::spawn_blocking(move || resolver(&reference))
            .await
            .map_err(|_| QueryFailure(ResourceState::TransientFailure, "资源凭据读取任务中断"))?
            .map_err(|_| {
                QueryFailure(
                    ResourceState::NeedsConfiguration,
                    "未找到资源查询凭据，请检查配置的凭据引用",
                )
            })?;
        let token = token.trim();
        if token.is_empty() || token.bytes().any(|c| !c.is_ascii_graphic()) {
            return Err(QueryFailure(
                ResourceState::NeedsConfiguration,
                "资源凭据必须为非空且不含空格或控制字符的 token",
            ));
        }
        Ok(token.to_owned())
    }
    pub(super) async fn get(
        &self,
        url: reqwest::Url,
        token: &str,
        extra: &[(&str, String)],
    ) -> QueryResult<Value> {
        let mut headers = vec![
            ("Authorization", format!("Bearer {token}")),
            ("Accept", "application/json".into()),
        ];
        headers.extend_from_slice(extra);
        let response = self
            .connection
            .http
            .request("GET", url.as_str(), &headers, None)
            .await
            .map_err(|_| QueryFailure(ResourceState::TransientFailure, "资源查询网络连接失败"))?;
        tracing::debug!(event = "resource_http_result", service = self.service,
            target = %self.binding.cache_key(self.service), http_status = response.status);
        match response.status {
            200..=299 => {}
            300..=399 => {
                return Err(QueryFailure(
                    ResourceState::NeedsConfiguration,
                    "资源接口跳转异常，请核对 base_url；不会跨来源转发查询凭据",
                ));
            }
            401 | 403 => {
                return Err(QueryFailure(
                    ResourceState::AuthenticationFailed,
                    self.authentication_hint,
                ));
            }
            404 => {
                return Err(QueryFailure(
                    ResourceState::NeedsConfiguration,
                    "此端点不支持该资源接口，请检查服务类型和 base_url",
                ));
            }
            status if crate::http::is_transient_status(status) => {
                return Err(QueryFailure(
                    ResourceState::TransientFailure,
                    "资源服务暂不可用或请求受限",
                ));
            }
            _ => return Err(QueryFailure(ResourceState::Failure, "资源服务拒绝查询")),
        }
        if response.body.len() > 2 * 1024 * 1024 {
            return Err(invalid());
        }
        serde_json::from_slice(&response.body).map_err(|_| invalid())
    }
    pub(super) fn endpoint(&self, path: &str) -> reqwest::Url {
        self.base.join(path).expect("static relative endpoint")
    }
}
pub(super) fn number(value: Option<&Value>) -> QueryResult<Decimal> {
    let text = match value {
        Some(Value::Number(value)) => value.to_string(),
        Some(Value::String(value)) => value.clone(),
        _ => return Err(invalid()),
    };
    Decimal::from_str_exact(&text)
        .or_else(|_| Decimal::from_scientific(&text))
        .map_err(|_| invalid())
}

#[async_trait]
impl<P: ApiProtocol + 'static> ResourceReader for ApiResource<P> {
    fn service(&self) -> &str {
        P::SERVICE
    }
    fn binding(&self) -> &ResourceBinding {
        &self.client.binding
    }
    fn scope(&self) -> ResourceScope {
        self.protocol.scope()
    }
    async fn read(&self) -> ResourceSnapshot {
        let mut operation = crate::logging::Operation::new("resource.query");
        operation.begin("query");
        let result = crate::logging::instrument(operation.span(), async {
            let token = self.client.credential().await?;
            self.protocol.query(&self.client, &token).await
        })
        .await;
        let (state, metrics, message) = match result {
            Ok(metrics) => (ResourceState::Ready, metrics, None),
            Err(QueryFailure(state, message)) => (state, Vec::new(), Some(message.into())),
        };
        operation.span().in_scope(|| {
            if state == ResourceState::Ready {
                tracing::debug!(event = "resource_query_result", service = P::SERVICE,
                    target = %self.binding().cache_key(P::SERVICE), connection_id = %self.binding().account, successful = true, state = ?state);
            } else {
                tracing::warn!(event = "resource_query_result", service = P::SERVICE,
                    target = %self.binding().cache_key(P::SERVICE), connection_id = %self.binding().account, successful = false, state = ?state, reason = message.as_deref().unwrap_or(""));
            }
        });
        operation.finish(state == ResourceState::Ready);
        ResourceSnapshot {
            binding: self.binding().clone(),
            service: P::SERVICE.into(),
            scope: self.scope(),
            state,
            metrics,
            message,
            queried_at: Some((self.client.connection.now)()),
        }
    }
}
