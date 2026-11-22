//! Ordered attachments. No fetching, decoding buffers, or provider-owned ID guesses.
use proto::protocol::{
    InboundBlock as Block, InboundContent, MediaSource, MessagesRequest, ToolResultContent,
};
use serde_json::{json, Value};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Wire {
    Messages,
    Chat,
    Responses,
}

pub const INVALID: &str = "invalid-attachment";
pub const UNSUPPORTED: &str = "attachment-wire-unsupported";
pub const IMAGE_UNSUPPORTED: &str = "image-input-unsupported";

pub fn has_images(req: &MessagesRequest) -> bool {
    fn images(blocks: &[Block]) -> bool {
        blocks.iter().any(|block| match block {
            Block::Image { .. } => true,
            Block::ToolResult {
                content: ToolResultContent::Blocks(parts),
                ..
            } => images(parts),
            _ => false,
        })
    }
    req.messages
        .iter()
        .any(|m| matches!(&m.content, InboundContent::Blocks(b) if images(b)))
}

pub fn has_documents(req: &MessagesRequest) -> bool {
    fn documents(parts: &[Block]) -> bool {
        parts.iter().any(|block| match block {
            Block::Document { .. } => true,
            Block::ToolResult {
                content: ToolResultContent::Blocks(parts),
                ..
            } => documents(parts),
            _ => false,
        })
    }
    req.messages
        .iter()
        .any(|m| matches!(&m.content, InboundContent::Blocks(parts) if documents(parts)))
}

pub fn has_media(req: &MessagesRequest) -> bool {
    fn media(blocks: &[Block]) -> bool {
        blocks.iter().any(|block| match block {
            Block::Image { .. } | Block::Document { .. } => true,
            Block::ToolResult {
                content: ToolResultContent::Blocks(parts),
                ..
            } => media(parts),
            _ => false,
        })
    }
    req.messages
        .iter()
        .any(|m| matches!(&m.content, InboundContent::Blocks(b) if media(b)))
}

