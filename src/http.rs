//! HTTP/REST 型数据库驱动公用工具（ClickHouse / TDengine / InfluxDB）。
//!
//! 这些数据库的 HTTP 接口不提供标准的绑定参数协议，因此统一采用
//! “参数内联为字面量”的方式：值全部来自 [`DbValue`]（非用户拼接的原始 SQL），
//! 并按各方言语义严格转义，避免注入。

use std::time::Duration;

use crate::error::{Error, Result};
use crate::value::{DbValue, format_datetime};

/// 字面量渲染风格（各库字符串转义规则不同）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiteralStyle {
    /// 标准 SQL：单引号内部双写 `''`；二进制 `X'..'`；布尔 1/0（ODBC/HANA 系）
    Standard,
    /// ClickHouse：反斜杠转义；二进制用 `unhex('..')`
    ClickHouse,
    /// TDengine：反斜杠转义（与 MySQL 类似）
    TDengine,
    /// InfluxQL / 行协议：单引号转义 `\'`；布尔用 `true/false`
    Influx,
}

/// 把 SQL 中的 `?` 占位符按顺序替换为字面量（跳过单引号字符串内部）。
pub fn inline_params(sql: &str, params: &[DbValue], style: LiteralStyle) -> Result<String> {
    let mut out = String::with_capacity(sql.len() + params.len() * 8);
    let mut in_string = false;
    let mut index = 0usize;

    for ch in sql.chars() {
        if ch == '\'' {
            in_string = !in_string;
            out.push(ch);
            continue;
        }
        if ch == '?' && !in_string {
            let value = params.get(index).ok_or_else(|| {
                Error::Db("参数数量少于占位符数量（HTTP 型驱动内联参数失败）".into())
            })?;
            out.push_str(&render_literal(value, style));
            index += 1;
            continue;
        }
        out.push(ch);
    }

    if index != params.len() {
        return Err(Error::Db(format!(
            "参数数量（{}）多于占位符数量（{index}）",
            params.len()
        )));
    }
    Ok(out)
}

/// `DbValue` → SQL 字面量。
pub fn render_literal(value: &DbValue, style: LiteralStyle) -> String {
    match value {
        DbValue::Null => "NULL".to_string(),
        DbValue::Bool(v) => match style {
            LiteralStyle::Influx => {
                if *v {
                    "true".into()
                } else {
                    "false".into()
                }
            }
            _ => {
                if *v {
                    "1".into()
                } else {
                    "0".into()
                }
            }
        },
        DbValue::Int(v) => v.to_string(),
        DbValue::Float(v) => {
            if v.is_finite() {
                v.to_string()
            } else {
                "NULL".into()
            }
        }
        DbValue::Decimal(v) => v.to_string(),
        DbValue::Text(v) => quote_string(v, style),
        DbValue::Blob(v) => match style {
            // ClickHouse：十六进制转字符串
            LiteralStyle::ClickHouse => format!("unhex('{}')", to_hex(v)),
            // 标准 SQL：十六进制二进制字面量
            LiteralStyle::Standard => format!("X'{}'", to_hex(v)),
            // TDengine/InfluxDB：二进制字面量不受支持，降级为十六进制文本
            _ => quote_string(&to_hex(v), style),
        },
        DbValue::DateTime(v) => quote_string(&format_datetime(v), style),
    }
}

/// 字符串字面量转义。
pub fn quote_string(text: &str, style: LiteralStyle) -> String {
    match style {
        LiteralStyle::Standard => format!("'{}'", text.replace('\'', "''")),
        _ => format!("'{}'", text.replace('\\', "\\\\").replace('\'', "\\'")),
    }
}

/// 十六进制编码。
pub fn to_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;

    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(text, "{byte:02x}");
    }
    text
}

/// 构建带全局超时的 HTTP 客户端（错误体由调用方读取，便于得出可读错误信息）。
fn agent(timeout: Duration) -> ureq::Agent {
    let config = ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .http_status_as_error(false)
        .build();
    ureq::Agent::new_with_config(config)
}

/// 发送 HTTP 请求（POST 文本），返回响应体文本；非 2xx 时给出带响应体的错误。
pub fn post_text(url: &str, body: &str, auth: Option<(&str, &str)>, timeout: Duration) -> Result<String> {
    send_post(url, body, "text/plain; charset=utf-8", auth, timeout)
}

/// 发送 HTTP 请求（POST JSON），返回响应体文本；非 2xx 时给出带响应体的错误。
pub fn post_json(url: &str, body: &str, auth: Option<(&str, &str)>, timeout: Duration) -> Result<String> {
    send_post(url, body, "application/json; charset=utf-8", auth, timeout)
}

