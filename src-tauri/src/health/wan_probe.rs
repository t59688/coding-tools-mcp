use std::net::IpAddr;
use std::time::Duration;

use serde::Serialize;
use serde_json::{json, Value};

use crate::settings::AppSettings;
use crate::workspace::WorkspaceProfile;

const PROBE_TIMEOUT: Duration = Duration::from_secs(8);
const CHECK_HOST_POLL: Duration = Duration::from_millis(800);
const CHECK_HOST_ATTEMPTS: u32 = 12;
const ACCEPTABLE_MCP_STATUSES: [u16; 4] = [200, 401, 403, 405];

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WanProbeStep {
    pub id: String,
    pub label: String,
    pub ok: bool,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WanProbeResult {
    pub public_url: String,
    pub endpoint: String,
    pub verdict: String,
    pub reachable: bool,
    pub summary: String,
    pub hint: String,
    pub steps: Vec<WanProbeStep>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExternalOutcome {
    McpOk,
    HostOnly,
    Unreachable,
    ProbeUnavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HandshakeOutcome {
    McpOk,
    AuthRequired,
    Failed,
}

pub async fn probe_mcp_wan_access(profile: &WorkspaceProfile) -> WanProbeResult {
    probe_mcp_endpoint(
        &profile.effective_public_url(),
        &profile.public_endpoint(),
        &AppSettings::load_or_default(),
    )
    .await
}

async fn probe_mcp_endpoint(
    public_url: &str,
    endpoint: &str,
    settings: &AppSettings,
) -> WanProbeResult {
    let mut steps = Vec::new();
    if public_url.trim().is_empty() || endpoint.trim().is_empty() {
        steps.push(step(
            "configured",
            "公网地址",
            false,
            "未配置公网 URL。请先启用 FRP / Cloudflare / Global Gateway。",
        ));
        return finish(public_url, endpoint, steps, "not_configured", false);
    }
    steps.push(step(
        "configured",
        "公网地址",
        true,
        format!("endpoint={endpoint}"),
    ));

    let parsed = match reqwest::Url::parse(endpoint) {
        Ok(url) => url,
        Err(err) => {
            steps.push(step("url", "URL 解析", false, err.to_string()));
            return finish(public_url, endpoint, steps, "unreachable", false);
        }
    };
    let Some(host) = parsed.host_str().map(str::to_string) else {
        steps.push(step("url", "URL 解析", false, "公网 URL 没有主机名。"));
        return finish(public_url, endpoint, steps, "unreachable", false);
    };
    let scheme = parsed.scheme().to_string();
    let port = parsed.port_or_known_default().unwrap_or(80);
    steps.push(step(
        "scheme",
        "传输协议",
        true,
        if scheme == "https" {
            "HTTPS".into()
        } else {
            format!("{scheme}（部分云端 MCP 客户端要求 HTTPS）")
        },
    ));

    let client = match http_client(settings) {
        Ok(client) => client,
        Err(err) => {
            steps.push(step("client", "探测客户端", false, err));
            return finish(public_url, endpoint, steps, "inconclusive", false);
        }
    };

    let ips = resolve_host(&client, &host, port).await;
    let public_ips: Vec<IpAddr> = ips.iter().copied().filter(|ip| is_public_ip(*ip)).collect();
    let private_ips: Vec<IpAddr> = ips.iter().copied().filter(|ip| !is_public_ip(*ip)).collect();
    if ips.is_empty() {
        steps.push(step(
            "dns",
            "DNS 解析",
            false,
            format!("{host} 无法解析。请确认公网域名填写正确。"),
        ));
        return finish(public_url, endpoint, steps, "unreachable", false);
    }
    steps.push(step(
        "dns",
        "DNS 解析",
        !public_ips.is_empty(),
        format!(
            "{} → {}",
            host,
            ips.iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        ),
    ));
    let only_private = public_ips.is_empty();
    steps.push(step(
        "address_scope",
        "地址范围",
        !only_private,
        if only_private {
            format!(
                "解析结果均为私网/回环地址（{}），外网客户端无法访问。",
                join_ips(&private_ips)
            )
        } else if private_ips.is_empty() {
            format!("公网地址 {}", join_ips(&public_ips))
        } else {
            format!(
                "公网 {}；同时还有私网 {}（可能是本地 Hosts / 分流）",
                join_ips(&public_ips),
                join_ips(&private_ips)
            )
        },
    ));
    if only_private {
        return finish(public_url, endpoint, steps, "unreachable", false);
    }

    let (external, external_detail) = probe_from_external_nodes(&client, endpoint).await;
    steps.push(step(
        "external_http",
        "外网节点探测",
        matches!(external, ExternalOutcome::McpOk | ExternalOutcome::HostOnly),
        external_detail,
    ));

    let (handshake, handshake_detail) = probe_mcp_handshake(&client, endpoint).await;
    steps.push(step(
        "mcp_handshake",
        "MCP 握手（经公网 URL）",
        matches!(
            handshake,
            HandshakeOutcome::McpOk | HandshakeOutcome::AuthRequired
        ),
        handshake_detail,
    ));

    let (verdict, reachable, summary, hint) =
        decide_verdict(!only_private, external, handshake);
    WanProbeResult {
        public_url: public_url.to_string(),
        endpoint: endpoint.to_string(),
        verdict: verdict.to_string(),
        reachable,
        summary: summary.to_string(),
        hint: hint.to_string(),
        steps,
    }
}

fn finish(
    public_url: &str,
    endpoint: &str,
    steps: Vec<WanProbeStep>,
    verdict: &str,
    reachable: bool,
) -> WanProbeResult {
    let (summary, hint) = match verdict {
        "not_configured" => (
            "尚未配置公网 MCP 地址。",
            "先启动隧道或 Global Gateway，再测试外网可达性。",
        ),
        "unreachable" => (
            "当前 MCP 不能被外网访问。",
            "检查隧道是否运行、公网域名/证书，以及云厂商或本机防火墙是否放行入站。",
        ),
        "inconclusive" => (
            "无法从外网探测点确认可达性。",
            "本机可能无法访问探测服务。可稍后重试，或用手机流量访问公网 URL 对照。",
        ),
        _ => (
            "探测未完成。",
            "请重新运行外网探测。",
        ),
    };
    WanProbeResult {
        public_url: public_url.to_string(),
        endpoint: endpoint.to_string(),
        verdict: verdict.to_string(),
        reachable,
        summary: summary.to_string(),
        hint: hint.to_string(),
        steps,
    }
}

pub(crate) fn decide_verdict(
    has_public_ip: bool,
    external: ExternalOutcome,
    handshake: HandshakeOutcome,
) -> (&'static str, bool, &'static str, &'static str) {
    if !has_public_ip {
        return (
            "unreachable",
            false,
            "公网 URL 解析不到公网 IP，外网无法访问。",
            "不要用 127.0.0.1 或局域网地址作为公网入口；应使用 FRP / Cloudflare 给出的域名。",
        );
    }
    match external {
        ExternalOutcome::McpOk => (
            "reachable",
            true,
            "外网探测点可以访问该 MCP 入口。",
            if matches!(handshake, HandshakeOutcome::AuthRequired) {
                "外网可达。返回 401/403 表示认证生效，云端客户端需要带上 Bearer 或完成 OAuth。"
            } else {
                "云端 MCP 客户端应能连上该地址。若仍失败，再核对 TLS 证书与 OAuth 元数据。"
            },
        ),
        ExternalOutcome::HostOnly => (
            "host_only",
            false,
            "外网能打到这台主机，但 MCP 路径可能未正确挂载。",
            "隧道或反代已通，检查子域名、Global Gateway 的 /w/<id>/mcp 路径，以及 FRP 是否返回了 404 页。",
        ),
        ExternalOutcome::Unreachable => (
            "unreachable",
            false,
            "多个外网节点都无法访问该 MCP 入口。",
            "本机健康检查通过但外网失败，通常是只做了 NAT 回环。请确认隧道进程、入站端口和安全组。",
        ),
        ExternalOutcome::ProbeUnavailable => match handshake {
            HandshakeOutcome::McpOk | HandshakeOutcome::AuthRequired => (
                "inconclusive",
                false,
                "外网探测服务不可用；本机经公网 URL 可以握手，不能单独证明外网可达。",
                "本机访问公网 URL 可能走 NAT 回环。请用手机流量打开该 URL，或稍后重试外网探测。",
            ),
            _ => (
                "inconclusive",
                false,
                "外网探测服务不可用，无法确认可达性。",
                "请检查本机出网，或改用手机流量访问公网 MCP 地址做对照。",
            ),
        },
    }
}

pub(crate) fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let oct = v4.octets();
            let cgnat = oct[0] == 100 && (oct[1] & 0xc0) == 0x40;
            !(v4.is_unspecified()
                || v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_documentation()
                || v4.is_multicast()
                || cgnat)
        }
        IpAddr::V6(v6) => {
            !(v6.is_unspecified()
                || v6.is_loopback()
                || v6.is_multicast()
                || v6.is_unique_local()
                || v6.is_unicast_link_local())
        }
    }
}