pub fn check(req: &MessagesRequest, wire: Wire, native: bool) -> Result<(), &'static str> {
    fn blocks(
        parts: &[Block],
        wire: Wire,
        native: bool,
        tool: bool,
        role: &str,
    ) -> Result<(), &'static str> {
        for block in parts {
            match block {
                Block::Image { source } | Block::Document { source, .. } => {
                    let image = matches!(block, Block::Image { .. });
                    validate_source(source, image)?;
                    if tool && (wire == Wire::Chat || (!image && wire == Wire::Responses)) {
                        return Err(UNSUPPORTED);
                    }
                    // Without an account ownership proof an opaque ID cannot be routed safely.
                    if matches!(source, MediaSource::File { .. }) {
                        return Err(UNSUPPORTED);
                    }
                    if !native {
                        if role != "user" || (tool && wire == Wire::Chat) {
                            return Err(UNSUPPORTED);
                        }
                        if matches!(source, MediaSource::File { .. } | MediaSource::Unsupported) {
                            return Err(UNSUPPORTED);
                        }
                        // Only plain inline documents have an explicit portable representation.
                        if !image
                            && (wire == Wire::Chat
                                || tool
                                || !matches!(source, MediaSource::Text { media_type, .. } if media_type == "text/plain"))
                        {
                            return Err(UNSUPPORTED);
                        }
                    }
                }
                Block::ToolResult {
                    content: ToolResultContent::Blocks(parts),
                    ..
                } => blocks(parts, wire, native, true, role)?,
                Block::NativeOnly if !native => return Err(UNSUPPORTED),
                _ => {}
            }
        }
        Ok(())
    }
    let mut calls = std::collections::HashSet::new();
    for message in &req.messages {
        if let InboundContent::Blocks(parts) = &message.content {
            blocks(parts, wire, native, false, &message.role)?;
            for block in parts {
                match block {
                    Block::ToolUse { id, .. } => {
                        if !calls.insert(id) && has_media(req) {
                            return Err(UNSUPPORTED);
                        }
                    }
                    Block::ToolResult {
                        tool_use_id,
                        content: ToolResultContent::Blocks(nested),
                        ..
                    } if !native => {
                        if nested.iter().any(|b| {
                            !matches!(
                                b,
                                Block::Text { .. } | Block::Image { .. } | Block::Document { .. }
                            )
                        }) {
                            return Err(UNSUPPORTED);
                        }
                        if !calls.contains(tool_use_id)
                            && nested
                                .iter()
                                .any(|b| matches!(b, Block::Image { .. } | Block::Document { .. }))
                        {
                            return Err(UNSUPPORTED);
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    if wire == Wire::Chat && !native && has_media(req) {
        check_chat_tool_sequence(req)?;
    }
    Ok(())
}

fn validate_source(source: &MediaSource, image: bool) -> Result<(), &'static str> {
    match source {
        MediaSource::Base64 { media_type, data } => {
            if image
                && !matches!(
                    media_type.as_str(),
                    "image/png" | "image/jpeg" | "image/gif" | "image/webp"
                )
            {
                return Err(INVALID);
            }
            let prefix = base64_prefix(data)?;
            let signature = match media_type.as_str() {
                "image/png" => prefix.starts_with(b"\x89PNG\r\n\x1a\n"),
                "image/jpeg" => prefix.starts_with(b"\xff\xd8\xff"),
                "image/gif" => prefix.starts_with(b"GIF87a") || prefix.starts_with(b"GIF89a"),
                "image/webp" => prefix.starts_with(b"RIFF") && prefix.get(8..12) == Some(b"WEBP"),
                "application/pdf" => prefix.starts_with(b"%PDF-"),
                _ => !image && media_type == "text/plain",
            };
            if !signature {
                return Err(INVALID);
            }
        }
        MediaSource::Text { media_type, .. } if !image && media_type == "text/plain" => {}
        MediaSource::Url { url }
            if (url.starts_with("https://") || url.starts_with("http://"))
                && !url.bytes().any(|c| c.is_ascii_control() || c == b' ')
                && url.split_once("://").is_some_and(|(_, rest)| {
                    !rest.split('/').next().unwrap_or_default().is_empty()
                }) => {}
        MediaSource::File { file_id } if !file_id.is_empty() => {}
        _ => return Err(INVALID),
    }
    Ok(())
}

// Validate canonical padded base64 without allocating the decoded attachment.
fn base64_prefix(data: &str) -> Result<Vec<u8>, &'static str> {
    if data.is_empty() || !data.len().is_multiple_of(4) || data.len() / 4 * 3 > 32 * 1024 * 1024 {
        return Err(INVALID);
    }
    let mut prefix = Vec::with_capacity(12);
    for (index, chunk) in data.as_bytes().chunks_exact(4).enumerate() {
        let mut values = [0u8; 4];
        let mut pad = 0;
        for (i, byte) in chunk.iter().enumerate() {
            values[i] = match byte {
                b'A'..=b'Z' if pad == 0 => byte - b'A',
                b'a'..=b'z' if pad == 0 => byte - b'a' + 26,
                b'0'..=b'9' if pad == 0 => byte - b'0' + 52,
                b'+' if pad == 0 => 62,
                b'/' if pad == 0 => 63,
                b'=' if i >= 2 && index == data.len() / 4 - 1 => {
                    pad += 1;
                    0
                }
                _ => return Err(INVALID),
            };
        }
        if (pad == 1 && values[2] & 3 != 0) || (pad == 2 && values[1] & 15 != 0) {
            return Err(INVALID);
        }
        let bytes = [
            values[0] << 2 | values[1] >> 4,
            values[1] << 4 | values[2] >> 2,
            values[2] << 6 | values[3],
        ];
        for byte in bytes.iter().take(3 - pad) {
            if prefix.len() < 12 {
                prefix.push(*byte);
            }
        }
    }
    Ok(prefix)
}

pub fn from_url(url: &str) -> Result<MediaSource, &'static str> {
    if let Some(data) = url.strip_prefix("data:") {
        let (mime, data) = data.split_once(";base64,").ok_or(INVALID)?;
        Ok(MediaSource::Base64 {
            media_type: mime.into(),
            data: data.into(),
        })
    } else {
        Ok(MediaSource::Url { url: url.into() })
    }
}

pub fn parse_part(part: &Value, wire: Wire) -> Result<Option<Block>, &'static str> {
    let kind = part["type"].as_str().unwrap_or_default();
    let file = if wire == Wire::Chat && kind == "file" {
        &part["file"]
    } else {
        part
    };
    if matches!(kind, "image_url" | "input_image" | "file" | "input_file") {
        let keys: &[&str] = if matches!(kind, "image_url" | "input_image") {
            &["image_url", "file_id"]
        } else {
            &["file_data", "file_id", "file_url"]
        };
        if keys.iter().filter(|key| !file[**key].is_null()).count() != 1
            || !file["file_url"].is_null()
        {
            return Err(INVALID);
        }
    }
    match (wire, kind) {
        (Wire::Chat, "image_url") | (Wire::Responses, "input_image") => {
            let url = if wire == Wire::Chat {
                part["image_url"]["url"]
                    .as_str()
                    .or_else(|| part["image_url"].as_str())
            } else {
                part["image_url"].as_str()
            };
            let source = if let Some(url) = url {
                from_url(url)?
            } else {
                MediaSource::File {
                    file_id: part["file_id"].as_str().ok_or(INVALID)?.into(),
                }
            };
            Ok(Some(Block::Image { source }))
        }
        (Wire::Chat, "file") | (Wire::Responses, "input_file") => {
            let file = if wire == Wire::Chat {
                &part["file"]
            } else {
                part
            };
            let source = if let Some(data) = file["file_data"].as_str() {
                let source = from_url(data)?;
                match source {
                    MediaSource::Base64 { media_type, data } if media_type == "text/plain" => {
                        MediaSource::Text {
                            media_type,
                            data: decode_text(&data)?,
                        }
                    }
                    source => source,
                }
            } else {
                MediaSource::File {
                    file_id: file["file_id"].as_str().ok_or(INVALID)?.into(),
                }
            };
            Ok(Some(Block::Document {
                source,
                title: file["filename"].as_str().map(str::to_owned),
            }))
        }
        _ => Ok(None),
    }
}

fn part(block: &Block, wire: Wire, role: &str) -> Result<Value, &'static str> {
    match block {
        Block::Text { text } => Ok(
            json!({"type": if wire == Wire::Chat { "text" } else if role == "assistant" { "output_text" } else { "input_text" }, "text": text}),
        ),
        Block::Image { source } => {
            let url = match source {
                MediaSource::Base64 { media_type, data } => {
                    format!("data:{media_type};base64,{data}")
                }
                MediaSource::Url { url } => url.clone(),
                _ => return Err(UNSUPPORTED),
            };
            Ok(if wire == Wire::Chat {
                json!({"type":"image_url","image_url":{"url":url}})
            } else {
                json!({"type":"input_image","image_url":url})
            })
        }
        // A text document remains a document, not an injected prompt string.
        Block::Document {
            source: MediaSource::Text { data, .. },
            title,
        } => {
            if wire == Wire::Chat {
                return Err(UNSUPPORTED);
            }
            Ok(
                json!({"type":"input_file", "filename":title.as_deref().unwrap_or("document.txt"), "file_data":format!("data:text/plain;base64,{}", encode_base64(data.as_bytes()))}),
            )
        }
        _ => Err(UNSUPPORTED),
    }
}

