import RsProxyCache

if let error = rpc_start("/tmp/rpc", 1024, "example.com") {
    rpc_free_string(error)
}
if let address = rpc_playback_url("https://example.com/video.mp4") {
    rpc_free_string(address)
}
rpc_close()