pub(crate) fn mcp_status_ok(status: u16) -> bool {
    ACCEPTABLE_MCP_STATUSES.contains(&status)
}

pub(crate) fn parse_check_host_node(value: &Value) -> Option<(bool, u16, String)> {
    let first = value.as_array()?.first()?.as_array()?;
    let success = first.first()?.as_i64()? == 1;
    let status = first
        .get(3)
        .and_then(|item| match item {
            Value::String(text) => text.parse().ok(),
            Value::Number(number) => number.as_u64().map(|n| n as u16),
            _ => None,
        })
        .unwrap_or(0);
    let message = first
        .get(2)
        .and_then(Value::as_str)
        .unwrap_or(if success { "ok" } else { "failed" })
        .to_string();
    Some((success, status, message))
}

pub(crate) fn summarize_external_nodes(results: &[(String, bool, u16, String)]) -> ExternalOutcome {
    if results.is_empty() {
        return ExternalOutcome::ProbeUnavailable;
    }
    if results
        .iter()
        .any(|(_, success, status, _)| *success && mcp_status_ok(*status))
    {
        return ExternalOutcome::McpOk;
    }
    if results.iter().any(|(_, success, status, _)| *success || *status > 0) {
        return ExternalOutcome::HostOnly;
    }
    ExternalOutcome::Unreachable
}

