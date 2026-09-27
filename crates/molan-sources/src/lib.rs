// molan-sources: 书源引擎（搜索/目录/正文抓取，对齐 handlers-sources.js + htmlmini.js）
use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::path::PathBuf;

fn sources_file(data_dir: &std::path::Path) -> PathBuf {
    data_dir.join("sources.json")
}

fn load_sources(data_dir: &std::path::Path) -> Result<Vec<Value>> {
    let s = std::fs::read_to_string(sources_file(data_dir))?;
    Ok(serde_json::from_str::<Value>(&s)?
        .as_array()
        .cloned()
        .unwrap_or_default())
}

fn find_source(data_dir: &std::path::Path, source_id: &str) -> Result<Value> {
    load_sources(data_dir)?
        .into_iter()
        .find(|s| s["id"].as_str() == Some(source_id))
        .ok_or_else(|| anyhow!("未知书源 {}", source_id))
}

/// 模块级 HTTP 单例：复用连接池与 TLS 会话（UA 与 20s 总超时保持不变）
fn http_client() -> Result<&'static reqwest::Client> {
    static C: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    if let Some(c) = C.get() {
        return Ok(c);
    }
    let built = reqwest::Client::builder()
        .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/127.0 Safari/537.36")
        .timeout(std::time::Duration::from_secs(20))
        .build()?;
    let _ = C.set(built);
    Ok(C.get().expect("http client initialized"))
}

async fn fetch_html(url: &str) -> Result<String> {
    let client = http_client()?;
    let resp = client.get(url).send().await?;
    let status = resp.status().as_u16();
    if !(200..300).contains(&status) {
        return Err(anyhow!("抓取失败 {}: {}", status, url));
    }
    let bytes = resp.bytes().await?;
    let (html, _, had_errors) = encoding_rs::UTF_8.decode(&bytes);
    if had_errors && html.matches('\u{FFFD}').count() > 30 {
        let (gbk, _, _) = encoding_rs::GBK.decode(&bytes);
        return Ok(gbk.to_string());
    }
    Ok(html.to_string())
}

fn rel_url(base: &str, link: &str) -> String {
    let l = link.trim();
    if l.starts_with("http://") || l.starts_with("https://") {
        return l.to_string();
    }
    // 简易相对地址解析
    if let Some(idx) = base.find("://") {
        if let Some(slash) = base[idx + 3..].find('/') {
            let origin = &base[..idx + 3 + slash];
            if l.starts_with('/') {
                return format!("{}{}", origin, l);
            }
            return format!("{}/{}", origin.trim_end_matches('/'), l);
        }
    }
    l.to_string()
}

fn qsel_text(html: &str, sel: &str) -> Option<String> {
    let doc = scraper::Html::parse_fragment(html);
    doc.select(&scraper::Selector::parse(sel).ok()?)
        .next()
        .map(|el| clean_text(&el.text().collect::<String>()))
}

fn qsel_attr(html: &str, sel: &str, attr: &str) -> Option<String> {
    let doc = scraper::Html::parse_fragment(html);
    doc.select(&scraper::Selector::parse(sel).ok()?)
        .next()
        .and_then(|el| el.value().attr(attr))
        .map(|s| s.to_string())
}

