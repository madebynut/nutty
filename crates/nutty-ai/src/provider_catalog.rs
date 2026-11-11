//! Bounded provider model discovery without sending prompts or terminal content.

use std::{collections::HashSet, time::Duration};

use anyhow::{Context as _, Result, bail, ensure};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::{
    network::Proxy,
    provider::{Kind, Model, Reasoning},
};

/// Read the authenticated model catalog; connection tests use the same read-only request.
pub async fn fetch(
    kind: Kind,
    endpoint: &str,
    key: &str,
    proxy: Option<&Proxy>,
    cancel: CancellationToken,
) -> Result<Vec<Model>> {
    let mut url = url::Url::parse(endpoint).context("API 地址无效")?;
    ensure!(
        matches!(url.scheme(), "http" | "https")
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none(),
        "API 地址无效，请将密钥保存在独立字段"
    );
    ensure!(
        kind == Kind::Ollama || !key.trim().is_empty(),
        "请先填写 API 密钥"
    );
    let path = url.path().trim_end_matches('/');
    let suffix = match kind {
        Kind::Ollama => {
            if path.ends_with("/api") {
                "tags"
            } else {
                "api/tags"
            }
        }
        Kind::Anthropic => {
            if path.ends_with("/v1") {
                "models"
            } else {
                "v1/models"
            }
        }
        Kind::Gemini => {
            if path.ends_with("/v1beta") || path.ends_with("/v1") {
                "models"
            } else {
                "v1beta/models"
            }
        }
        Kind::OpenAi | Kind::DeepSeek => "models",
    };
    url.set_path(&format!("{path}/{suffix}"));
    let client = if let Some(proxy) = proxy {
        proxy.http_client().await?
    } else {
        reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(10))
            .build()?
    };
    let calling = async {
        let mut models = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..16 {
            let mut request_url = url.clone();
            if kind == Kind::Anthropic {
                request_url.query_pairs_mut().append_pair("limit", "100");
            }
            if kind == Kind::Gemini {
                request_url.query_pairs_mut().append_pair("pageSize", "100");
            }
            if let Some(cursor) = &cursor {
                request_url.query_pairs_mut().append_pair(
                    if kind == Kind::Gemini {
                        "pageToken"
                    } else {
                        "after_id"
                    },
                    cursor,
                );
            }
            let mut request = client.get(request_url).timeout(Duration::from_secs(15));
            request = match kind {
                Kind::Anthropic => request
                    .header("x-api-key", key)
                    .header("anthropic-version", "2023-06-01"),
                Kind::Gemini => request.header("x-goog-api-key", key),
                _ if !key.is_empty() => request.bearer_auth(key),
                _ => request,
            };
            // Never include response bodies or URLs in errors: gateways may echo credentials.
            let mut response = request
                .send()
                .await
                .map_err(|_| anyhow::anyhow!("连接失败，请检查 API 地址、代理和网络"))?;
            match response.status().as_u16() {
                200..=299 => {}
                401 | 403 => bail!("认证失败，请检查 API 密钥和访问权限"),
                404 | 405 => bail!("此地址不支持模型列表，请检查 API 地址或手动添加模型"),
                429 => bail!("请求过于频繁，请稍后重试"),
                code => bail!("模型接口返回 HTTP {code}"),
            }
            let mut bytes = Vec::new();
            while let Some(chunk) = response
                .chunk()
                .await
                .map_err(|_| anyhow::anyhow!("读取模型列表失败"))?
            {
                ensure!(bytes.len() + chunk.len() <= 2 * 1024 * 1024, "模型列表过大");
                bytes.extend_from_slice(&chunk);
            }
            let value: Value = serde_json::from_slice(&bytes)
                .map_err(|_| anyhow::anyhow!("模型接口未返回有效 JSON"))?;
            models.extend(parse(kind, &value)?);
            ensure!(models.len() <= 512, "模型超过 512 个，请手动选择添加");
            let next = match kind {
                Kind::Gemini => value["nextPageToken"].as_str(),
                Kind::Anthropic if value["has_more"].as_bool() == Some(true) => {
                    value["last_id"].as_str()
                }
                _ => None,
            }
            .filter(|token| !token.is_empty())
            .map(str::to_owned);
            if next.is_none() {
                let mut seen = HashSet::new();
                models.retain(|model| seen.insert(model.id.clone()));
                ensure!(!models.is_empty(), "没有可用于对话的模型，请手动添加模型");
                models.sort_by(|a, b| a.id.cmp(&b.id));
                return Ok(models);
            }
            ensure!(next != cursor, "模型列表分页无效");
            cursor = next;
        }
        bail!("模型列表分页过多，请手动添加模型")
    };
    tokio::select! {
        biased;
        () = cancel.cancelled() => bail!("请求已取消"),
        result = tokio::time::timeout(Duration::from_secs(30), calling) => result.context("模型列表请求超时")?,
    }
}