fn step(id: &str, label: &str, ok: bool, detail: impl Into<String>) -> WanProbeStep {
    WanProbeStep {
        id: id.into(),
        label: label.into(),
        ok,
        detail: detail.into(),
    }
}

fn join_ips(ips: &[IpAddr]) -> String {
    ips.iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

fn http_client(settings: &AppSettings) -> Result<reqwest::Client, String> {
    let mut builder = reqwest::Client::builder()
        .timeout(PROBE_TIMEOUT)
        .user_agent(format!(
            "coding-tools-mcp-desktop/{} (+https://github.com/lengsukq/coding-tools-mcp)",
            env!("CARGO_PKG_VERSION")
        ));
    let mode = settings.download.proxy_mode.trim();
    match mode {
        "" | "none" => builder = builder.no_proxy(),
        "system" => {}
        "manual" => {
            let proxy_url = settings.download.proxy_url.trim();
            if proxy_url.is_empty() {
                return Err("下载代理模式为手动，但未填写代理地址。".into());
            }
            let proxy = reqwest::Proxy::all(proxy_url)
                .map_err(|err| format!("代理地址无效: {err}"))?;
            builder = builder.proxy(proxy);
        }
        url => {
            let proxy = reqwest::Proxy::all(url).map_err(|err| format!("代理地址无效: {err}"))?;
            builder = builder.proxy(proxy);
        }
    }
    builder.build().map_err(|err| err.to_string())
}

async fn resolve_host(client: &reqwest::Client, host: &str, port: u16) -> Vec<IpAddr> {
    if let Ok(ip) = host.parse::<IpAddr>() {
        return vec![ip];
    }
    let mut ips = doh_lookup(client, host).await;
    if ips.is_empty() {
        let lookup = if host.contains(':') {
            format!("[{host}]:{port}")
        } else {
            format!("{host}:{port}")
        };
        if let Ok(found) = tokio::net::lookup_host(lookup).await {
            ips = found.map(|addr| addr.ip()).collect();
        }
    }
    ips.sort();
    ips.dedup();
    ips
}

async fn doh_lookup(client: &reqwest::Client, host: &str) -> Vec<IpAddr> {
    let mut ips = Vec::new();
    for record_type in ["A", "AAAA"] {
        let url = format!("https://cloudflare-dns.com/dns-query?name={host}&type={record_type}");
        let Ok(response) = client
            .get(url)
            .header("accept", "application/dns-json")
            .send()
            .await
        else {
            continue;
        };
        let Ok(payload) = response.json::<Value>().await else {
            continue;
        };
        if let Some(answers) = payload.get("Answer").and_then(Value::as_array) {
            for answer in answers {
                if let Some(data) = answer.get("data").and_then(Value::as_str) {
                    if let Ok(ip) = data.parse::<IpAddr>() {
                        ips.push(ip);
                    }
                }
            }
        }
    }
    ips
}

async fn probe_from_external_nodes(
    client: &reqwest::Client,
    endpoint: &str,
) -> (ExternalOutcome, String) {
    if let Some((outcome, detail)) = check_host_probe(client, endpoint).await {
        return (outcome, detail);
    }
    if let Some((outcome, detail)) = hackertarget_probe(client, endpoint).await {
        return (outcome, format!("check-host.net 不可用，已改用 HackerTarget。{detail}"));
    }
    if let Some((outcome, detail)) = allorigins_probe(client, endpoint).await {
        return (
            outcome,
            format!("多节点探测不可用，已改用 allorigins。{detail}"),
        );
    }
    (
        ExternalOutcome::ProbeUnavailable,
        "无法使用外网探测服务（check-host.net / HackerTarget / allorigins）。".into(),
    )
}

async fn check_host_probe(
    client: &reqwest::Client,
    endpoint: &str,
) -> Option<(ExternalOutcome, String)> {
    let start_url = format!(
        "https://check-host.net/check-http?host={}&max_nodes=5",
        urlencoding_lite(endpoint)
    );
    let response = client
        .get(start_url)
        .header("accept", "application/json")
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    let payload = response.json::<Value>().await.ok()?;
    if payload.get("ok").and_then(Value::as_u64) != Some(1) {
        return None;
    }
    let request_id = payload.get("request_id")?.as_str()?.to_string();
    let node_labels = payload.get("nodes").cloned().unwrap_or(json!({}));
    let result_url = format!("https://check-host.net/check-result/{request_id}");

    for _ in 0..CHECK_HOST_ATTEMPTS {
        tokio::time::sleep(CHECK_HOST_POLL).await;
        let Ok(result_response) = client
            .get(&result_url)
            .header("accept", "application/json")
            .send()
            .await
        else {
            continue;
        };
        let Ok(result) = result_response.json::<Value>().await else {
            continue;
        };
        let Some(map) = result.as_object() else {
            continue;
        };
        if map.values().all(Value::is_null) {
            continue;
        }
        let mut parsed = Vec::new();
        for (node, value) in map {
            if value.is_null() {
                continue;
            }
            let Some((success, status, message)) = parse_check_host_node(value) else {
                continue;
            };
            let location = node_labels
                .get(node)
                .and_then(Value::as_array)
                .and_then(|items| items.get(2).and_then(Value::as_str))
                .unwrap_or(node);
            parsed.push((location.to_string(), success, status, message));
        }
        if parsed.is_empty() {
            continue;
        }
        let outcome = summarize_external_nodes(&parsed);
        let detail = parsed
            .iter()
            .map(|(location, success, status, message)| {
                if *success || *status > 0 {
                    format!("{location}: HTTP {status}")
                } else {
                    format!("{location}: {message}")
                }
            })
            .collect::<Vec<_>>()
            .join("；");
        return Some((outcome, detail));
    }
    None
}

async fn hackertarget_probe(
    client: &reqwest::Client,
    endpoint: &str,
) -> Option<(ExternalOutcome, String)> {
    let url = format!(
        "https://api.hackertarget.com/httpheaders/?q={}",
        urlencoding_lite(endpoint)
    );
    let response = client.get(url).send().await.ok()?;
    let status = response.status();
    let body = response.text().await.ok()?;
    if !status.is_success()
        || (body.to_ascii_lowercase().contains("error") && !body.contains("HTTP/"))
    {
        return None;
    }
    let http_status = parse_http_status_from_headers(&body)?;
    let outcome = if mcp_status_ok(http_status) {
        ExternalOutcome::McpOk
    } else {
        ExternalOutcome::HostOnly
    };
    Some((outcome, format!("HackerTarget HTTP {http_status}")))
}

async fn allorigins_probe(
    client: &reqwest::Client,
    endpoint: &str,
) -> Option<(ExternalOutcome, String)> {
    let url = format!(
        "https://api.allorigins.win/get?url={}",
        urlencoding_lite(endpoint)
    );
    let response = client.get(url).send().await.ok()?;
    if !response.status().is_success() {
        return None;
    }
    let payload = response.json::<Value>().await.ok()?;
    let http_status = payload
        .pointer("/status/http_code")
        .and_then(Value::as_u64)
        .map(|code| code as u16)?;
    let outcome = if mcp_status_ok(http_status) {
        ExternalOutcome::McpOk
    } else if http_status > 0 {
        ExternalOutcome::HostOnly
    } else {
        ExternalOutcome::Unreachable
    };
    Some((outcome, format!("allorigins HTTP {http_status}")))
}

async fn probe_mcp_handshake(
    client: &reqwest::Client,
    endpoint: &str,
) -> (HandshakeOutcome, String) {
    let body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": {
                "name": "coding-tools-mcp-wan-probe",
                "version": env!("CARGO_PKG_VERSION")
            }
        }
    });
    match client
        .post(endpoint)
        .header("accept", "application/json, text/event-stream")
        .json(&body)
        .send()
        .await
    {
        Ok(response) => {
            let status = response.status().as_u16();
            let text = response.text().await.unwrap_or_default();
            if status == 401 || status == 403 {
                return (
                    HandshakeOutcome::AuthRequired,
                    format!("HTTP {status}（入口可达，需要认证）"),
                );
            }
            if mcp_status_ok(status)
                && (text.contains("protocolVersion") || text.contains("jsonrpc"))
            {
                return (
                    HandshakeOutcome::McpOk,
                    format!("HTTP {status}，MCP initialize 有响应"),
                );
            }
            if looks_like_frp_placeholder(&text) {
                return (
                    HandshakeOutcome::Failed,
                    format!("HTTP {status}；FRP 未挂载代理（返回 frp 占位页）"),
                );
            }
            (
                HandshakeOutcome::Failed,
                format!("HTTP {status}；响应不像 MCP initialize"),
            )
        }
        Err(err) => (HandshakeOutcome::Failed, err.to_string()),
    }
}