fn clean_text(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

// 正文清洗（去广告行）
fn clean_body(body: &str, junk: &[Value]) -> String {
    let mut lines: Vec<String> = Vec::new();
    for line in body.lines() {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        let junk_hit = junk
            .iter()
            .any(|j| j.as_str().map(|s| t.contains(s)).unwrap_or(false));
        if !junk_hit {
            lines.push(t.to_string());
        }
    }
    lines.join("\n")
}

pub async fn search_books(
    data_dir: &std::path::Path,
    source_id: &str,
    keyword: &str,
) -> Result<Value> {
    let src = find_source(data_dir, source_id)?;
    let search = match src["search"].as_object() {
        Some(s) => s,
        None => return Ok(json!([])),
    };
    let url_tpl = search["url"].as_str().unwrap_or("");
    if url_tpl.is_empty() {
        return Ok(json!([]));
    }
    let url = url_tpl.replace("{{key}}", &percent_encode(keyword));
    let html = fetch_html(&url).await?;
    let doc = scraper::Html::parse_document(&html);
    let item_sel = scraper::Selector::parse(search["item"].as_str().unwrap_or("div.listitem"))
        .map_err(|_| anyhow!("书源 item 选择器无效"))?;
    let name_sel = search["name"].as_str().unwrap_or("h2").to_string();
    let link_sel = search["link"].as_str().unwrap_or("a").to_string();
    let author_sel = search["author"].as_str().unwrap_or("").to_string();
    let base = src["baseUrl"].as_str().unwrap_or("");
    let mut out = Vec::new();
    for it in doc.select(&item_sel).take(30) {
        let frag_html = it.html();
        let title = qsel_text(&frag_html, &name_sel)
            .or_else(|| qsel_text(&frag_html, &link_sel))
            .unwrap_or_default();
        let link = qsel_attr(&frag_html, &link_sel, "href").unwrap_or_default();
        if title.is_empty() || link.is_empty() {
            continue;
        }
        let author = if author_sel.is_empty() {
            String::new()
        } else {
            qsel_text(&frag_html, &author_sel).unwrap_or_default()
        };
        out.push(
            json!({"title": title, "name": title, "author": author, "url": rel_url(base, &link)}),
        );
    }
    Ok(json!(out))
}

fn percent_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char)
            }
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

pub async fn fetch_book_catalog(
    data_dir: &std::path::Path,
    source_id: &str,
    book_url: &str,
) -> Result<Value> {
    let src = find_source(data_dir, source_id)?;
    let cat = src["catalog"]
        .as_object()
        .ok_or_else(|| anyhow!("书源无目录配置"))?;
    let cleaned = book_url.trim_end_matches('/');
    let url = cat["urlTemplate"]
        .as_str()
        .filter(|t| !t.is_empty())
        .map(|t| t.replace("{{url}}", cleaned))
        .unwrap_or_else(|| cleaned.to_string());
    let html = match fetch_html(&url).await {
        Ok(h) => h,
        Err(e) => {
            if url != cleaned {
                fetch_html(cleaned).await?
            } else {
                return Err(e);
            }
        }
    };
    let doc = scraper::Html::parse_document(&html);
    let title = cat["bookName"]
        .as_str()
        .filter(|s| !s.is_empty())
        .and_then(|sel| {
            scraper::Selector::parse(sel).ok().and_then(|s| {
                doc.select(&s)
                    .next()
                    .map(|el| clean_text(&el.text().collect::<String>()))
            })
        })
        .unwrap_or_default();
    let item_sel = scraper::Selector::parse(cat["item"].as_str().unwrap_or("#list dd a"))
        .map_err(|_| anyhow!("书源目录选择器无效"))?;
    let base = src["baseUrl"].as_str().unwrap_or("");
    let mut chapters = Vec::new();
    for it in doc.select(&item_sel) {
        let name = clean_text(&it.text().collect::<String>());
        let link = it.value().attr("href").unwrap_or("");
        if name.is_empty() || link.is_empty() {
            continue;
        }
        chapters.push(json!({"name": name, "title": name, "url": rel_url(base, link)}));
    }
    Ok(json!({"bookName": if title.is_empty() { cleaned } else { &title }, "chapters": chapters}))
}

pub async fn fetch_chapter_text(
    data_dir: &std::path::Path,
    source_id: &str,
    url: &str,
) -> Result<String> {
    let src = find_source(data_dir, source_id)?;
    let html = fetch_html(url).await?;
    let body_sel = src["content"]["body"].as_str().unwrap_or("#content");
    let junk: Vec<Value> = src["content"]["junkLines"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let doc = scraper::Html::parse_document(&html);
    let text = scraper::Selector::parse(body_sel)
        .ok()
        .and_then(|s| {
            doc.select(&s)
                .next()
                .map(|el| el.text().collect::<String>())
        })
        .unwrap_or_default();
    Ok(clean_body(&text, &junk))
}

pub fn list_sources(data_dir: &std::path::Path) -> Value {
    let mut srcs = load_sources(data_dir).unwrap_or_default();
    for s in srcs.iter_mut() {
        s["enabled"] = json!(true);
    }
    json!(srcs)
}
