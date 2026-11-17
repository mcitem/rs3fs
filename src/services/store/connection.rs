use super::{Store, TransferSlots};
use anyhow::{Context, Result, anyhow, ensure};
use aws_sdk_s3::{
    Client,
    config::{Credentials, Region, RequestChecksumCalculation, ResponseChecksumValidation},
};
use tokio::io::AsyncReadExt;

#[derive(serde::Deserialize)]
#[serde(rename_all = "PascalCase")]
struct OscaQuotaResponse {
    code: u16,
    data: Option<ServerCapacity>,
}

#[derive(Clone, Copy, serde::Deserialize)]
#[serde(rename_all = "PascalCase")]
pub(crate) struct ServerCapacity {
    pub(crate) total: i64,
    pub(crate) used: i64,
}
#[derive(Clone)]
pub(crate) struct Server {
    client: Client,
    transfer_slots: TransferSlots,
    custom_endpoint: bool,
    endpoint: Option<String>,
    osca_quota_url: Option<String>,
}

impl Server {
    pub(crate) fn region(&self) -> Option<&str> {
        self.client.config().region().map(Region::as_ref)
    }

    pub(crate) async fn connect(
        region: String,
        endpoint: String,
        profile: String,
        credentials: Option<Credentials>,
    ) -> Result<Self> {
        let custom_endpoint = !endpoint.is_empty();
        let endpoint = if custom_endpoint {
            let address = if endpoint.contains("://") {
                endpoint
            } else {
                format!("https://{endpoint}")
            };
            let url = url::Url::parse(&address).context("自定义端点 URL 无效")?;
            ensure!(
                matches!(url.scheme(), "http" | "https")
                    && url.host().is_some()
                    && url.username().is_empty()
                    && url.password().is_none()
                    && url.query().is_none()
                    && url.fragment().is_none(),
                "请输入 S3 API 的 HTTP 或 HTTPS 地址。"
            );
            Some(url.to_string())
        } else {
            None
        };
        let osca_quota_url = endpoint
            .as_deref()
            .and_then(|endpoint| url::Url::parse(endpoint).ok())
            .filter(|url| {
                url.scheme() == "https"
                    && url.host_str() == Some("fgws3-ocloud.ihep.ac.cn")
                    && url.port().is_none()
                    && url.path() == "/"
            })
            .map(|mut url| {
                url.set_query(Some("fgwapi=bucket-quota"));
                url.to_string()
            });
        let mut loader = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .timeout_config(
                aws_config::timeout::TimeoutConfig::builder()
                    .connect_timeout(std::time::Duration::from_secs(10))
                    .operation_attempt_timeout(std::time::Duration::from_secs(180))
                    .build(),
            );
        if !region.is_empty() {
            loader = loader.region(Region::new(region));
        }
        if !profile.is_empty() {
            loader = loader.profile_name(profile);
        }
        if let Some(credentials) = credentials {
            loader = loader.credentials_provider(credentials);
        }
        let config = loader.load().await;
        let effective_endpoint = endpoint
            .clone()
            .or_else(|| config.endpoint_url().map(str::to_owned));
        let mut builder = aws_sdk_s3::config::Builder::from(&config)
            .request_checksum_calculation(RequestChecksumCalculation::WhenRequired)
            .response_checksum_validation(ResponseChecksumValidation::WhenRequired);
        if let Some(endpoint) = endpoint {
            builder = builder.endpoint_url(endpoint).force_path_style(true);
        }
        if config.region().is_none() {
            builder = builder.region(Region::new("us-east-1"));
        }
        Ok(Self {
            client: Client::from_conf(builder.build()),
            transfer_slots: TransferSlots::new(4),
            custom_endpoint,
            endpoint: effective_endpoint,
            osca_quota_url,
        })
    }

    pub(crate) fn with_transfer_slots(mut self, slots: TransferSlots) -> Self {
        self.transfer_slots = slots;
        self
    }

    pub(crate) async fn buckets(&self) -> Result<Vec<Bucket>> {
        let mut request = self.client.list_buckets();
        if !self.custom_endpoint {
            request = request.max_buckets(1000);
        }
        let mut pages = request.into_paginator().send();
        let mut buckets = Vec::new();
        while let Some(page) = pages.next().await {
            let page = page.map_err(|error| connection_error("无法列出存储桶", error))?;
            buckets.extend(page.buckets().iter().filter_map(|bucket| {
                Some(Bucket {
                    name: bucket.name()?.to_owned(),
                    region: bucket.bucket_region().map(str::to_owned),
                })
            }));
        }
        buckets.sort_by(|a, b| a.name.cmp(&b.name));
        buckets.dedup_by(|a, b| a.name == b.name);
        Ok(buckets)
    }

