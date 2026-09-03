//! 连通验证（FR-14 交付闭环，设计 §8.10）：probe_port（端口占用 / 监听
//! 检测）与 mc_ping（纯 Rust 手写 Server List Ping 协议，1.7+ 现代握手）。

use schemars::JsonSchema;
use serde::Deserialize;
use std::time::Duration;

use crate::agent::message::ToolOutcome;

use super::{Tool, ToolCtx, ToolError};

/// SLP 各步骤的默认超时。
const PING_TIMEOUT: Duration = Duration::from_secs(5);

// ---------------------------------------------------------------------------
// probe_port
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ProbePortArgs {
    /// 探测模式：bind = 端口占用检测（部署前）；connect = 监听验证（起服后）
    pub mode: String,
    /// 端口号（1–65535）
    pub port: u16,
    /// 目标地址（默认 127.0.0.1；connect 模式使用）
    #[serde(default)]
    pub host: Option<String>,
}

pub struct ProbePortTool;

#[async_trait::async_trait]
impl Tool for ProbePortTool {
    fn name(&self) -> &'static str {
        "probe_port"
    }
    fn description(&self) -> String {
        "本机端口探测：mode=bind 检查端口是否被占用（部署前必做）；mode=connect 验证某地址端口是否已有服务监听（起服后）。只读。".into()
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::to_value(schemars::schema_for!(ProbePortArgs)).expect("Schema 派生失败")
    }
    fn permission(&self) -> super::Permission {
        super::Permission::ReadOnly
    }
    async fn run(&self, args: serde_json::Value, ctx: &ToolCtx) -> Result<ToolOutcome, ToolError> {
        let args: ProbePortArgs = serde_json::from_value(args)
            .map_err(|err| ToolError::Io(format!("参数解析失败：{err}")))?;
        if ctx.cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        match args.mode.as_str() {
            "bind" => {
                let addr = format!("127.0.0.1:{}", args.port);
                let bindable = tokio::net::TcpListener::bind(&addr).await;
                match bindable {
                    Ok(listener) => {
                        drop(listener);
                        Ok(ToolOutcome::ok(format!(
                            "端口 {addr} 空闲可用（bind 成功），可安全部署。"
                        )))
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::AddrInUse => Ok(
                        ToolOutcome::err(format!("端口 {addr} 已被占用；换端口或先停止占用进程。")),
                    ),
                    Err(err) => Ok(ToolOutcome::err(format!("端口 {addr} 绑定测试失败：{err}"))),
                }
            }
            "connect" => {
                let host = args.host.clone().unwrap_or_else(|| "127.0.0.1".to_string());
                let addr = format!("{host}:{}", args.port);
                let connected =
                    tokio::time::timeout(PING_TIMEOUT, tokio::net::TcpStream::connect(&addr)).await;
                match connected {
                    Err(_) => Ok(ToolOutcome::err(format!(
                        "连接 {addr} 超时（{} 秒）；服务可能未就绪或被防火墙拦截。",
                        PING_TIMEOUT.as_secs()
                    ))),
                    Ok(Err(err)) => Ok(ToolOutcome::err(format!("连接 {addr} 失败：{err}"))),
                    Ok(Ok(_)) => Ok(ToolOutcome::ok(format!(
                        "{addr} 有服务在监听（TCP 握手成功）；可用 mc_ping 进一步确认 MC 协议响应。"
                    ))),
                }
            }
            other => Ok(ToolOutcome::err(format!(
                "未知 mode「{other}」；可选 bind | connect"
            ))),
        }
    }
}

// ---------------------------------------------------------------------------
// mc_ping：Server List Ping（1.7+ 现代协议）
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
pub struct McPingArgs {
    /// 目标地址（默认 127.0.0.1）
    #[serde(default)]
    pub host: Option<String>,
    /// 目标端口（默认从托管服务器的 server.properties 读取，再退回 25565）
    #[serde(default)]
    pub port: Option<u16>,
}

pub struct McPingTool;

/// 追加 VarInt（LEB128）。
fn write_varint(buf: &mut Vec<u8>, mut value: u32) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            buf.push(byte);
            break;
        }
        buf.push(byte | 0x80);
    }
}

/// 组一个带 VarInt 长度前缀的完整包。
pub(crate) fn framed(packet_id: u8, payload: &[u8]) -> Vec<u8> {
    let mut inner = Vec::with_capacity(payload.len() + 1);
    write_varint(&mut inner, packet_id as u32);
    inner.extend_from_slice(payload);
    let mut out = Vec::with_capacity(inner.len() + 5);
    write_varint(&mut out, inner.len() as u32);
    out.extend_from_slice(&inner);
    out
}

