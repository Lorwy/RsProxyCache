import RsProxyCache

if let error = rpc_start("/tmp/rpc", 1024, "example.com") {
    rpc_free_string(error)
}
if let address = rpc_playback_url("https://example.com/video.mp4") {
    rpc_free_string(address)
}
if let task = rpc_download_start("https://example.com/video.mp4", 0) {
    rpc_free_string(task)
}
if let progress = rpc_download_progress("https://example.com/video.mp4") {
    rpc_free_string(progress)
}
if let cancelled = rpc_download_cancel("https://example.com/video.mp4") {
    rpc_free_string(cancelled)
}
rpc_close()