    pub(crate) fn open(&self, name: String, region: Option<String>) -> Store {
        let client = match region.filter(|_| !self.custom_endpoint) {
            Some(region) => Client::from_conf(
                self.client
                    .config()
                    .to_builder()
                    .region(Region::new(region))
                    .build(),
            ),
            None => self.client.clone(),
        };
        Store {
            client,
            bucket: name,
            transfer_slots: self.transfer_slots.clone(),
            endpoint: self.endpoint.clone(),
            osca_quota_url: self.osca_quota_url.clone(),
        }
    }
}

#[derive(Clone)]
pub(crate) struct Bucket {
    name: String,
    region: Option<String>,
}

impl Bucket {
    pub(crate) fn named(name: &str) -> Self {
        Self {
            name: name.to_owned(),
            region: None,
        }
    }

    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    pub(crate) fn region(&self) -> Option<&str> {
        self.region.as_deref()
    }

    pub(crate) fn open(&self, server: &Server) -> Store {
        server.open(self.name.clone(), self.region.clone())
    }
}

pub(super) fn connection_error<E: std::fmt::Debug>(
    action: &str,
    error: aws_sdk_s3::error::SdkError<E>,
) -> anyhow::Error {
    if let Some(response) = error.raw_response() {
        let status = response.status().as_u16();
        let hint = if status == 403 {
            "。请检查密钥和存储桶权限"
        } else {
            ""
        };
        return anyhow!("{action}（HTTP {status}）{hint}");
    }
    let detail = format!("{error:?}");
    let lower = detail.to_ascii_lowercase();
    let hint = if lower.contains("tls handshake eof") {
        "。对端在 TLS 握手期间关闭连接。请检查是否使用 S3 API 的 HTTPS 端口，以及反向代理是否配置了 TLS"
    } else if lower.contains("unknownissuer") || lower.contains("unknown issuer") {
        "。证书颁发机构不受信任。可将服务的根 CA 导入 Windows 受信任的根证书存储后重启应用"
    } else if lower.contains("notvalidforname") {
        "。请使用证书覆盖的主机名连接"
    } else if lower.contains("causedasendentity") {
        "。服务端使用了 CA 证书作为站点证书。请为 S3 API 配置由该 CA 签发的站点证书"
    } else if lower.contains("accessdenied") || lower.contains("access denied") {
        "。请检查密钥和存储桶权限"
    } else {
        ""
    };
    anyhow!("{action}{hint}")
}

impl Store {
    pub(crate) fn endpoint(&self) -> Option<&str> {
        self.endpoint.as_deref()
    }

    #[cfg(windows)]
    pub(crate) fn preview_origin(&self) -> (Option<&str>, Option<&str>, &str) {
        (
            self.endpoint(),
            self.client.config().region().map(|region| region.as_ref()),
            &self.bucket,
        )
    }

    pub(crate) fn bucket(&self) -> &str {
        &self.bucket
    }

    pub(crate) async fn share_url(&self, key: String, minutes: u64) -> Result<String> {
        let config = aws_sdk_s3::presigning::PresigningConfig::expires_in(
            std::time::Duration::from_secs(minutes * 60),
        )?;
        let request = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .presigned(config)
            .await
            .context("无法生成临时下载链接")?;
        Ok(request.uri().to_string())
    }

    pub(crate) fn supports_server_capacity(&self) -> bool {
        self.osca_quota_url.is_some()
    }

    pub(crate) async fn server_capacity(&self) -> Result<Option<ServerCapacity>> {
        let Some(uri) = self.osca_quota_url.clone() else {
            return Ok(None);
        };
        let bucket = self.bucket.clone();
        let output = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key("quota")
            .customize()
            .map_request(move |mut request| {
                request
                    .set_uri(uri.clone())
                    .map_err(std::io::Error::other)?;
                request.headers_mut().insert("bucket", bucket.clone());
                Ok::<_, std::io::Error>(request)
            })
            .send()
            .await
            .context("OSCA 配额请求失败")?;
        let mut bytes = Vec::new();
        output
            .body
            .into_async_read()
            .take(16 * 1024 + 1)
            .read_to_end(&mut bytes)
            .await?;
        ensure!(bytes.len() <= 16 * 1024, "OSCA 配额响应过大");
        let response: OscaQuotaResponse =
            serde_json::from_slice(&bytes).context("OSCA 配额响应格式无效")?;
        ensure!(
            response.code == 200,
            "OSCA 配额查询失败（{}）",
            response.code
        );
        let data = response.data.context("OSCA 未返回容量数据")?;
        ensure!(data.used >= 0 && data.total >= 0, "OSCA 返回了无效的容量");
        Ok(Some(data))
    }
}
