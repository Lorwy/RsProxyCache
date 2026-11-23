# RsProxyCache SDK

组件在宿主 App 进程内运行，支持通过 HTTP Range 读取的单文件 MP4。一个进程启动一次代理，多个播放器可以共享它。请把缓存目录设置为 App 私有缓存目录，所有重定向目标也需要加入源站白名单。

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

宿主关闭最后一个播放器后，可以在后台线程调用 `ProxyCache.close()`。组件抛出的启动或地址生成错误由宿主管理。播放中的请求错误通过播放器报告。

### Media3 验收

`sdk/android/acceptance` 是使用该 AAR 的独立验收 App。用 Gradle 9.3.1 和 Android SDK 36 构建 `assembleDebug`，安装 APK，再通过 `PlaybackActivity` 的 `source` Intent 参数传入支持 Range 的 HTTPS MP4。验收 App 会起播并跳到中间位置，可观察 `RpcAcceptance` 日志。切到后台并返回验证恢复。传入 `--ez warm true` 可在起播前通过代理完整读取视频，测试视频需小于配置的缓存容量。确认落盘后断开模拟器外网、强制停止验收 App，再用同一源地址重新启动。重启时省略 `warm` 参数，观察播放器能否从缓存起播并跳转。

## iOS

用 Xcode 和对应的 Rust Apple targets 构建。

```sh
sh sdk/ios/build.sh
sh sdk/ios/verify-link.sh
```

将 `target/sdk/RsProxyCache.xcframework` 链接到 Swift 工程。最低版本为 iOS 15，支持 arm64 真机与 arm64、x86_64 模拟器。它是静态 XCFramework，在 Xcode 中选择 **Do Not Embed**。链接检查调用了三个 SDK 操作，并覆盖三个架构。

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