fn looks_like_frp_placeholder(body: &str) -> bool {
    let lower = body.to_ascii_lowercase();
    (lower.contains("powered by") && lower.contains("frp"))
        || (lower.contains("the page you requested was not found") && lower.contains("frp"))
}

pub(crate) fn parse_http_status_from_headers(body: &str) -> Option<u16> {
    for line in body.lines() {
        let trimmed = line.trim();
        let Some(rest) = trimmed.strip_prefix("HTTP/") else {
            continue;
        };
        let status = rest.split_whitespace().nth(1)?;
        return status.parse().ok();
    }
    None
}

fn urlencoding_lite(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(byte as char);
            }
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::net::{Ipv4Addr, Ipv6Addr};

    #[test]
    fn loopback_and_lan_are_not_public() {
        assert!(!is_public_ip(IpAddr::V4(Ipv4Addr::LOCALHOST)));
        assert!(!is_public_ip(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 8))));
        assert!(!is_public_ip(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1))));
        assert!(!is_public_ip(IpAddr::V4(Ipv4Addr::new(172, 16, 0, 1))));
        assert!(!is_public_ip(IpAddr::V4(Ipv4Addr::new(100, 64, 1, 1))));
        assert!(!is_public_ip(IpAddr::V6(Ipv6Addr::LOCALHOST)));
    }

    #[test]
    fn globally_routable_ips_are_public() {
        assert!(is_public_ip(IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1))));
        assert!(is_public_ip(IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8))));
    }

    #[test]
    fn parses_check_host_http_success() {
        let value = json!([[1, 0.12, "OK", "200", "1.2.3.4"]]);
        let parsed = parse_check_host_node(&value).expect("node");
        assert_eq!(parsed, (true, 200, "OK".into()));
    }

    #[test]
    fn parses_check_host_timeout() {
        let value = json!([[0, 5.0, "Connection timed out"]]);
        let parsed = parse_check_host_node(&value).expect("node");
        assert!(!parsed.0);
        assert_eq!(parsed.1, 0);
    }

    #[test]
    fn external_summary_prefers_mcp_ok() {
        let results = vec![
            ("Tokyo".into(), false, 0, "timeout".into()),
            ("Los Angeles".into(), true, 401, "OK".into()),
        ];
        assert_eq!(summarize_external_nodes(&results), ExternalOutcome::McpOk);
    }

    #[test]
    fn http_status_from_headers() {
        let body = "HTTP/1.1 405 Method Not Allowed\r\nServer: axum\r\n";
        assert_eq!(parse_http_status_from_headers(body), Some(405));
    }

    #[test]
    fn verdict_reachable_when_external_mcp_ok() {
        let (verdict, reachable, _, _) = decide_verdict(
            true,
            ExternalOutcome::McpOk,
            HandshakeOutcome::AuthRequired,
        );
        assert_eq!(verdict, "reachable");
        assert!(reachable);
    }

    #[test]
    fn verdict_unreachable_for_private_only() {
        let (verdict, reachable, _, _) = decide_verdict(
            false,
            ExternalOutcome::ProbeUnavailable,
            HandshakeOutcome::Failed,
        );
        assert_eq!(verdict, "unreachable");
        assert!(!reachable);
    }

    #[test]
    fn local_handshake_alone_is_inconclusive() {
        let (verdict, reachable, _, _) = decide_verdict(
            true,
            ExternalOutcome::ProbeUnavailable,
            HandshakeOutcome::McpOk,
        );
        assert_eq!(verdict, "inconclusive");
        assert!(!reachable);
    }
}
