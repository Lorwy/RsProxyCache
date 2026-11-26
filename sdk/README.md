# RsProxyCache SDK

组件在宿主 App 进程内运行，支持通过 HTTP Range 读取的单文件 MP4 和 HLS 点播。一个进程启动一次代理，多个播放器可以共享它。请把缓存目录设置为 App 私有缓存目录，所有重定向目标也需要加入源站白名单。

在后台线程进行代理启停。启动成功后才生成代理播放地址，并直接把返回值交给播放器。代理重启后用原始源地址重新生成地址。源地址和代理播放地址可能包含凭据，应避免写入日志。

## Android

用 Android NDK 28、JDK 和对应的 Rust Android targets 构建 AAR。

```sh
sh sdk/android/build.sh
```

构建脚本读取 `ANDROID_NDK_HOME`。产物位于 `target/sdk/rs-proxy-cache.aar`，包含 arm64-v8a、armeabi-v7a、x86_64 和 x86。最低版本为 API 24。HTTPS 回源使用随组件打包的 WebPKI 公共根证书。

将 AAR 加入宿主依赖，并在宿主的 Android 网络安全配置中允许播放器对 `127.0.0.1` 发起明文 HTTP 请求。AAR 声明了 `INTERNET` 权限。可参考 [验收 App 的网络配置](android/acceptance/src/main/res/xml/network_security_config.xml)。

```kotlin
import androidx.media3.common.MediaItem
import cn.gavinliu.rsproxycache.ProxyCache
import java.io.File

// 在后台线程初始化。
ProxyCache.start(File(context.cacheDir, "video"), 64L * 1024 * 1024,
    listOf("media.example.com"))
val address = ProxyCache.playbackUrl(sourceUrl)

// 在播放器线程设置媒体。
player.setMediaItem(MediaItem.fromUri(address))
player.prepare()
player.play()
```

Media3 播放 HLS 需要加入与播放器版本一致的 `media3-exoplayer-hls`。如果源地址没有 `.m3u8` 后缀，使用 `MediaItem.Builder().setUri(address).setMimeType(MimeTypes.APPLICATION_M3U8)` 传给播放器。HLS 的子列表、分片和密钥如果来自其他域名，也需在启动时配置其源站白名单。

宿主关闭最后一个播放器后，可以在后台线程调用 `ProxyCache.close()`。组件抛出的启动或地址生成错误由宿主管理。播放中的请求错误通过播放器报告。

### Media3 验收

`sdk/android/acceptance` 是使用该 AAR 的独立验收 App。用 Gradle 9.3.1 和 Android SDK 36 构建 `assembleDebug`，安装 APK，再通过 `PlaybackActivity` 的 `source` Intent 参数传入支持 Range 的 HTTPS MP4。验收 App 会起播并跳到中间位置，可观察 `RpcAcceptance` 日志。切到后台并返回验证恢复。传入 `--ez warm true` 可在起播前通过代理完整读取视频，测试视频需小于配置的缓存容量。确认落盘后断开模拟器外网、强制停止验收 App，再用同一源地址重新启动。重启时省略 `warm` 参数，观察播放器能否从缓存起播并跳转。

HLS 使用相同的 `source` 参数。可加 `--ez hls true` 显式设置 Media3 的媒体类型，并加 `--ez verifyTracks true` 在跳转成功后切换到可用的其他视频、音轨和字幕轨道，通过 `EventLogger` 检查实际加载的格式。验收已发布的明文 HLS 时，断网重启 App，使用同一个源地址生成新的代理播放地址。AES-128 HLS 的播放列表和密钥仍需回源。

验收离线下载时传入 `--ez downloadCycle true` 和唯一的 `--es cacheRun <名称>`。App 调用 SDK 下载，达到 `--el cancelAfterBytes <字节数>` 后取消，检查本轮保留的媒体数据，再重新发起至完成。默认取消阈值为 1,200,000 字节，HLS 可按分片大小调整。完成后断网并强制停止 App，再用相同的 `source` 和 `cacheRun` 启动，省略 `downloadCycle`。在 `RpcAcceptance` 日志中查看首帧和跳转后的播放位置。