/// 构造握手包载荷（next_state=1 表示 status 查询）。
pub(crate) fn handshake_payload(host: &str, port: u16, protocol: i32) -> Vec<u8> {
    let mut payload = Vec::new();
    write_varint(&mut payload, protocol as u32);
    write_varint(&mut payload, host.len() as u32);
    payload.extend_from_slice(host.as_bytes());
    payload.extend_from_slice(&port.to_be_bytes());
    write_varint(&mut payload, 1); // next state: status
    payload
}

/// 读取一个带 VarInt 长度前缀的完整帧（[packet_id][payload...]）。
pub(crate) async fn read_framed(stream: &mut tokio::net::TcpStream) -> Result<Vec<u8>, String> {
    use tokio::io::AsyncReadExt;
    // VarInt 最多 5 字节
    let mut length = 0u32;
    let mut shift = 0;
    loop {
        let mut byte = [0u8; 1];
        stream
            .read_exact(&mut byte)
            .await
            .map_err(|err| format!("读取包长度失败：{err}"))?;
        length |= ((byte[0] & 0x7f) as u32) << shift;
        if byte[0] & 0x80 == 0 {
            break;
        }
        shift += 7;
        if shift > 28 {
            return Err("包长度 VarInt 异常".to_string());
        }
    }
    if length > 1_048_576 {
        return Err(format!("包长度异常：{length}"));
    }
    let mut frame = vec![0u8; length as usize];
    stream
        .read_exact(&mut frame)
        .await
        .map_err(|err| format!("读取包体失败：{err}"))?;
    Ok(frame)
}

/// 从字节切片头部读一个 VarInt，返回 (值, 消耗字节数)。
pub(crate) fn read_varint(buf: &[u8]) -> Result<(u32, usize), String> {
    let mut value = 0u32;
    let mut shift = 0;
    for (index, &byte) in buf.iter().enumerate() {
        value |= ((byte & 0x7f) as u32) << shift;
        if byte & 0x80 == 0 {
            return Ok((value, index + 1));
        }
        shift += 7;
        if shift > 28 {
            return Err("VarInt 编码异常".to_string());
        }
    }
    Err("VarInt 提前结束".to_string())
}

/// 解析 status 响应帧：[packet_id 0x00][VarInt 字符串长度][JSON]。
///
/// MC 协议所有 String 字段都带 VarInt 字节长度前缀——status 响应的 JSON
/// 也不例外（实测 vanilla 1.21.1 响应帧载荷为 `81 01 7b ...`，即 VarInt
/// 129 后跟 129 字节 JSON）。此前漏读该前缀、把长度字节当 JSON 首字节
/// 解析，导致对真实服务器永远报"status 响应不是合法 JSON"。
pub(crate) fn parse_status_frame(frame: &[u8]) -> Result<serde_json::Value, String> {
    if frame.first() != Some(&0x00) {
        let id = frame
            .first()
            .map(|b| format!("0x{b:02x}"))
            .unwrap_or_else(|| "空".into());
        return Err(format!("非 status 响应（packet_id {id}）"));
    }
    let (string_len, consumed) = read_varint(&frame[1..])?;
    let rest = &frame[1 + consumed..];
    let len = string_len as usize;
    if len > rest.len() {
        return Err(format!(
            "status 字符串长度前缀（{len}）超过实际载荷（{} 字节）",
            rest.len()
        ));
    }
    serde_json::from_slice(&rest[..len]).map_err(|err| format!("status 响应不是合法 JSON：{err}"))
}

#[async_trait::async_trait]
impl Tool for McPingTool {
    fn name(&self) -> &'static str {
        "mc_ping"
    }
    fn description(&self) -> String {
        "向 MC 服务器发送 Server List Ping（SLP）协议查询：返回 MOTD / 版本 / 在线人数，是开服交付的最终验证手段。只读。".into()
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::to_value(schemars::schema_for!(McPingArgs)).expect("Schema 派生失败")
    }
    fn permission(&self) -> super::Permission {
        super::Permission::ReadOnly
    }
    async fn run(&self, args: serde_json::Value, ctx: &ToolCtx) -> Result<ToolOutcome, ToolError> {
        let args: McPingArgs = serde_json::from_value(args)
            .map_err(|err| ToolError::Io(format!("参数解析失败：{err}")))?;
        if ctx.cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        let host = args.host.clone().unwrap_or_else(|| "127.0.0.1".to_string());
        // 端口缺省时先问托管服务器的 server.properties，再退回 25565
        let default_dir = ctx.workspace.join("server");
        let port = args.port.unwrap_or_else(|| {
            crate::tools::mc::process::parse_port(&default_dir).unwrap_or(25565)
        });
        let addr = format!("{host}:{port}");

        let result = tokio::time::timeout(PING_TIMEOUT, ping(&addr, &host, port)).await;
        match result {
            Err(_) => Ok(ToolOutcome::err(format!(
                "SLP 查询 {addr} 超时（{} 秒）；服务可能未就绪、端口不对或协议异常。",
                PING_TIMEOUT.as_secs()
            ))),
            Ok(Err(reason)) => Ok(ToolOutcome::err(reason)),
            Ok(Ok(status)) => Ok(ToolOutcome::ok(status)),
        }
    }
}

