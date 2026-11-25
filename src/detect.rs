//! Register site-specific extractors in `detectors`. Extractors return URLs for
//! the bounded discovery queue; networking, redirects and HLS parsing stay shared.
use anyhow::Result;
use regex::Regex;
use url::Url;

pub trait Detector {
    fn matches(&self, page: &Url) -> bool;
    fn candidates(&self, page: &Url, document: &str) -> Result<Vec<Url>>;
}

struct GenericHtml;
impl Detector for GenericHtml {
    fn matches(&self, _: &Url) -> bool {
        true
    }
    fn candidates(&self, page: &Url, document: &str) -> Result<Vec<Url>> {
        let text = document
            .replace("\\/", "/")
            .replace("\\u0026", "&")
            .replace("\\u002F", "/")
            .replace("\\u002f", "/")
            .replace("&amp;", "&")
            .replace("&#38;", "&")
            .replace("&quot;", "\"");
        let hls = Regex::new(
            r#"(?i)(?:https?://|//|\.\.?/|/)?[^\s"'<>\\{}]+\.m3u8(?:\?[^\s"'<>\\{}]*)?"#,
        )?;
        let frames = Regex::new(r#"(?is)<iframe\b[^>]*\bsrc\s*=\s*["']([^"']+)["']"#)?;
        let config = Regex::new(
            r#"(?i)["']?(?:hls|hls_url|hlsUrl|m3u8|m3u8_url)["']?\s*:\s*["']([^"']+)["']"#,
        )?;
        let tags = Regex::new(r#"(?is)<(?:video|source)\b[^>]*>"#)?;
        let src = Regex::new(r#"(?i)\bsrc\s*=\s*["']([^"']+)["']"#)?;
        let mut raw: Vec<String> = hls.find_iter(&text).map(|m| m.as_str().into()).collect();
        for re in [&frames, &config] {
            raw.extend(re.captures_iter(&text).map(|c| c[1].to_owned()));
        }
        for tag in tags.find_iter(&text) {
            let t = tag.as_str();
            if t.to_ascii_lowercase().contains("mpegurl") {
                if let Some(c) = src.captures(t) {
                    raw.push(c[1].into());
                }
            }
        }
        Ok(raw
            .iter()
            .filter_map(|s| page.join(s).ok())
            .filter(|u| matches!(u.scheme(), "http" | "https"))
            .collect())
    }
}

pub fn candidates(page: &Url, document: &str) -> Result<Vec<Url>> {
    // Put site-specific detectors before GenericHtml, using matches() to scope them.
    let detectors: Vec<Box<dyn Detector>> = vec![Box::new(GenericHtml)];
    let mut urls = Vec::new();
    for detector in detectors {
        if detector.matches(page) {
            urls.extend(detector.candidates(page, document)?);
        }
    }
    Ok(urls)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn escaped_config_and_extensionless_hls() {
        let page = Url::parse("https://example.com/watch/1").unwrap();
        let urls = candidates(&page, r#"<script>v={url:"https:\/\/cdn.example.com\/master.m3u8?x=1\u0026y=2"}</script><source type="application/x-mpegURL" src="/api/stream?id=1&amp;v=2"><iframe src="../player"></iframe>"#).unwrap();
        assert!(urls
            .iter()
            .any(|u| u.as_str() == "https://cdn.example.com/master.m3u8?x=1&y=2"));
        assert!(urls
            .iter()
            .any(|u| u.as_str() == "https://example.com/api/stream?id=1&v=2"));
        assert!(urls
            .iter()
            .any(|u| u.as_str() == "https://example.com/player"));
    }
}
