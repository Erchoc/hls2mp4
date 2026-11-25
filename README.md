# hls2mp4

零依赖的命令行工具：把网上的 m3u8（HLS）视频**无损**转成 MP4。

- 输入 `.m3u8` 链接或视频网页地址，自动探测播放源、并发下载、解密并封装
- 单个可执行文件，内置 FFmpeg，无需 Python / Node / 系统 FFmpeg
- 只做 `-c copy` 封装，不重新编码，画质不损失
- 默认**直连**，不走任何代理；需要时用 `--proxy` 显式开启

## 安装

```sh
brew install Erchoc/tap/hls2mp4
```

目前提供 macOS Apple Silicon 版本。也可以从 [Releases](https://github.com/Erchoc/hls2mp4/releases) 下载二进制，或见下方“从源码构建”。

## 使用

```sh
# m3u8 或网页地址均可；默认选最高画质
hls2mp4 'https://cdn.example.com/master.m3u8' -o movie.mp4
hls2mp4 'https://example.com/watch/123' -o movie.mp4

# 只探测播放源与清晰度，不下载
hls2mp4 'https://example.com/watch/123' --probe

# 画质上限 720p；网页有多个播放源时用 --source 选择
hls2mp4 'https://cdn.example.com/master.m3u8' -q 720p -o movie.mp4
hls2mp4 'https://example.com/watch/123' --source 2 -o movie.mp4

# 请求上下文与并发
hls2mp4 'https://cdn.example.com/master.m3u8' \
  --referer 'https://example.com/watch/123' \
  --cookie 'session=...' -H 'User-Agent: Mozilla/5.0' -j 8 -o movie.mp4
```

URL 请用单引号包裹。输出文件已存在时不会覆盖。

### 代理

| 用法 | 行为 |
| --- | --- |
| 不加参数 | 直连，忽略系统和环境变量里的代理 |
| `--proxy` | 使用系统 / 环境变量代理 |
| `--proxy=http://127.0.0.1:7890` | 使用指定代理（支持 `socks5://`） |

自定义代理必须写成 `--proxy=URL`（带等号）。

### 其他选项

| 选项 | 说明 |
| --- | --- |
| `--key` / `--iv` | 手动指定 AES-128 密钥 / IV（32 位十六进制） |
| `--live-snapshot` | 仅保存直播当前列表中的分片 |
| `--keep-cache` / `--cache-dir` | 保留 / 指定分片缓存，支持断点复用 |

完整参数见 `hls2mp4 --help`。

## 支持范围

- MPEG-TS、fMP4、byte range、AES-128 加密、独立音轨、直播窗口快照
- 网页中的 `.m3u8` 地址、常见播放器配置、最多三层 iframe
- 每个资源失败自动重试 3 次

不支持：DRM（SAMPLE-AES / Widevine / FairPlay）、需要执行 JavaScript 或验证码的页面（此时请从浏览器 Network 面板复制 m3u8 地址再传入）。若源编码无法直接放进 MP4，会报错而不是转码。

## 从源码构建

需要 Rust 1.89+、Python 3 和 FFmpeg 8+（仅构建机需要）：

```sh
python3 scripts/bundle_ffmpeg.py     # 打包 FFmpeg 到 assets/ffmpeg.tar.gz
cargo build --release --locked
python3 tests/integration.py target/release/hls2mp4
```

Linux / Windows 需提供静态 FFmpeg（`--ffmpeg`），尚未验证。

## 许可证

GPL-3.0-or-later，见 `LICENSE`。发行版内置的 FFmpeg 为 GPL / version3 构建，许可说明见 `assets/FFMPEG-LICENSE.md`。