fn parse(kind: Kind, value: &Value) -> Result<Vec<Model>> {
    let array = value[if matches!(kind, Kind::Gemini | Kind::Ollama) {
        "models"
    } else {
        "data"
    }]
    .as_array()
    .context("模型列表格式无效")?;
    ensure!(array.len() <= 512, "模型超过 512 个，请手动选择添加");
    let mut models = Vec::new();
    for item in array {
        if kind == Kind::Gemini
            && item["supportedGenerationMethods"]
                .as_array()
                .is_some_and(|methods| !methods.iter().any(|method| method == "generateContent"))
        {
            continue;
        }
        let id = match kind {
            Kind::Gemini => item["name"]
                .as_str()
                .map(|id| id.strip_prefix("models/").unwrap_or(id)),
            Kind::Ollama => item["name"].as_str().or_else(|| item["model"].as_str()),
            _ => item["id"].as_str(),
        }
        .context("模型名称缺失")?;
        ensure!(
            !id.trim().is_empty() && id.len() <= 256 && !id.contains(['\r', '\n', '\0']),
            "模型名称无效"
        );
        let metadata = item
            .pointer("/effort/supported_levels")
            .or_else(|| item.pointer("/reasoning/supported_levels"));
        let reasoning = if let Some(levels) = metadata.and_then(Value::as_array) {
            let mut reasoning = vec![Reasoning::Auto];
            if kind == Kind::DeepSeek {
                reasoning.push(Reasoning::Off);
            }
            for level in levels.iter().filter_map(Value::as_str) {
                if let Some(level) = Reasoning::ALL
                    .into_iter()
                    .find(|candidate| candidate.label().eq_ignore_ascii_case(level))
                    && !reasoning.contains(&level)
                {
                    reasoning.push(level);
                }
            }
            reasoning
        } else {
            suggested_reasoning(kind, id)
        };
        models.push(Model {
            id: id.into(),
            reasoning,
        });
    }
    Ok(models)
}

pub fn suggested_reasoning(kind: Kind, id: &str) -> Vec<Reasoning> {
    match kind {
        Kind::DeepSeek if id.contains("v4") || id == "deepseek-flash" => {
            kind.reasoning_choices().to_vec()
        }
        Kind::DeepSeek if id == "deepseek-reasoner" => vec![Reasoning::Auto],
        _ => vec![Reasoning::Auto],
    }
}