## iOS

用 Xcode 和对应的 Rust Apple targets 构建。

```sh
sh sdk/ios/build.sh
sh sdk/ios/verify-link.sh
```

将 `target/sdk/RsProxyCache.xcframework` 链接到 Swift 工程。最低版本为 iOS 15，支持 arm64 真机与 arm64、x86_64 模拟器。它是静态 XCFramework，在 Xcode 中选择 **Do Not Embed**。链接检查覆盖三个架构及播放和离线缓存接口。

```swift
import Foundation
import RsProxyCache

let directory = FileManager.default.urls(for: .cachesDirectory, in: .userDomainMask)[0]
    .appendingPathComponent("video")
if let error = rpc_start(directory.path, 64 * 1024 * 1024, "media.example.com") {
    let message = String(cString: error)
    rpc_free_string(error)
    // 由宿主处理 message。
} else if let address = rpc_playback_url(sourceUrl) {
    let playbackAddress = String(cString: address)
    rpc_free_string(address)
    // 将 playbackAddress 交给播放器。
}
```

`rpc_playback_url` 返回空指针时，宿主可检查代理状态和源地址，并决定如何恢复。关闭代理时调用 `rpc_close()`。

## 离线缓存

| 操作 | Android | iOS | 独立服务 |
| --- | --- | --- | --- |
| 开始 | `ProxyCache.startDownload(source, bandwidth)` | `rpc_download_start(source, bandwidth)` | `POST /v1/offline/{完整源地址}` |
| 查询 | `ProxyCache.downloadProgress(source)` | `rpc_download_progress(source)` | `GET /v1/offline/{完整源地址}` |
| 取消 | `ProxyCache.cancelDownload(source)` | `rpc_download_cancel(source)` | `DELETE /v1/offline/{完整源地址}` |

使用 HLS 主播放列表的 `BANDWIDTH` 指定清晰度。SDK 传 0 或 HTTP 请求省略 `X-RPC-Bandwidth` 时，优先选择最近播放的版本，首次选择带宽最低的版本。同一任务的并发请求共享进度。不同清晰度的请求在当前任务运行时收到 HTTP 409 或 SDK 返回的 `conflict: true`。

状态 JSON 包含 `state`、`processed_bytes` 和 `bandwidth`。处理字节数包括本地复用的数据。`state` 可为 `running`、`completed`、`cancelled`、`unsupported`、`insufficient_capacity` 或 `failed`。取消是异步的，可以继续查询直到任务结束。查询不存在的任务时，HTTP 返回 404，SDK 返回空值。iOS 调用方用 `rpc_free_string` 释放返回的 JSON 字符串。

只有缓存配额无法满足时才返回 `insufficient_capacity`。失败时可检查 `error_code`。磁盘写满为 `disk_full`，缓存写入或发布失败分别为 `cache_write_failed` 和 `publish_failed`。回源响应错误、超时和读取响应体失败分别为 `origin_response`、`origin_timeout` 和 `origin_body_failed`。若 DNS 解析结果全部被回源地址规则拒绝，则为 `origin_address_denied`。其他失败为 `download_failed`。

默认同时运行两个离线任务，另有 16 个等待名额。排队期间状态仍为 `running`，`queued` 为 `true`，可以取消。队列满时，HTTP 返回 429，SDK 返回 `state: "failed"` 和 `error_code: "queue_full"`，释放名额后可重试。独立服务可用 `RPC_DOWNLOAD_PARALLELISM` 和 `RPC_DOWNLOAD_QUEUE_CAPACITY` 调整这两个上限。

任务随代理进程结束，重新发起时复用有效数据。已发布的 MP4 和明文 HLS 优先按源地址的 `host/path` 保存在缓存根目录中，仍参与容量淘汰。需要长期保存时可复制媒体文件，复制 HLS 时需保留播放列表和资源之间的相对位置。