/// 执行完整 SLP 流程：连接 → 握手 → status 请求 → 解析 JSON。
async fn ping(addr: &str, host: &str, port: u16) -> Result<String, String> {
    let mut stream = tokio::net::TcpStream::connect(addr)
        .await
        .map_err(|err| format!("连接 {addr} 失败：{err}"))?;

    use tokio::io::AsyncWriteExt;
    // 协议版本用 -1（状态查询语义，不触发版本校验）
    let handshake = framed(0x00, &handshake_payload(host, port, -1));
    let request = framed(0x00, &[]);
    stream
        .write_all(&handshake)
        .await
        .map_err(|err| format!("发送握手包失败：{err}"))?;
    stream
        .write_all(&request)
        .await
        .map_err(|err| format!("发送 status 请求失败：{err}"))?;

    let frame = read_framed(&mut stream).await?;
    let json = parse_status_frame(&frame)?;
    let version = json
        .pointer("/version/name")
        .and_then(|v| v.as_str())
        .unwrap_or("未知版本")
        .to_string();
    let players_max = json
        .pointer("/players/max")
        .and_then(|v| v.as_i64())
        .unwrap_or(-1);
    let players_online = json
        .pointer("/players/online")
        .and_then(|v| v.as_i64())
        .unwrap_or(-1);
    let motd = match json.pointer("/description") {
        Some(serde_json::Value::String(text)) => text.clone(),
        Some(other) => other.to_string(),
        None => "（无 MOTD）".to_string(),
    };
    Ok(format!(
        "MC 服务器响应正常（SLP）：\n- 版本：{version}\n- MOTD：{motd}\n- 在线人数：{players_online}/{players_max}\n结论：{addr} 已可正常进服。"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varint_encoding() {
        let mut buf = Vec::new();
        write_varint(&mut buf, 0);
        assert_eq!(buf, vec![0x00]);
        buf.clear();
        write_varint(&mut buf, 1);
        assert_eq!(buf, vec![0x01]);
        buf.clear();
        write_varint(&mut buf, 127);
        assert_eq!(buf, vec![0x7f]);
        buf.clear();
        write_varint(&mut buf, 128);
        assert_eq!(buf, vec![0x80, 0x01]);
        buf.clear();
        write_varint(&mut buf, 2097151);
        assert_eq!(buf, vec![0xff, 0xff, 0x7f]);
    }

    #[test]
    fn handshake_payload_layout() {
        // 协议 -1（varint 0x7f 0xff 0xff 0xff 0x0f）、host "127.0.0.1"、port 25565、next 1
        let payload = handshake_payload("127.0.0.1", 25565, -1);
        let expected: Vec<u8> = [
            &[0xff, 0xff, 0xff, 0xff, 0x0f][..], // protocol = -1 (varint)
            &[0x09],                             // host 长度 9
            b"127.0.0.1",                        // host
            &0x63dd_u16.to_be_bytes(),           // 25565 大端
            &[0x01],                             // next state = 1
        ]
        .concat();
        assert_eq!(payload, expected);
    }

    #[test]
    fn framed_prefixes_length_of_inner() {
        let packet = framed(0x00, b"hello");
        // 长度 6（1 字节 packet_id + 5 字节载荷），packet_id 0，载荷 hello
        assert_eq!(packet, vec![0x06, 0x00, b'h', b'e', b'l', b'l', b'o']);
    }

    #[test]
    fn read_varint_round_trip_and_errors() {
        assert_eq!(read_varint(&[0x7f]), Ok((127, 1)));
        assert_eq!(read_varint(&[0x80, 0x01]), Ok((128, 2)));
        assert_eq!(
            read_varint(&[0xff, 0xff, 0xff, 0xff, 0x0f]),
            Ok((u32::MAX, 5))
        );
        assert!(read_varint(&[0x80]).is_err(), "提前结束应报错");
    }

    /// 黄金向量：解析 vanilla 形状的 status 帧（实测 vanilla 1.21.1 载荷
    /// 首字节即 VarInt 字符串长度，如 `81 01 7b ...` = 129 + JSON）。
    #[test]
    fn parse_status_frame_reads_varint_string_prefix() {
        let json = br#"{"version":{"name":"1.21.1"}}"#;
        let mut payload = Vec::new();
        write_varint(&mut payload, json.len() as u32);
        payload.extend_from_slice(json);
        let mut frame = vec![0x00];
        frame.extend_from_slice(&payload);
        let parsed = parse_status_frame(&frame).unwrap();
        assert_eq!(
            parsed.pointer("/version/name").and_then(|v| v.as_str()),
            Some("1.21.1")
        );

        // 非 0x00 packet_id 应拒绝（而不是把 packet_id 当 JSON 首字节）
        assert!(parse_status_frame(&[0x01, 0x02]).is_err());
        // 长度前缀超过实际载荷应报错
        assert!(parse_status_frame(&[0x00, 0x7f, b'{']).is_err());
    }

    #[tokio::test]
    async fn probe_bind_detects_occupied_and_free() {
        let (tx, _rx) = crate::events::event_channel();
        let ctx = ToolCtx {
            workspace: std::env::temp_dir(),
            data_dir: std::env::temp_dir(),
            http: reqwest::Client::new(),
            cancel: crate::cancel::CancelToken::new(),
            interaction: std::sync::Arc::new(crate::tools::general::tests::QuietInteraction),
            events: tx,
            command_timeout_secs: 10,
            search_backend: String::new(),
            network: Default::default(),
            retrieval: Default::default(),
            curseforge_key: String::new(),
        };
        // 占一个随机空闲端口，bind 模式应报占用
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let outcome = ProbePortTool
            .run(serde_json::json!({ "mode": "bind", "port": port }), &ctx)
            .await
            .unwrap();
        assert!(!outcome.is_ok(), "被占用端口应报错：{outcome:?}");
        drop(listener);
        // 内核释放端口有短暂延迟，让渡一下再探测
        tokio::time::sleep(Duration::from_millis(100)).await;
        let outcome = ProbePortTool
            .run(serde_json::json!({ "mode": "bind", "port": port }), &ctx)
            .await
            .unwrap();
        assert!(outcome.is_ok(), "释放后应可绑定：{outcome:?}");
    }

    #[tokio::test]
    async fn probe_unknown_mode_rejected() {
        let (tx, _rx) = crate::events::event_channel();
        let ctx = ToolCtx {
            workspace: std::env::temp_dir(),
            data_dir: std::env::temp_dir(),
            http: reqwest::Client::new(),
            cancel: crate::cancel::CancelToken::new(),
            interaction: std::sync::Arc::new(crate::tools::general::tests::QuietInteraction),
            events: tx,
            command_timeout_secs: 10,
            search_backend: String::new(),
            network: Default::default(),
            retrieval: Default::default(),
            curseforge_key: String::new(),
        };
        let outcome = ProbePortTool
            .run(serde_json::json!({ "mode": "nope", "port": 25565 }), &ctx)
            .await
            .unwrap();
        assert!(!outcome.is_ok());
    }

    /// 假 MC 服务器：按 SLP 协议应答握手 + status 请求，验证 mc_ping 全链路。
    /// 响应载荷形状与真实 vanilla 一致：[packet_id][VarInt 字符串长度][JSON]。
    #[tokio::test]
    async fn mc_ping_against_fake_slp_server() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.expect("accept");
            let _handshake = read_framed(&mut sock).await.expect("握手包");
            let _request = read_framed(&mut sock).await.expect("status 请求");
            let status = r#"{"version":{"name":"1.21.1"},"players":{"online":2,"max":10},"description":"测试服 MOTD"}"#;
            let mut payload = Vec::new();
            write_varint(&mut payload, status.len() as u32); // String 的 VarInt 长度前缀
            payload.extend_from_slice(status.as_bytes());
            let response = framed(0x00, &payload);
            use tokio::io::AsyncWriteExt;
            sock.write_all(&response).await.expect("写响应");
        });

        let (tx, _rx) = crate::events::event_channel();
        let ctx = ToolCtx {
            workspace: std::env::temp_dir(),
            data_dir: std::env::temp_dir(),
            http: reqwest::Client::new(),
            cancel: crate::cancel::CancelToken::new(),
            interaction: std::sync::Arc::new(crate::tools::general::tests::QuietInteraction),
            events: tx,
            command_timeout_secs: 10,
            search_backend: String::new(),
            network: Default::default(),
            retrieval: Default::default(),
            curseforge_key: String::new(),
        };
        let outcome = McPingTool
            .run(serde_json::json!({ "port": port }), &ctx)
            .await
            .unwrap();
        let ToolOutcome::Ok { content } = outcome else {
            panic!("SLP 应成功：{outcome:?}");
        };
        assert!(content.contains("1.21.1"), "{content}");
        assert!(content.contains("测试服 MOTD"), "{content}");
        assert!(content.contains("2/10"), "{content}");
    }
}
