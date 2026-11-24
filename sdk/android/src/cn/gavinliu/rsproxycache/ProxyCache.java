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

    /** Starts or joins an offline cache task and returns its JSON status. */
    public static String startDownload(String source, long bandwidth) {
        if (bandwidth < 0) throw new IllegalArgumentException("Negative HLS bandwidth");
        String status = nativeDownloadStart(utf8(source), bandwidth);
        if (status == null) throw new IllegalStateException("Proxy unavailable or source invalid or disallowed");
        return status;
    }

    /** Returns the task's JSON status, or null if there is no task. */
    public static String downloadProgress(String source) {
        return nativeDownloadProgress(utf8(source));
    }

    /** Requests cancellation and returns the current JSON status, or null if there is no task. */
    public static String cancelDownload(String source) {
        return nativeDownloadCancel(utf8(source));
    }

    private static byte[] utf8(String value) {
        if (value.indexOf(0) >= 0) throw new IllegalArgumentException("Embedded NUL");
        return (value + "\0").getBytes(StandardCharsets.UTF_8);
    }

    private static native String nativeStart(byte[] directory, long capacity, byte[] hosts);
    private static native String nativePlaybackUrl(byte[] source);
    private static native String nativeDownloadStart(byte[] source, long bandwidth);
    private static native String nativeDownloadProgress(byte[] source);
    private static native String nativeDownloadCancel(byte[] source);
}
