mod detect;

use anyhow::{bail, Context, Result};
use clap::Parser;
use rayon::prelude::*;
use regex::Regex;
use reqwest::{
    blocking::Client,
    header::{HeaderMap, HeaderName, HeaderValue, RANGE},
};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    fs,
    io::Read,
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::{AtomicUsize, Ordering},
    time::{Duration, Instant},
};
use url::Url;

#[derive(Parser, Debug)]
#[command(
    version,
    about = "Download HLS from a playlist or webpage and losslessly remux to MP4. FFmpeg is bundled."
)]
struct Args {
    /// HTTP(S) playlist or webpage URL
    url: Url,
    #[arg(short, long, default_value = "video.mp4")]
    output: PathBuf,
    /// best, worst, or a maximum height such as 720p
    #[arg(short, long, default_value = "best")]
    quality: String,
    /// Inspect discovered streams without downloading media
    #[arg(long)]
    probe: bool,
    /// Select a discovered source (1-based); otherwise ambiguous pages require selection
    #[arg(long)]
    source: Option<usize>,
    #[arg(short = 'j', long, default_value = "6", value_parser = clap::value_parser!(u8).range(1..=32))]
    jobs: u8,
    /// Request header, repeatable: -H 'Authorization: Bearer ...'
    #[arg(short = 'H', long = "header")]
    headers: Vec<String>,
    /// Proxy is OFF by default. `--proxy` uses the system/env proxy;
    /// `--proxy=http://host:port` (or socks5://...) uses a custom one
    #[arg(long, num_args = 0..=1, require_equals = true, default_missing_value = "system")]
    proxy: Option<String>,
    #[arg(long)]
    referer: Option<String>,
    /// Cookie header for the input origin only
    #[arg(long)]
    cookie: Option<String>,
    /// Override AES-128 key (32 hexadecimal digits)
    #[arg(long)]
    key: Option<String>,
    /// Override AES-128 IV (32 hexadecimal digits)
    #[arg(long, requires = "key")]
    iv: Option<String>,
    /// Save only the currently listed segments of a live stream
    #[arg(long)]
    live_snapshot: bool,
    /// Preserve cached downloads after success
    #[arg(long)]
    keep_cache: bool,
    #[arg(long, default_value = ".hls2mp4-cache")]
    cache_dir: PathBuf,
}

fn hash(data: impl AsRef<[u8]>) -> String {
    format!("{:x}", Sha256::digest(data.as_ref()))
}
fn attrs(line: &str) -> HashMap<String, String> {
    let text = line.split_once(':').map(|(_, v)| v).unwrap_or("");
    let re = Regex::new(r#"([A-Z0-9-]+)=(?:"([^"]*)"|([^,]*))"#).unwrap();
    re.captures_iter(text)
        .map(|c| {
            (
                c[1].into(),
                c.get(2).or_else(|| c.get(3)).unwrap().as_str().into(),
            )
        })
        .collect()
}
fn hex16(s: &str) -> Result<Vec<u8>> {
    let s = s.strip_prefix("0x").unwrap_or(s);
    anyhow::ensure!(
        s.len() == 32 && s.is_ascii(),
        "Expected exactly 32 hexadecimal digits"
    );
    (0..32)
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).context("Invalid hexadecimal key/IV"))
        .collect()
}
fn http_url(url: &Url) -> Result<()> {
    anyhow::ensure!(
        matches!(url.scheme(), "http" | "https"),
        "Only HTTP(S) source URLs are supported"
    );
    Ok(())
}
struct Net {
    client: Client,
    cookie: Option<String>,
    origin: url::Origin,
}
impl Net {
    fn new(a: &Args) -> Result<Self> {
        http_url(&a.url)?;
        let mut headers = HeaderMap::new();
        for h in &a.headers {
            let (k, v) = h.split_once(':').context("Header must be Name: value")?;
            anyhow::ensure!(
                !k.trim().eq_ignore_ascii_case("cookie"),
                "Use --cookie for origin-scoped cookies"
            );
            headers.insert(
                HeaderName::from_bytes(k.trim().as_bytes())?,
                HeaderValue::from_str(v.trim())?,
            );
        }
        if let Some(r) = &a.referer {
            headers.insert(reqwest::header::REFERER, HeaderValue::from_str(r)?);
        }
        let mut builder = Client::builder();
        builder = match a.proxy.as_deref() {
            None => builder.no_proxy(),
            Some("system") => builder,
            Some(p) => builder.no_proxy().proxy(reqwest::Proxy::all(p).context("Invalid --proxy URL")?),
        };
        let client = builder
            .default_headers(headers)
            .user_agent("Mozilla/5.0 hls2mp4/0.1")
            .cookie_store(true)
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(180))
            .build()?;
        // Seed the cookie jar through requests only to the initial origin.
        Ok(Self {
            client,
            cookie: a.cookie.clone(),
            origin: a.url.origin(),
        })
    }
    fn get(&self, url: &Url, range: Option<(u64, u64)>) -> Result<reqwest::blocking::Response> {
        http_url(url)?;
        let mut req = self.client.get(url.clone());
        if url.origin() == self.origin {
            if let Some(c) = &self.cookie {
                req = req.header(reqwest::header::COOKIE, c);
            }
        }
        if let Some((start, len)) = range {
            req = req.header(RANGE, format!("bytes={}-{}", start, start + len - 1));
        }
        let response = req.send()?.error_for_status()?;
        if range.is_some() {
            anyhow::ensure!(
                response.status() == 206,
                "Server ignored byte range request"
            );
        }
        Ok(response)
    }
    fn text(&self, url: &Url) -> Result<(Url, String)> {
        let r = self.get(url, None)?;
        let final_url = r.url().clone();
        let mut buf = Vec::new();
        r.take(8 * 1024 * 1024 + 1).read_to_end(&mut buf)?;
        anyhow::ensure!(buf.len() <= 8 * 1024 * 1024, "Page/playlist exceeds 8 MiB");
        Ok((
            final_url,
            String::from_utf8(buf)?
                .trim_start_matches('\u{feff}')
                .into(),
        ))
    }
}

