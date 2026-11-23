package cn.gavinliu.rsproxycache;

import java.io.File;
import java.nio.charset.StandardCharsets;
import java.util.List;

/** Process-wide proxy. Perform startup and shutdown off the UI thread. */
public final class ProxyCache {
    static { System.loadLibrary("rsproxycache_jni"); }

    private ProxyCache() {}

    public static void start(File cacheDirectory, long capacityBytes, List<String> originHosts) {
        if (capacityBytes < 0) throw new IllegalArgumentException("Negative cache capacity");
        for (String host : originHosts) {
            if (host.contains(",")) throw new IllegalArgumentException("Expected individual hosts");
        }
        String error = nativeStart(utf8(cacheDirectory.getAbsolutePath()), capacityBytes,
                utf8(String.join(",", originHosts)));
        if (error != null) throw new IllegalStateException(error);
    }

    public static String playbackUrl(String source) {
        String url = nativePlaybackUrl(utf8(source));
        if (url == null) throw new IllegalStateException("Proxy unavailable or source invalid or disallowed");
        return url;
    }

    public static native void close();

    private static byte[] utf8(String value) {
        if (value.indexOf(0) >= 0) throw new IllegalArgumentException("Embedded NUL");
        return (value + "\0").getBytes(StandardCharsets.UTF_8);
    }

    private static native String nativeStart(byte[] directory, long capacity, byte[] hosts);
    private static native String nativePlaybackUrl(byte[] source);
}
