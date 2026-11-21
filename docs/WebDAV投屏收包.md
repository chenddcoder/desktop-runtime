# 桌面端 WebDAV 投屏收包（真端口实现）

> 2026-11-08 · 对应 `src-tauri/src/webdav.rs`

## 为什么需要它

快应用在 Android 上靠原生插件 `eskit.plugin.webdav.server` 起局域网服务，手机小程序把
`.nes` / `.md` / 图片 PUT 上来落盘。桌面端（WKWebView / WebView2）**没有这个插件**，
而 web-renderer 的 `EsWebdavServerModule` 原本是纯 mock —— 「投屏收包」在 PC 上一直是死路。

这里用 tokio 手写一个够用的 WebDAV 端，把链路补回来：

```
起端口 → 手机 PUT → 落盘 app data → 页面 readBase64 读回 → 应用打开文件
```

## 协议范围

支持：`OPTIONS` / `PUT` / `GET` / `HEAD` / `DELETE` / `MKCOL` / `PROPFIND`，外加验收用的 `GET /__info`。

不支持（**有意**）：锁（LOCK/UNLOCK）、`MOVE` / `COPY`、`Range` 分段。
这不是通用网盘，别按通用网盘验收。

## 目录约定

落盘根 = `<app_data>/es_files`，URL 路径直接映射到根下，所以：

```
PUT /es_files/files/a.nes  →  <app_data>/es_files/es_files/files/a.nes
```

看着多一层 `es_files`，**必须保留**：快应用侧生成的是带 `/es_files/` 前缀的绝对 URL
（与 Android 插件同一口径），少拼这层就和真机路径不一致、页面读不回来。

## 环境变量（一份二进制跑任意快应用）

| 变量 | 作用 |
|---|---|
| `ES_APP_PKG` | 覆盖默认快应用包名（优先级高于编译期 `es-app.config.json`） |
| `ES_APP_NAME` | 覆盖窗口/日志里的应用名 |
| `ES_FILES_DIR` | 覆盖落盘根（验收指到临时目录、或把落盘挪到大容量盘） |

```bash
# 直接拉起卡带机，不改配置、不重新编译
ES_APP_PKG=es.com.nes.tv ES_APP_NAME=卡带机 ./quickapp-desktop
```

> `es-app.config.json` 是 `include_str!` **编译期**注入的，改完必须 rebuild；
> 环境变量就是为了绕开这一点。

## 安全

- **路径穿越**：`safe_join` 必须**先 percent-decode 再校验**。
  `%2e%2e` 解码就是 `..`，只按原文判 `seg == ".."` 会被直接绕过
  （`PUT /%2e%2e/%2e%2e/xxx` 能把文件写成落盘根之外的任意位置）；
  `%2f` / `%5c` 解码出分隔符同样要拒。服务绑 `0.0.0.0`，这不是理论风险。
- **`es_read_file_base64` 只允许读落盘根内**（canonicalize 后 `starts_with` 比较），
  否则等于给 webview 开一个任意文件读取洞。

## 请求体解析的两个坑

1. **HEAD 的 `Content-Length` 只能有一个**。实体必须 0 字节、却要声明真实文件大小，
   于是长度统一由 `write_response_len` 输出，调用方不许再塞第二个 ——
   否则响应里同时出现 `Content-Length: 0` 和 `Content-Length: N`，属于协议违规。
2. **`Transfer-Encoding: chunked` 必须真解析**。按 `Content-Length` 走会把长度当 0，
   结果是**写出 0 字节文件却回 201**——客户端以为成功，数据全丢，排查时最容易被
   误判成「文件损坏」。`read_chunked` 就是为这条路径写的。

## 验收

### 单元 / 端到端（`cargo test`）

```bash
cd desktop-runtime/src-tauri && cargo test --bin quickapp-desktop webdav
```

6 个用例，其中三个是**真起 TcpListener、走生产同一份 `handle`**：

- PUT → 201 + 真落盘 + 字节一致 → GET 回读一致 → HEAD 单一长度 + 0 实体 → PROPFIND 列目录 → DELETE 204
- `%2e%2e` 穿越 → 403，且父目录没被写入
- chunked 两段上传 → 内容完整拼接（不是 0 字节）

> `handle` 的 `AppHandle` 参数做成 `Option`，就是为了让测试能跑真端口
> ——`AppHandle` 只能在 Tauri runtime 里构造。

### 真应用冒烟（curl）

```bash
ES_FILES_DIR=/tmp/probe ES_APP_PKG=es.com.nes.tv ./quickapp-desktop &
curl -s http://127.0.0.1:5990/__info
curl -X PUT --data-binary @a.nes http://127.0.0.1:5990/es_files/files/a.nes   # 201
curl http://127.0.0.1:5990/es_files/files/a.nes -o back.nes                    # md5 一致
```

## 已知限制

- **`~/Library/Application Support` 下 DELETE 会 EPERM**（`Operation not permitted, os error 1`）。
  实测：同一文件 shell `rm`、Python `os.remove` 都能删；换成 `ES_FILES_DIR=/tmp/...`
  后 DELETE 立刻 204。结论是**未签名的裸二进制访问 Library 的 TCC 限制**（写允许、unlink 被拒），
  不是代码问题，打包成正式 `.app`（有 bundle id + 签名）后应正常。
  **验收 DELETE 时请用 `ES_FILES_DIR` 指到临时目录。**
- 端口从 5990 起，被占则顺延（最多 40 个）。**信 `/__info` 或 `start_success` 回传的 url，别硬编码端口**。
- `webdav_stop` 会轮询「端口能否重新 bind」确认真的释放；只 sleep 一个拍脑袋的时长会让
  下次启动顺延到 5991，页面二维码就对不上。