#[derive(Clone)]
struct Source {
    url: Url,
    text: String,
}
fn discover(net: &Net, start: &Url) -> Result<Vec<Source>> {
    let mut queue = VecDeque::from([(start.clone(), 0)]);
    let mut seen = HashSet::new();
    let mut sources = Vec::new();
    while let Some((url, depth)) = queue.pop_front() {
        if !seen.insert(url.to_string()) {
            continue;
        }
        if seen.len() > 24 {
            break;
        }
        let (base, text) = match net.text(&url) {
            Ok(v) => v,
            Err(e) if depth > 0 => {
                eprintln!("Skipped one candidate: {e}");
                continue;
            }
            Err(e) => return Err(e),
        };
        if text.trim_start().starts_with("#EXTM3U") {
            if !sources.iter().any(|s: &Source| s.url == base) {
                sources.push(Source { url: base, text });
            }
            continue;
        }
        if depth >= 3 {
            continue;
        }
        for candidate in detect::candidates(&base, &text)? {
            queue.push_back((candidate, depth + 1));
        }
    }
    anyhow::ensure!(!sources.is_empty(), "No HLS URL found in static HTML/config/iframes. For JavaScript-only or authenticated players, supply the .m3u8 URL from browser Network, with --referer / --cookie if needed. DRM is unsupported.");
    Ok(sources)
}
#[derive(Debug, Clone)]
struct Variant {
    url: Url,
    width: u32,
    height: u32,
    bandwidth: u64,
    codecs: String,
    audio: Option<String>,
}
fn variants(source: &Source) -> Result<Vec<Variant>> {
    let lines: Vec<_> = source.text.lines().map(str::trim).collect();
    let mut result = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        if !line.starts_with("#EXT-X-STREAM-INF:") {
            continue;
        }
        let a = attrs(line);
        let uri = lines[i + 1..]
            .iter()
            .find(|l| !l.is_empty() && !l.starts_with('#'))
            .context("Variant has no URI")?;
        let (w, h) = a
            .get("RESOLUTION")
            .and_then(|s| s.split_once('x'))
            .unwrap_or(("0", "0"));
        result.push(Variant {
            url: source.url.join(uri)?,
            width: w.parse().unwrap_or(0),
            height: h.parse().unwrap_or(0),
            bandwidth: a
                .get("AVERAGE-BANDWIDTH")
                .or(a.get("BANDWIDTH"))
                .and_then(|v| v.parse().ok())
                .unwrap_or(0),
            codecs: a.get("CODECS").cloned().unwrap_or_default(),
            audio: a.get("AUDIO").cloned(),
        });
    }
    Ok(result)
}
fn choose<'a>(v: &'a [Variant], quality: &str) -> Result<&'a Variant> {
    let mut ranked: Vec<_> = v.iter().collect();
    ranked.sort_by_key(|v| (v.height, v.bandwidth));
    match quality {
        "best" => Ok(ranked[ranked.len() - 1]),
        "worst" => Ok(ranked[0]),
        q => {
            let height: u32 = q
                .trim_end_matches('p')
                .parse()
                .context("Quality must be best, worst, or a height such as 720p")?;
            ranked
                .into_iter()
                .rev()
                .find(|v| v.height > 0 && v.height <= height)
                .context("No variant at or below the requested height")
        }
    }
}
fn select(net: &Net, mut source: Source, quality: &str) -> Result<(Source, Option<Source>)> {
    let mut audio = None;
    for _ in 0..8 {
        let vs = variants(&source)?;
        if vs.is_empty() {
            return Ok((source, audio));
        }
        let selected = choose(&vs, quality)?;
        eprintln!(
            "Selected {}x{}, {:.2} Mbps, {}",
            selected.width,
            selected.height,
            selected.bandwidth as f64 / 1e6,
            selected.codecs
        );
        if let Some(group) = &selected.audio {
            let tracks: Vec<_> = source
                .text
                .lines()
                .filter(|l| l.starts_with("#EXT-X-MEDIA:"))
                .map(attrs)
                .filter(|a| {
                    a.get("TYPE").map(String::as_str) == Some("AUDIO")
                        && a.get("GROUP-ID") == Some(group)
                })
                .collect();
            if let Some(track) = tracks
                .iter()
                .find(|a| a.get("DEFAULT").map(String::as_str) == Some("YES"))
                .or(tracks.first())
            {
                if let Some(uri) = track.get("URI") {
                    let (url, text) = net.text(&source.url.join(uri)?)?;
                    audio = Some(Source { url, text });
                }
            }
        }
        let (url, text) = net.text(&selected.url)?;
        source = Source { url, text };
    }
    bail!("Master playlist nesting exceeds 8 levels")
}
#[derive(Clone)]
struct Asset {
    url: Url,
    range: Option<(u64, u64)>,
    path: PathBuf,
    is_key: bool,
}
fn asset(
    dir: &Path,
    url: Url,
    range: Option<(u64, u64)>,
    is_key: bool,
    assets: &mut Vec<Asset>,
) -> String {
    let id = hash(format!("{}|{:?}|{}", url, range, is_key));
    let name = format!("{}.{}", id, if is_key { "key" } else { "bin" });
    let path = dir.join(&name);
    if !assets.iter().any(|a| a.path == path) {
        assets.push(Asset {
            url,
            range,
            path,
            is_key,
        });
    }
    name
}
fn byte_range(value: &str, url: &Url, ends: &mut HashMap<String, u64>) -> Result<(u64, u64)> {
    let (len, start) = match value.split_once('@') {
        Some((l, s)) => (l.parse::<u64>()?, s.parse::<u64>()?),
        None => (
            value.parse::<u64>()?,
            *ends
                .get(url.as_str())
                .context("Implicit byte range has no preceding range for this URI")?,
        ),
    };
    anyhow::ensure!(len > 0, "Empty byte range");
    let end = start.checked_add(len).context("Byte range overflow")?;
    ends.insert(url.to_string(), end);
    Ok((start, len))
}
fn localize(
    source: &Source,
    dir: &Path,
    name: &str,
    a: &Args,
    assets: &mut Vec<Asset>,
) -> Result<PathBuf> {
    anyhow::ensure!(
        source.text.trim_start().starts_with("#EXTM3U"),
        "Response is not an HLS playlist"
    );
    anyhow::ensure!(
        source.text.contains("#EXT-X-ENDLIST") || a.live_snapshot,
        "Live/event playlist: use --live-snapshot to download the current window only"
    );
    let mut output = String::new();
    let mut pending_range: Option<String> = None;
    let mut ends = HashMap::new();
    let mut segment_count = 0;
    let mut key_seen = false;
    let uri_re = Regex::new(r#"URI="[^"]*""#)?;
    let iv_re = Regex::new(r",IV=[^,]*")?;
    for line in source.text.lines().map(str::trim) {
        if line.starts_with("#EXT-X-BYTERANGE:") {
            pending_range = Some(line.split_once(':').unwrap().1.into());
            continue;
        }
        let replaced = if line.starts_with("#EXT-X-KEY:") || line.starts_with("#EXT-X-SESSION-KEY:")
        {
            let attr = attrs(line);
            let method = attr.get("METHOD").map(String::as_str).unwrap_or("");
            if method == "NONE" {
                line.to_owned()
            } else {
                anyhow::ensure!(method == "AES-128" && attr.get("KEYFORMAT").map(String::as_str).unwrap_or("identity") == "identity", "Unsupported encryption/DRM: {method}. Only HLS AES-128 identity encryption is supported");
                key_seen = true;
                let local = if let Some(k) = &a.key {
                    fs::write(dir.join("override.key"), hex16(k)?)?;
                    "override.key".to_owned()
                } else {
                    asset(
                        dir,
                        source
                            .url
                            .join(attr.get("URI").context("AES key has no URI")?)?,
                        None,
                        true,
                        assets,
                    )
                };
                let mut key_line = uri_re
                    .replace(line, format!("URI=\"{local}\""))
                    .into_owned();
                if let Some(iv) = &a.iv {
                    hex16(iv)?;
                    key_line = iv_re.replace(&key_line, "").into_owned();
                    key_line.push_str(&format!(",IV=0x{}", iv.trim_start_matches("0x")));
                }
                key_line
            }
        } else if line.starts_with("#EXT-X-MAP:") {
            let attr = attrs(line);
            let url = source
                .url
                .join(attr.get("URI").context("MAP has no URI")?)?;
            let range = attr
                .get("BYTERANGE")
                .map(|v| byte_range(v, &url, &mut ends))
                .transpose()?;
            let local = asset(dir, url, range, false, assets);
            format!("#EXT-X-MAP:URI=\"{local}\"")
        } else if !line.is_empty() && !line.starts_with('#') {
            segment_count += 1;
            let url = source.url.join(line)?;
            let range = pending_range
                .take()
                .map(|v| byte_range(&v, &url, &mut ends))
                .transpose()?;
            asset(dir, url, range, false, assets)
        } else if line.starts_with("#EXT-X-PART")
            || line.starts_with("#EXT-X-PRELOAD-HINT")
            || line.starts_with("#EXT-X-RENDITION-REPORT")
        {
            continue;
        } else {
            line.to_owned()
        };
        output.push_str(&replaced);
        output.push('\n');
    }
    anyhow::ensure!(segment_count > 0, "Playlist has no complete media segments");
    if a.key.is_some() && !key_seen {
        bail!("--key was supplied but the playlist declares no AES-128 encryption");
    }
    if !source.text.contains("#EXT-X-ENDLIST") {
        output.push_str("#EXT-X-ENDLIST\n");
    }
    let path = dir.join(name);
    fs::write(&path, output)?;
    eprintln!("{name}: {segment_count} segments");
    Ok(path)
}
fn download(net: &Net, asset: &Asset) -> Result<u64> {
    let marker = asset.path.with_extension("sha256");
    if asset.path.exists() && marker.exists() {
        let data = fs::read(&asset.path)?;
        if fs::read_to_string(&marker)? == hash(&data) {
            return Ok(0);
        }
    }
    let mut last = None;
    for attempt in 0..3 {
        let result = (|| -> Result<u64> {
            let mut r = net.get(&asset.url, asset.range)?;
            let expected = r.content_length();
            let partial = asset.path.with_extension("part");
            let mut f = fs::File::create(&partial)?;
            let size = std::io::copy(&mut r, &mut f)?;
            anyhow::ensure!(size > 0, "Empty response");
            if let Some(n) = expected {
                anyhow::ensure!(n == size, "Truncated response");
            }
            if let Some((_, n)) = asset.range {
                anyhow::ensure!(size == n, "Incorrect byte range length");
            }
            if asset.is_key {
                anyhow::ensure!(
                    size == 16,
                    "AES-128 key must be 16 bytes; the server may require authentication"
                );
            }
            f.sync_all()?;
            fs::rename(&partial, &asset.path)?;
            fs::write(&marker, hash(fs::read(&asset.path)?))?;
            Ok(size)
        })();
        match result {
            Ok(size) => return Ok(size),
            Err(e) => last = Some(e),
        }
        if attempt < 2 {
            std::thread::sleep(Duration::from_millis(500 * (attempt + 1)));
        }
    }
    Err(last.unwrap()).context("Asset download failed after 3 attempts")
}
fn unpack_ffmpeg() -> Result<tempfile::TempDir> {
    let dir = tempfile::Builder::new()
        .prefix("hls2mp4-engine-")
        .tempdir()?;
    let bytes = include_bytes!("../assets/ffmpeg.tar.gz");
    tar::Archive::new(flate2::read::GzDecoder::new(&bytes[..])).unpack(dir.path())?;
    Ok(dir)
}
fn remux(video: &Path, audio: Option<&Path>, output: &Path) -> Result<()> {
    let engine = unpack_ffmpeg()?;
    let executable = engine.path().join(if cfg!(windows) {
        "ffmpeg.exe"
    } else {
        "ffmpeg"
    });
    let mut cmd = Command::new(executable);
    cmd.args([
        "-hide_banner",
        "-nostdin",
        "-v",
        "warning",
        "-n",
        "-protocol_whitelist",
        "file,crypto,data",
        "-allowed_extensions",
        "ALL",
        "-extension_picky",
        "0",
        "-i",
    ])
    .arg(video);
    if let Some(audio) = audio {
        cmd.args([
            "-protocol_whitelist",
            "file,crypto,data",
            "-allowed_extensions",
            "ALL",
            "-extension_picky",
            "0",
            "-i",
        ])
        .arg(audio);
        cmd.args(["-map", "0:v:0", "-map", "1:a:0"]);
    } else {
        cmd.args(["-map", "0:v:0", "-map", "0:a:0?"]);
    }
    let result = cmd
        .args(["-c", "copy", "-movflags", "+faststart", "-f", "mp4"])
        .arg(output)
        .output()?;
    anyhow::ensure!(
        result.status.success(),
        "Lossless MP4 remux failed (no transcoding attempted): {}",
        String::from_utf8_lossy(&result.stderr)
    );
    if !result.stderr.is_empty() {
        eprint!("{}", String::from_utf8_lossy(&result.stderr));
    }
    Ok(())
}
fn run(a: Args) -> Result<()> {
    let started = Instant::now();
    if let Some(k) = &a.key {
        hex16(k)?;
    }
    if let Some(iv) = &a.iv {
        hex16(iv)?;
    }
    // Validate quality even for a direct media playlist.
    if !matches!(a.quality.as_str(), "best" | "worst") {
        let h: u32 = a
            .quality
            .trim_end_matches('p')
            .parse()
            .context("Invalid --quality")?;
        anyhow::ensure!(h > 0, "Quality height must be positive");
    }
    let net = Net::new(&a)?;
    eprintln!("Detecting HLS sources…");
    let sources = discover(&net, &a.url)?;
    for (i, s) in sources.iter().enumerate() {
        // Hide query credentials in ordinary logs.
        let mut display = s.url.clone();
        display.set_query(None);
        display.set_fragment(None);
        eprintln!("Source {}: {}", i + 1, display);
        for v in variants(s)? {
            eprintln!(
                "  {}x{}  {:.2} Mbps  {}",
                v.width,
                v.height,
                v.bandwidth as f64 / 1e6,
                v.codecs
            );
        }
    }
    if a.probe {
        return Ok(());
    }
    let index = match a.source {
        Some(i) => i.checked_sub(1).context("--source starts at 1")?,
        None if sources.len() == 1 => 0,
        None => bail!("Multiple sources found; select one with --source N"),
    };
    let source = sources
        .get(index)
        .context("Source index out of range")?
        .clone();
    let (video, audio) = select(&net, source, &a.quality)?;
    anyhow::ensure!(
        !a.output.exists(),
        "Output already exists: {}",
        a.output.display()
    );
    let parent = a
        .output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(parent)?;
    let id = hash(format!(
        "{}|{}|{}|{}|{:?}|{:?}",
        video.url,
        video.text,
        audio
            .as_ref()
            .map(|s| format!("{}{}", s.url, s.text))
            .unwrap_or_default(),
        a.quality,
        a.key,
        a.iv
    ));
    let dir = a.cache_dir.join(&id[..24]);
    fs::create_dir_all(&dir)?;
    let dir = dir.canonicalize()?;
    // An OS-held lock prevents two processes corrupting the same cache.
    let lock_file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join("lock"))?;
    lock_file
        .try_lock()
        .context("This download is already running; use a different --cache-dir")?;
    let mut assets = Vec::new();
    let video_path = localize(&video, &dir, "video.m3u8", &a, &mut assets)?;
    let audio_path = audio
        .as_ref()
        .map(|s| localize(s, &dir, "audio.m3u8", &a, &mut assets))
        .transpose()?;
    let done = AtomicUsize::new(0);
    let bytes = std::sync::atomic::AtomicU64::new(0);
    let transfer_start = Instant::now();
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(a.jobs as usize)
        .build()?;
    pool.install(|| {
        assets.par_iter().try_for_each(|asset| -> Result<()> {
            let n = download(&net, asset)?;
            bytes.fetch_add(n, Ordering::Relaxed);
            let count = done.fetch_add(1, Ordering::Relaxed) + 1;
            if count.is_multiple_of(25) || count == assets.len() {
                eprintln!("Downloaded/verified {count}/{} assets", assets.len());
            }
            Ok(())
        })
    })?;
    let seconds = transfer_start.elapsed().as_secs_f64();
    let size = bytes.load(Ordering::Relaxed) as f64 / 1048576.0;
    eprintln!(
        "Transfer: {size:.1} MiB in {seconds:.2}s ({:.2} MiB/s); cached assets excluded",
        size / seconds.max(0.001)
    );
    eprintln!("Remuxing to MP4 with stream copy (no re-encoding)…");
    let temp = tempfile::Builder::new()
        .prefix(".hls2mp4-")
        .suffix(".mp4")
        .tempfile_in(parent)?;
    let temp_path = temp.into_temp_path();
    // FFmpeg -n expects a nonexistent destination, but the random name remains private to this task.
    fs::remove_file(&temp_path)?;
    remux(&video_path, audio_path.as_deref(), &temp_path)?;
    let final_size = fs::metadata(&temp_path)?.len();
    // persist_noclobber atomically refuses to replace an existing destination.
    temp_path
        .persist_noclobber(&a.output)
        .map_err(|e| e.error)?;
    if !a.keep_cache {
        if let Err(e) = fs::remove_dir_all(&dir) {
            eprintln!("Could not clean cache: {e}");
        }
    }
    drop(lock_file);
    println!(
        "Saved {} ({:.1} MiB), total {:.2}s. Video/audio codecs and quality preserved.",
        a.output.display(),
        final_size as f64 / 1048576.0,
        started.elapsed().as_secs_f64()
    );
    Ok(())
}
fn main() {
    if let Err(e) = run(Args::parse()) {
        eprintln!("Error: {e:#}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn attributes_preserve_codec_commas() {
        let a = attrs("#EXT-X-STREAM-INF:BANDWIDTH=2000000,CODECS=\"avc1.64001f,mp4a.40.2\",RESOLUTION=1280x720");
        assert_eq!(a["CODECS"], "avc1.64001f,mp4a.40.2");
        assert_eq!(a["RESOLUTION"], "1280x720");
    }
    #[test]
    fn implicit_ranges_require_context() {
        let url = Url::parse("https://example.com/media").unwrap();
        let mut ends = HashMap::new();
        assert!(byte_range("20", &url, &mut ends).is_err());
        assert_eq!(byte_range("20@10", &url, &mut ends).unwrap(), (10, 20));
        assert_eq!(byte_range("30", &url, &mut ends).unwrap(), (30, 30));
        assert!(byte_range("0@0", &url, &mut ends).is_err());
    }
    #[test]
    fn quality_chooses_height_before_bitrate() {
        let s = Source { url: Url::parse("https://example.com/master.m3u8").unwrap(), text: "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=9000000,RESOLUTION=1280x720\n720.m3u8\n#EXT-X-STREAM-INF:BANDWIDTH=5000000,RESOLUTION=1920x1080\n1080.m3u8\n".into() };
        let v = variants(&s).unwrap();
        assert_eq!(choose(&v, "best").unwrap().height, 1080);
        assert_eq!(choose(&v, "720p").unwrap().height, 720);
        assert!(choose(&v, "480p").is_err());
    }
    #[test]
    fn key_validation() {
        assert_eq!(hex16("00112233445566778899aabbccddeeff").unwrap().len(), 16);
        assert!(hex16("bad").is_err());
        assert!(hex16("zz112233445566778899aabbccddeeff").is_err());
    }
}