fn encode_base64(data: &[u8]) -> String {
    const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let a = chunk[0];
        let b = *chunk.get(1).unwrap_or(&0);
        let c = *chunk.get(2).unwrap_or(&0);
        for n in [a >> 2, (a & 3) << 4 | b >> 4] {
            out.push(TABLE[n as usize] as char);
        }
        out.push(if chunk.len() > 1 {
            TABLE[((b & 15) << 2 | c >> 6) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[(c & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

/// Overlay only requests containing media; legacy text/thinking translation stays unchanged.
pub fn encode(req: &MessagesRequest, wire: Wire, mut body: Value) -> Result<Value, &'static str> {
    if !has_media(req) || wire == Wire::Messages {
        return Ok(body);
    }
    check(req, wire, false)?;
    let mut messages = Vec::new();
    if wire == Wire::Chat {
        if let Some(items) = body["messages"].as_array() {
            messages.extend(items.iter().filter(|m| m["role"] == "system").cloned());
        }
    }
    let mut seen = std::collections::HashSet::new();
    for message in &req.messages {
        let blocks = match &message.content {
            InboundContent::Text(text) => vec![Block::Text { text: text.clone() }],
            InboundContent::Blocks(parts) => parts.clone(),
        };
        if wire == Wire::Chat && message.role == "assistant" {
            let mut calls = Vec::new();
            let mut text = String::new();
            for block in &blocks {
                match block {
                    Block::Text { text: part } => text.push_str(part),
                    Block::ToolUse { id, name, input } => {
                        seen.insert(id.clone());
                        calls.push(json!({"id":id,"type":"function","function":{"name":name,"arguments":input.to_string()}}));
                    }
                    _ => {}
                }
            }
            let mut item = json!({"role":"assistant"});
            if !text.is_empty() {
                item["content"] = json!(text);
            }
            if !calls.is_empty() {
                item["tool_calls"] = json!(calls);
            }
            if item.get("content").is_some() || item.get("tool_calls").is_some() {
                messages.push(item);
            }
            continue;
        }
        let mut content = Vec::new();
        for block in blocks {
            let flush = |content: &mut Vec<Value>, messages: &mut Vec<Value>| {
                if !content.is_empty() {
                    messages.push(json!({"role":message.role,"content":std::mem::take(content)}));
                }
            };
            match &block {
                Block::Text { .. } | Block::Image { .. } | Block::Document { .. } => {
                    content.push(part(&block, wire, &message.role)?)
                }
                Block::ToolUse { id, name, input } => {
                    flush(&mut content, &mut messages);
                    seen.insert(id.clone());
                    messages.push(if wire == Wire::Chat { json!({"role":"assistant","tool_calls":[{"id":id,"type":"function","function":{"name":name,"arguments":input.to_string()}}]}) } else { json!({"type":"function_call","call_id":id,"name":name,"arguments":input.to_string()}) });
                }
                Block::ToolResult {
                    tool_use_id,
                    content: output,
                    ..
                } if seen.contains(tool_use_id) => {
                    flush(&mut content, &mut messages);
                    let output = match output {
                        ToolResultContent::Text(text) => json!(text),
                        ToolResultContent::Blocks(_) if wire == Wire::Chat => {
                            json!(output.as_text())
                        }
                        ToolResultContent::Blocks(parts) => Value::Array(
                            parts
                                .iter()
                                .map(|b| part(b, wire, "user"))
                                .collect::<Result<_, _>>()?,
                        ),
                    };
                    messages.push(if wire == Wire::Chat {
                        json!({"role":"tool","tool_call_id":tool_use_id,"content":output})
                    } else {
                        json!({"type":"function_call_output","call_id":tool_use_id,"output":output})
                    });
                }
                Block::Thinking { signature, .. }
                    if wire == Wire::Responses
                        && message.role == "assistant"
                        && !signature.is_empty() =>
                {
                    flush(&mut content, &mut messages);
                    messages.push(json!({"type":"reasoning","encrypted_content":signature}));
                }
                _ => {}
            }
        }
        if !content.is_empty() {
            messages.push(json!({"role":message.role,"content":content}));
        }
    }
    body[if wire == Wire::Chat {
        "messages"
    } else {
        "input"
    }] = Value::Array(messages);
    Ok(body)
}

/// Inspect wire attachment slots even when an otherwise native-only request cannot form a full IR.
pub fn validate_raw(raw: &Value, wire: Wire) -> Result<(), &'static str> {
    let turns = &raw[if wire == Wire::Responses {
        "input"
    } else {
        "messages"
    }];
    for turn in turns.as_array().into_iter().flatten() {
        for parts in [&turn["content"], &turn["output"]] {
            for part in parts.as_array().into_iter().flatten() {
                if let Some(block) = parse_part(part, wire)? {
                    if (wire == Wire::Chat && turn["role"] == "tool")
                        || (wire == Wire::Responses
                            && matches!(
                                turn["type"].as_str(),
                                Some("function_call_output" | "custom_tool_call_output")
                            )
                            && matches!(&block, Block::Document { .. }))
                    {
                        return Err(UNSUPPORTED);
                    }
                    if matches!(
                        &block,
                        Block::Image {
                            source: MediaSource::File { .. }
                        } | Block::Document {
                            source: MediaSource::File { .. },
                            ..
                        }
                    ) {
                        return Err(UNSUPPORTED);
                    }
                    match block {
                        Block::Image { source } => validate_source(&source, true)?,
                        Block::Document { source, .. } => validate_source(&source, false)?,
                        _ => {}
                    }
                }
            }
        }
    }
    Ok(())
}

pub fn raw_has_images(raw: &Value, wire: Wire) -> bool {
    raw[if wire == Wire::Responses {
        "input"
    } else {
        "messages"
    }]
    .as_array()
    .into_iter()
    .flatten()
    .any(|turn| {
        [&turn["content"], &turn["output"]].iter().any(|parts| {
            parts.as_array().into_iter().flatten().any(|part| {
                part["type"]
                    == if wire == Wire::Chat {
                        "image_url"
                    } else {
                        "input_image"
                    }
            })
        })
    })
}

fn decode_text(data: &str) -> Result<String, &'static str> {
    base64_prefix(data)?;
    let value = |byte: u8| match byte {
        b'A'..=b'Z' => byte - b'A',
        b'a'..=b'z' => byte - b'a' + 26,
        b'0'..=b'9' => byte - b'0' + 52,
        b'+' => 62,
        b'/' => 63,
        _ => 0,
    };
    let mut bytes = Vec::with_capacity(data.len() / 4 * 3);
    for chunk in data.as_bytes().chunks_exact(4) {
        let a = value(chunk[0]);
        let b = value(chunk[1]);
        let c = value(chunk[2]);
        let d = value(chunk[3]);
        bytes.push(a << 2 | b >> 4);
        if chunk[2] != b'=' {
            bytes.push(b << 4 | c >> 2);
        }
        if chunk[3] != b'=' {
            bytes.push(c << 6 | d);
        }
    }
    String::from_utf8(bytes).map_err(|_| INVALID)
}

pub fn validate_urls(
    req: &MessagesRequest,
    valid: impl Fn(&str) -> bool + Copy,
) -> Result<(), &'static str> {
    fn blocks(parts: &[Block], valid: impl Fn(&str) -> bool + Copy) -> Result<(), &'static str> {
        for block in parts {
            match block {
                Block::Image {
                    source: MediaSource::Url { url },
                }
                | Block::Document {
                    source: MediaSource::Url { url },
                    ..
                } if !valid(url) => return Err(INVALID),
                Block::ToolResult {
                    content: ToolResultContent::Blocks(parts),
                    ..
                } => blocks(parts, valid)?,
                _ => {}
            }
        }
        Ok(())
    }
    for message in &req.messages {
        if let InboundContent::Blocks(parts) = &message.content {
            blocks(parts, valid)?;
        }
    }
    Ok(())
}

pub fn raw_has_documents(raw: &Value, wire: Wire) -> bool {
    raw[if wire == Wire::Responses {
        "input"
    } else {
        "messages"
    }]
    .as_array()
    .into_iter()
    .flatten()
    .any(|turn| {
        [&turn["content"], &turn["output"]].iter().any(|parts| {
            parts
                .as_array()
                .into_iter()
                .flatten()
                .any(|part| matches!(part["type"].as_str(), Some("input_file" | "file")))
        })
    })
}

pub fn validate_raw_urls(
    raw: &Value,
    wire: Wire,
    valid: impl Fn(&str) -> bool,
) -> Result<(), &'static str> {
    for turn in raw[if wire == Wire::Responses {
        "input"
    } else {
        "messages"
    }]
    .as_array()
    .into_iter()
    .flatten()
    {
        for parts in [&turn["content"], &turn["output"]] {
            for part in parts.as_array().into_iter().flatten() {
                if matches!(part["type"].as_str(), Some("image_url" | "input_image")) {
                    let url = part["image_url"]
                        .as_str()
                        .or_else(|| part["image_url"]["url"].as_str());
                    if url.is_some_and(|url| !url.starts_with("data:") && !valid(url)) {
                        return Err(INVALID);
                    }
                }
            }
        }
    }
    Ok(())
}