/// 发送 HTTP 请求（POST JSON 体），返回响应体**原始字节**（用于二进制协议应答）。
///
/// <param name="accept_octet_stream">是否声明 `Accept: application/octet-stream`（C# DbServer 二进制应答）</param>
/// <returns>响应体字节</returns>
pub fn post_bytes(
    url: &str,
    body: &str,
    accept_octet_stream: bool,
    auth: Option<(&str, &str)>,
    timeout: Duration,
) -> Result<Vec<u8>> {
    let agent = agent(timeout);
    let mut request = agent
        .post(url)
        .header("Content-Type", "application/json; charset=utf-8");
    if accept_octet_stream {
        request = request.header("Accept", "application/octet-stream");
    }

    if let Some((user, password)) = auth {
        let token = base64_encode(format!("{user}:{password}").as_bytes());
        request = request.header("Authorization", &format!("Basic {token}"));
    }

    let mut response = request
        .send(body)
        .map_err(|e| Error::Db(format!("HTTP 请求失败：{e}")))?;
    let status = response.status();
    let bytes = response
        .body_mut()
        .read_to_vec()
        .map_err(|e| Error::Db(format!("读取 HTTP 响应失败：{e}")))?;
    if !status.is_success() {
        let text = String::from_utf8_lossy(&bytes);
        return Err(Error::Db(format!("HTTP {}：{}", status.as_u16(), text.trim())));
    }
    Ok(bytes)
}

/// POST 请求的公共实现。
fn send_post(
    url: &str,
    body: &str,
    content_type: &str,
    auth: Option<(&str, &str)>,
    timeout: Duration,
) -> Result<String> {
    let agent = agent(timeout);
    let mut request = agent.post(url).header("Content-Type", content_type);

    if let Some((user, password)) = auth {
        let token = base64_encode(format!("{user}:{password}").as_bytes());
        request = request.header("Authorization", &format!("Basic {token}"));
    }

    let mut response = request
        .send(body)
        .map_err(|e| Error::Db(format!("HTTP 请求失败：{e}")))?;
    let status = response.status();
    let text = response
        .body_mut()
        .read_to_string()
        .map_err(|e| Error::Db(format!("读取 HTTP 响应失败：{e}")))?;
    if !status.is_success() {
        return Err(Error::Db(format!("HTTP {}：{}", status.as_u16(), text.trim())));
    }
    Ok(text)
}

/// 发送 HTTP GET，返回响应体文本。
pub fn get_text(url: &str, auth: Option<(&str, &str)>, timeout: Duration) -> Result<String> {
    let agent = agent(timeout);
    let mut request = agent.get(url);

    if let Some((user, password)) = auth {
        let token = base64_encode(format!("{user}:{password}").as_bytes());
        request = request.header("Authorization", &format!("Basic {token}"));
    }

    let mut response = request
        .call()
        .map_err(|e| Error::Db(format!("HTTP 请求失败：{e}")))?;
    let status = response.status();
    let text = response
        .body_mut()
        .read_to_string()
        .map_err(|e| Error::Db(format!("读取 HTTP 响应失败：{e}")))?;
    if !status.is_success() {
        return Err(Error::Db(format!("HTTP {}：{}", status.as_u16(), text.trim())));
    }
    Ok(text)
}

/// Base64 编码（Basic 认证用）——实现已下沉 `dhrust::sign::base64_encode`（2026-10-09 收编；
/// 原"避免引入额外依赖"的内联实现不再需要——dhrust 本就是本仓依赖）。
fn base64_encode(input: &[u8]) -> String {
    dhrust::sign::base64_encode(input)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inline_params_skips_quoted_strings() {
        let sql = "INSERT INTO t (a, b) VALUES (?, 'x?y')";
        let out = inline_params(sql, &[DbValue::Int(1)], LiteralStyle::ClickHouse).unwrap();
        assert_eq!(out, "INSERT INTO t (a, b) VALUES (1, 'x?y')");
    }

    #[test]
    fn inline_params_count_mismatch() {
        assert!(inline_params("SELECT ?", &[], LiteralStyle::TDengine).is_err());
        assert!(inline_params("SELECT 1", &[DbValue::Int(1)], LiteralStyle::TDengine).is_err());
    }

    #[test]
    fn literal_rendering() {
        assert_eq!(render_literal(&DbValue::Null, LiteralStyle::ClickHouse), "NULL");
        assert_eq!(render_literal(&DbValue::Bool(true), LiteralStyle::Influx), "true");
        assert_eq!(render_literal(&DbValue::Bool(true), LiteralStyle::ClickHouse), "1");
        assert_eq!(
            render_literal(&DbValue::Text("it's".into()), LiteralStyle::TDengine),
            "'it\\'s'"
        );
        assert_eq!(
            render_literal(&DbValue::Blob(vec![0x01, 0xab]), LiteralStyle::ClickHouse),
            "unhex('01ab')"
        );
    }

    #[test]
    fn base64_basic() {
        assert_eq!(base64_encode(b"root:taosdata"), "cm9vdDp0YW9zZGF0YQ==");
        assert_eq!(base64_encode(b"a"), "YQ==");
        assert_eq!(base64_encode(b"ab"), "YWI=");
    }
}
