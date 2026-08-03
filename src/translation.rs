//! Bing 词典查词模块。
//!
//! 深模块、浅接口：对外只有 [`translate`] 一个入口，内部包含两条抓取通道
//! （悬停接口 + 搜索页抓取）、降级链和 HTML 解析。UI 不接触任何 HTTP/解析细节。

use anyhow::Result;
use log::{debug, info, warn};
use std::sync::OnceLock;

const API_UA: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/75.0.3770.100 Safari/537.36";
const FALLBACK_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/90.0.4430.212 Safari/537.36 Edg/90.0.818.62";

/// 共享的 blocking client：连接池跨查询复用，避免每次搜索重建连接 + TLS 握手。
fn client() -> &'static reqwest::blocking::Client {
    static CLIENT: OnceLock<reqwest::blocking::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::blocking::Client::builder()
            .user_agent(API_UA)
            .build()
            .expect("failed to build HTTP client")
    })
}

/// GET 并校验状态码，返回响应文本。UA 可按请求覆盖（fallback 用 Edge UA）。
fn get(url: &str, ua: &str) -> Result<String> {
    let response = client()
        .get(url)
        .header(reqwest::header::USER_AGENT, ua)
        .send()?;
    if !response.status().is_success() {
        return Err(anyhow::anyhow!(
            "API returned status: {}",
            response.status()
        ));
    }
    Ok(response.text()?)
}

/// 未找到时的返回标记
pub const NOT_FOUND: &str = "No Data";

/// 查一个词的释义。
///
/// 先请求 Bing 悬停接口，失败降级到搜索页抓取，再失败返回 [`NOT_FOUND`]。
pub fn translate(word: &str) -> Result<String> {
    debug!("Fetching translation for: {}", word);

    match fetch_from_bing(word) {
        Ok(result) => {
            info!("Successfully fetched translation from API for: {}", word);
            return Ok(result);
        }
        Err(e) => {
            warn!("API request failed: {}, falling back to web scraping", e);
        }
    }

    match fetch_from_bing_fallback(word) {
        Ok(result) => {
            info!("Successfully fetched translation from API for: {}", word);
            return Ok(result);
        }
        Err(e) => {
            warn!("API request failed: {}", e);
        }
    }

    Ok(NOT_FOUND.to_string())
}

fn fetch_from_bing(word: &str) -> Result<String> {
    debug!("Trying Bing API for: {}", word);

    let url = format!(
        "https://cn.bing.com/dict/SerpHoverTrans?q={}",
        urlencoding::encode(word)
    );
    let html = get(&url, API_UA)?;
    let result = parse_bing_response(&html).ok_or(anyhow::anyhow!("parse_bing_response failed"))?;

    if result.is_empty() {
        return Err(anyhow::anyhow!("No valid data from API"));
    }

    Ok(result)
}

fn fetch_from_bing_fallback(word: &str) -> Result<String> {
    let url = format!(
        "https://cn.bing.com/dict/search?q={}",
        urlencoding::encode(word)
    );
    let html = get(&url, FALLBACK_UA)?;
    let result = parse_bing_response_fallback(&html)
        .ok_or(anyhow::anyhow!("parse_bing_response_fallback failed"))?;

    if result.is_empty() {
        return Err(anyhow::anyhow!("No valid data from API"));
    }

    Ok(result)
}

/// 解析悬停接口的 HTML：提取音标 + 词性释义。
fn parse_bing_response(html: &str) -> Option<String> {
    let mut result = String::new();

    // 提取音标
    let phonetic_pattern = r#"<span class="ht_attr" lang=".*?">\[(.*?)\] </span>"#;
    if let Some(caps) = regex_search(phonetic_pattern, html) {
        if !caps.is_empty() {
            // Bing 对部分字符返回 HTML 数字实体（如 &#240; = ð），需解码
            result.push_str(&format!("· [{}]\n", decode_html_entities(caps[0].trim())));
        }
    }

    // 提取词性解释
    let explain_pattern = r#"<span class="ht_pos">(.*?)</span><span class="ht_trs">(.*?)</span>"#;
    let mut explains = Vec::new();
    if let Some(matches) = regex_search_all(explain_pattern, html) {
        for caps in matches {
            if caps.len() >= 2 {
                let pos = decode_html_entities(&caps[0]);
                let trs = decode_html_entities(&caps[1]);
                explains.push(format!("· {} {}", pos, trs));
            }
        }
    }

    if explains.is_empty() && result.is_empty() {
        return None;
    }

    for explain in explains {
        result.push_str(&explain);
        result.push_str("\n");
    }

    if result.ends_with('\n') {
        result.pop();
    }

    Some(result)
}

/// 解析搜索页的 meta description。
///
/// Bing 对不存在的词也会返回 description（如纯 `"词典"` 或只有网络释义建议的样板文案），
/// 这里过滤掉不含真实释义标记（音标或词性）的描述，视为无结果。
fn parse_bing_response_fallback(html: &str) -> Option<String> {
    let start_pattern = r#"<meta name="description" content=""#;
    let end_pattern = r#"" />"#;

    let desc = if let Some(start_pos) = html.find(start_pattern) {
        let after_start = &html[start_pos + start_pattern.len()..];
        if let Some(end_pos) = after_start.find(end_pattern) {
            decode_html_entities(after_start[..end_pos].trim())
        } else {
            return None;
        }
    } else {
        return None;
    };

    if desc.is_empty() || desc.chars().count() < 10 {
        return None;
    }

    // 必须含音标（美[..] 英[..]）或词性标记，否则是样板文案 → 无结果
    let has_phonetic = desc.contains("美[") || desc.contains("英[");
    let pos_re = regex::Regex::new(
        r"(^|[，;\s])(n|v|adj|adv|prep|conj|int|pron|num|art|vt|vi|aux|abbr|det|modal)\.\s",
    )
    .ok()?;
    let has_pos = pos_re.is_match(&desc);
    if !has_phonetic && !has_pos {
        return None;
    }

    Some(desc)
}