// Chat requires the whole batch of results before any new conversation turn.
// Reordering a mixed Messages turn would violate attachment order instead.
fn check_chat_tool_sequence(req: &MessagesRequest) -> Result<(), &'static str> {
    let mut pending = std::collections::HashSet::new();
    for message in &req.messages {
        if message.role == "assistant" && !pending.is_empty() {
            return Err(UNSUPPORTED);
        }
        match &message.content {
            InboundContent::Text(text)
                if message.role == "user" && !text.is_empty() && !pending.is_empty() =>
            {
                return Err(UNSUPPORTED)
            }
            InboundContent::Blocks(parts) => {
                for block in parts {
                    match block {
                        Block::ToolUse { id, .. } if message.role == "assistant" => {
                            if !pending.insert(id.as_str()) {
                                return Err(UNSUPPORTED);
                            }
                        }
                        Block::ToolResult { tool_use_id, .. } if message.role == "user" => {
                            if !pending.remove(tool_use_id.as_str()) {
                                return Err(UNSUPPORTED);
                            }
                        }
                        Block::Text { .. } | Block::Image { .. } | Block::Document { .. }
                            if message.role == "user" && !pending.is_empty() =>
                        {
                            return Err(UNSUPPORTED)
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    if pending.is_empty() {
        Ok(())
    } else {
        Err(UNSUPPORTED)
    }
}
