package cn.gavinliu.rpcacceptance;

import android.app.Activity;
import android.net.Uri;
import android.os.Bundle;
import android.os.Handler;
import android.os.Looper;
import android.util.Log;
import androidx.media3.common.MediaItem;
import androidx.media3.common.PlaybackException;
import androidx.media3.common.Player;
import androidx.media3.exoplayer.ExoPlayer;
import androidx.media3.ui.PlayerView;
import cn.gavinliu.rsproxycache.ProxyCache;
import java.io.File;
import java.io.InputStream;
import java.net.HttpURLConnection;
import java.net.URL;
import java.util.Collections;

public final class PlaybackActivity extends Activity {
    private static final String TAG = "RpcAcceptance";
    private ExoPlayer player;
    private boolean seekRequested;
    private final Handler handler = new Handler(Looper.getMainLooper());

    @Override
    public void onCreate(Bundle state) {
        super.onCreate(state);
        PlayerView view = new PlayerView(this);
        setContentView(view);
        String source = getIntent().getStringExtra("source");
        if (source == null) throw new IllegalArgumentException("Provide the source intent extra");
        new Thread(() -> {
            try {
                ProxyCache.start(new File(getCacheDir(), "video"), 64L * 1024 * 1024,
                        Collections.singletonList(Uri.parse(source).getHost()));
                String url = ProxyCache.playbackUrl(source);
                if (getIntent().getBooleanExtra("warm", false)) warmCache(url);
                runOnUiThread(() -> {
                    if (!isDestroyed()) play(view, url);
                });
            } catch (Exception error) {
                Log.e(TAG, "Proxy startup failed", error);
            }
        }).start();
    }

    private void warmCache(String url) throws Exception {
        HttpURLConnection request = (HttpURLConnection) new URL(url).openConnection();
        request.setConnectTimeout(10_000);
        request.setReadTimeout(30_000);
        request.setRequestProperty("Range", "bytes=0-");
        try {
            if (request.getResponseCode() != 206) {
                throw new IllegalStateException("The test source must support byte ranges");
            }
            long total = 0;
            try (InputStream input = request.getInputStream()) {
                byte[] buffer = new byte[64 * 1024];
                int read;
                while ((read = input.read(buffer)) != -1) total += read;
            }
            Log.i(TAG, "Read " + total + " bytes through the proxy");
        } finally {
            request.disconnect();
        }
    }

    private void play(PlayerView view, String url) {
        player = new ExoPlayer.Builder(this).build();
        view.setPlayer(player);
        player.addListener(new Player.Listener() {
            @Override
            public void onRenderedFirstFrame() {
                if (seekRequested) return;
                Log.i(TAG, "Rendered first frame");
                seekRequested = true;
                long target = player.getDuration() / 2;
                player.seekTo(target);
                verifySeek(target, 100);
            }

            @Override
            public void onIsPlayingChanged(boolean playing) {
                if (playing) Log.i(TAG, "Playing");
            }

            @Override
            public void onPlayerError(PlaybackException error) {
                Log.e(TAG, "Playback failed", error);
            }
        });
        player.setMediaItem(MediaItem.fromUri(url));
        player.prepare();
        player.play();
    }

    private void verifySeek(long target, int attemptsLeft) {
        if (player == null) return;
        long position = player.getCurrentPosition();
        if (player.isPlaying() && position >= target && position < target + 5000) {
            Log.i(TAG, "Playing after seek at " + position + " (target " + target + ")");
        } else if (attemptsLeft > 0) {
            handler.postDelayed(() -> verifySeek(target, attemptsLeft - 1), 200);
        } else {
            Log.e(TAG, "Seek did not resume playback");
        }
    }

    @Override
    protected void onStop() {
        if (player != null) {
            Log.i(TAG, "Paused on background at " + player.getCurrentPosition());
            player.pause();
        }
        super.onStop();
    }

    @Override
    protected void onStart() {
        super.onStart();
        if (player != null) {
            Log.i(TAG, "Resuming at " + player.getCurrentPosition());
            player.play();
        }
    }

    @Override
    protected void onDestroy() {
        handler.removeCallbacksAndMessages(null);
        if (player != null) player.release();
        player = null;
        new Thread(ProxyCache::close).start();
        super.onDestroy();
    }
}