// 简单的正则搜索辅助函数
fn regex_search(pattern: &str, text: &str) -> Option<Vec<String>> {
    use regex::Regex;

    let re = Regex::new(pattern).ok()?;
    re.captures(text).map(|caps| {
        caps.iter()
            .skip(1)
            .filter_map(|m| m.map(|m| m.as_str().to_string()))
            .collect()
    })
}

fn regex_search_all(pattern: &str, text: &str) -> Option<Vec<Vec<String>>> {
    use regex::Regex;

    let re = Regex::new(pattern).ok()?;
    let mut results = Vec::new();

    for caps in re.captures_iter(text) {
        let groups: Vec<String> = caps
            .iter()
            .skip(1)
            .filter_map(|m| m.map(|m| m.as_str().to_string()))
            .collect();

        if !groups.is_empty() {
            results.push(groups);
        }
    }

    if results.is_empty() {
        None
    } else {
        Some(results)
    }
}

/// 解码 HTML 数字实体（`&#NNN;` 十进制 / `&#xNN;` 十六进制）和常用命名实体。
/// Bing 的音标/释义里部分字符以实体形式返回（如 `&#240;` = ð）。
fn decode_html_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(pos) = rest.find('&') {
        out.push_str(&rest[..pos]);
        let tail = &rest[pos..];
        if let Some(end) = tail.find(';') {
            let ent = &tail[..=end];
            if let Some(ch) = decode_entity(ent) {
                out.push(ch);
                rest = &tail[end + 1..];
                continue;
            }
        }
        out.push('&');
        rest = &tail[1..];
    }
    out.push_str(rest);
    out
}

fn decode_entity(ent: &str) -> Option<char> {
    if let Some(num) = ent.strip_prefix("&#").and_then(|t| t.strip_suffix(';')) {
        if let Some(hex) = num.strip_prefix('x').or_else(|| num.strip_prefix('X')) {
            return u32::from_str_radix(hex, 16).ok().and_then(char::from_u32);
        }
        return num.parse::<u32>().ok().and_then(char::from_u32);
    }
    match ent {
        "&amp;" => Some('&'),
        "&lt;" => Some('<'),
        "&gt;" => Some('>'),
        "&quot;" => Some('"'),
        "&apos;" => Some('\''),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 真实抓取固化的 HTML 夹具（离线、确定性测试）
    const API_FIXTURE: &str = include_str!("../tests/fixtures/bing_api_hello.html");
    const SEARCH_FIXTURE: &str = include_str!("../tests/fixtures/bing_search_hello.html");

    #[test]
    fn parses_phonetic_and_explanations_from_api() {
        let out = parse_bing_response(API_FIXTURE).expect("should parse api fixture");
        assert!(out.contains("[heˈləʊ]"), "phonetic missing: {out}");
        assert!(
            out.contains("· int. 你好；喂；您好；哈喽"),
            "explanation missing: {out}"
        );
    }

    #[test]
    fn parses_description_from_search_page() {
        let out =
            parse_bing_response_fallback(SEARCH_FIXTURE).expect("should parse search fixture");
        assert!(out.contains("hello"), "word missing: {out}");
        assert!(out.contains("美[heˈləʊ]"), "US phonetic missing: {out}");
        assert!(out.contains("int. 你好"), "explanation missing: {out}");
    }

    #[test]
    fn returns_none_on_garbage() {
        assert!(parse_bing_response("<html><body></body></html>").is_none());
        assert!(parse_bing_response_fallback("<html><head></head></html>").is_none());
        assert!(parse_bing_response("").is_none());
    }

    #[test]
    fn decodes_html_entities_in_phonetic() {
        // Bing 对部分音标字符返回 HTML 实体（&#240; = ð）
        let html = r#"<div><span id="ht_logo"></span><h4>the</h4><span class="ht_attr" lang="en">[&#240;ə] </span><ul><li><span class="ht_pos">art.</span><span class="ht_trs">这；那</span></li></ul></div>"#;
        let out = parse_bing_response(html).expect("should parse");
        assert!(out.contains("[ðə]"), "entity not decoded: {out}");
        assert!(!out.contains("&#"), "entity left raw: {out}");
    }

    #[test]
    fn decodes_hex_and_named_entities() {
        assert_eq!(decode_html_entities("a&#x41;&amp;b"), "aA&b");
    }

    #[test]
    fn rejects_boilerplate_description() {
        // Bing 对不存在的词返回的样板 description：纯“词典”两个字
        let html = r#"<meta name="description" content="词典" />"#;
        assert!(parse_bing_response_fallback(html).is_none());

        // 只有网络释义建议、无真实释义标记
        let html2 = r#"<meta name="description" content="必应词典为您提供asdfghjkl的释义，网络释义： 爱上对方过后就哭了；" />"#;
        assert!(parse_bing_response_fallback(html2).is_none());
    }

    #[test]
    fn parses_multiple_explanations() {
        // 多个 <li> 词性项
        let html = r#"<div><span class="ht_attr" lang="en">[bɑːk] </span>
            <ul>
            <li><span class="ht_pos">v.</span><span class="ht_trs">吠叫</span></li>
            <li><span class="ht_pos">n.</span><span class="ht_trs">树皮</span></li>
            </ul></div>"#;
        let out = parse_bing_response(html).expect("should parse");
        assert!(out.contains("v. 吠叫"), "{out}");
        assert!(out.contains("n. 树皮"), "{out}");
        assert!(out.starts_with("· [bɑːk]"), "{out}");
    }
}