/// Refreshing a catalog must preserve manual entries and user-selected effort levels.
pub fn merge(existing: &mut Vec<Model>, incoming: Vec<Model>) -> Result<()> {
    let additional = incoming
        .iter()
        .filter(|model| !existing.iter().any(|old| old.id == model.id))
        .count();
    ensure!(
        existing.len() + additional <= 512,
        "模型超过 512 个，请先移除不需要的模型"
    );
    for model in incoming {
        if !existing.iter().any(|old| old.id == model.id) {
            existing.push(model);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::{
        io::{AsyncReadExt as _, AsyncWriteExt as _},
        net::TcpListener,
    };

    #[test]
    fn catalog_metadata_and_refresh_preserve_manual_model_choices() -> Result<()> {
        let models = parse(
            Kind::DeepSeek,
            &serde_json::json!({"data":[
                {"id":"deepseek-flash","effort":{"supported_levels":["low","high","max","future"]}},
                {"id":"deepseek-reasoner"}
            ]}),
        )?;
        assert_eq!(
            models[0].reasoning,
            [
                Reasoning::Auto,
                Reasoning::Off,
                Reasoning::Low,
                Reasoning::High,
                Reasoning::Max
            ]
        );
        let mut configured = vec![
            Model {
                id: "deepseek-flash".into(),
                reasoning: vec![Reasoning::Max],
            },
            Model {
                id: "private-model".into(),
                reasoning: vec![Reasoning::Low],
            },
        ];
        merge(&mut configured, models)?;
        assert_eq!(configured.len(), 3);
        assert_eq!(
            configured[0].reasoning,
            [Reasoning::Max],
            "refresh retains user effort selection"
        );
        assert_eq!(
            configured[1].id, "private-model",
            "manual gateway model survives refresh"
        );
        assert!(
            parse(
                Kind::OpenAi,
                &serde_json::json!({"data":[{"id":"bad\nmodel"}]})
            )
            .is_err()
        );
        assert!(parse(Kind::OpenAi, &serde_json::json!({"error":"not a list"})).is_err());
        Ok(())
    }

    async fn serve(listener: &TcpListener, status: &str, body: Value) -> Result<String> {
        let (mut socket, _) = listener.accept().await?;
        let mut request = Vec::new();
        while !request.windows(4).any(|part| part == b"\r\n\r\n") {
            let mut buffer = [0; 4096];
            let count = socket.read(&mut buffer).await?;
            ensure!(
                count > 0 && request.len() < 32 * 1024,
                "fixture request invalid"
            );
            request.extend_from_slice(&buffer[..count]);
        }
        let body = body.to_string();
        socket.write_all(format!("HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await?;
        Ok(String::from_utf8(request)?.to_lowercase())
    }

    #[tokio::test]
    async fn native_catalogs_use_authenticated_get_and_follow_provider_pagination() -> Result<()> {
        tokio::time::timeout(Duration::from_secs(5),async {
            for kind in Kind::ALL {
                let listener = TcpListener::bind("127.0.0.1:0").await?;
                let endpoint = format!("http://{}/gateway",listener.local_addr()?);
                let server = tokio::spawn(async move {
                    let first = match kind {
                        Kind::Ollama => serde_json::json!({"models":[{"name":"qwen3"}]}),
                        Kind::Gemini => serde_json::json!({"models":[{"name":"models/chat","supportedGenerationMethods":["generateContent"]},{"name":"models/embedding","supportedGenerationMethods":["embedContent"]}],"nextPageToken":"next/token"}),
                        Kind::Anthropic => serde_json::json!({"data":[{"id":"chat"}],"has_more":true,"last_id":"chat"}),
                        _ => serde_json::json!({"data":[{"id":"chat"}]}),
                    };
                    let mut requests = vec![serve(&listener,"200 OK",first).await?];
                    if matches!(kind,Kind::Anthropic|Kind::Gemini) {
                        let second = if kind == Kind::Gemini {
                            serde_json::json!({"models":[{"name":"models/chat-2"}]})
                        } else { serde_json::json!({"data":[{"id":"chat-2"}],"has_more":false}) };
                        requests.push(serve(&listener,"200 OK",second).await?);
                    }
                    Ok::<_,anyhow::Error>(requests)
                });
                let models = fetch(kind,&endpoint,"fixture-key",None,CancellationToken::new()).await?;
                let requests = server.await??;
                assert!(requests.iter().all(|request| request.starts_with("get /gateway/")),"{kind:?} uses GET under configured path");
                assert!(!requests[0].contains("fixture-key?"));
                match kind {
                    Kind::Ollama => { assert!(requests[0].starts_with("get /gateway/api/tags ")); assert_eq!(models[0].id,"qwen3"); }
                    Kind::Gemini => { assert!(requests[0].contains("x-goog-api-key: fixture-key")); assert!(requests[1].contains("pagetoken=next%2ftoken")); assert_eq!(models.len(),2); }
                    Kind::Anthropic => { assert!(requests[0].contains("x-api-key: fixture-key")); assert!(requests[1].contains("after_id=chat")); assert_eq!(models.len(),2); }
                    _ => assert!(requests[0].contains("authorization: bearer fixture-key")),
                }
            }
            Ok::<_,anyhow::Error>(())
        }).await.context("catalog fixture timed out")?
    }

    #[tokio::test]
    async fn cancellation_and_auth_errors_never_disclose_credentials() -> Result<()> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let endpoint = format!("http://{}", listener.local_addr()?);
        let server = tokio::spawn(async move {
            serve(
                &listener,
                "401 Unauthorized",
                serde_json::json!({"error":"fixture-key"}),
            )
            .await
        });
        let error = fetch(
            Kind::DeepSeek,
            &endpoint,
            "fixture-key",
            None,
            CancellationToken::new(),
        )
        .await
        .expect_err("bad key rejected");
        assert!(!format!("{error:#}").contains("fixture-key"));
        server.await??;
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let endpoint = format!("http://{}", listener.local_addr()?);
        let cancel = CancellationToken::new();
        let request_cancel = cancel.clone();
        let request =
            tokio::spawn(
                async move { fetch(Kind::Ollama, &endpoint, "", None, request_cancel).await },
            );
        let (_socket, _) =
            tokio::time::timeout(Duration::from_secs(1), listener.accept()).await??;
        cancel.cancel();
        assert!(
            tokio::time::timeout(Duration::from_millis(200), request)
                .await??
                .is_err(),
            "closing or editing cancels pending network I/O"
        );
        Ok(())
    }
}
