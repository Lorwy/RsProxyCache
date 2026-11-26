package cn.gavinliu.rpcacceptance;

import android.app.Activity;
import android.content.Intent;
import android.net.Uri;
import android.os.Bundle;
import android.text.InputType;
import android.view.inputmethod.InputMethodManager;
import android.widget.Button;
import android.widget.CheckBox;
import android.widget.EditText;
import android.widget.LinearLayout;
import android.widget.ScrollView;
import android.widget.TextView;
import androidx.media3.common.MediaItem;
import androidx.media3.common.MimeTypes;
import androidx.media3.common.PlaybackException;
import androidx.media3.common.Player;
import androidx.media3.exoplayer.ExoPlayer;
import androidx.media3.ui.PlayerView;
import cn.gavinliu.rsproxycache.ProxyCache;
import java.io.File;
import java.util.ArrayList;
import java.util.LinkedHashSet;
import java.util.Locale;
import java.util.Set;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;

public final class SampleActivity extends Activity {
    private static final ExecutorService PROXY = Executors.newSingleThreadExecutor();
    static final String CACHE_DIRECTORY = "video";
    private ExoPlayer player;
    private EditText sourceInput;
    private EditText additionalHosts;
    private CheckBox forceHls;
    private Button play;
    private TextView status;
    private boolean started;
    private boolean proxyStarted; // Accessed on PROXY.

    @Override
    protected void onCreate(Bundle state) {
        super.onCreate(state);
        LinearLayout content = new LinearLayout(this);
        content.setOrientation(LinearLayout.VERTICAL);
        int padding = Math.round(16 * getResources().getDisplayMetrics().density);
        content.setPadding(padding, padding, padding, padding);
        ScrollView scroll = new ScrollView(this);
        scroll.setFitsSystemWindows(true);
        scroll.addView(content);
        setContentView(scroll);

        sourceInput = new EditText(this);
        sourceInput.setId(1001);
        sourceInput.setHint("Video URL (MP4 or HLS)");
        sourceInput.setInputType(InputType.TYPE_CLASS_TEXT | InputType.TYPE_TEXT_VARIATION_URI);
        sourceInput.setSingleLine();
        content.addView(sourceInput);

        additionalHosts = new EditText(this);
        additionalHosts.setId(1002);
        additionalHosts.setHint("Additional origin hosts (comma-separated)");
        additionalHosts.setInputType(InputType.TYPE_CLASS_TEXT | InputType.TYPE_TEXT_VARIATION_URI);
        additionalHosts.setSingleLine();
        content.addView(additionalHosts);

        forceHls = new CheckBox(this);
        forceHls.setId(1003);
        forceHls.setText("Force HLS for URLs without .m3u8");
        content.addView(forceHls);

        play = new Button(this);
        play.setText("Play through proxy");
        play.setOnClickListener(view -> startPlayback());
        content.addView(play);

        PlayerView playerView = new PlayerView(this);
        playerView.setShowSubtitleButton(true);
        content.addView(playerView, new LinearLayout.LayoutParams(
                LinearLayout.LayoutParams.MATCH_PARENT, Math.round(padding * 14f)));
        player = new ExoPlayer.Builder(this).build();
        playerView.setPlayer(player);

        status = new TextView(this);
        player.addListener(new Player.Listener() {
            @Override
            public void onPlaybackStateChanged(int state) {
                if (state == Player.STATE_READY) status.setText("Ready");
                if (state == Player.STATE_BUFFERING) status.setText("Buffering…");
                if (state == Player.STATE_ENDED) status.setText("Playback ended");
            }

            @Override
            public void onPlayerError(PlaybackException error) {
                status.setText("Playback failed (" + error.getErrorCodeName() + ")");
            }
        });
        content.addView(status);

        Button files = new Button(this);
        files.setText("Browse cache files");
        files.setOnClickListener(view -> startActivity(new Intent(this, CacheFilesActivity.class)));
        content.addView(files);
    }

    private void startPlayback() {
        String source = sourceInput.getText().toString().trim();
        Uri uri = Uri.parse(source);
        String scheme = uri.getScheme();
        String host = uri.getHost();
        if (host == null || host.isEmpty()
                || !("http".equalsIgnoreCase(scheme) || "https".equalsIgnoreCase(scheme))) {
            status.setText("Enter a valid HTTP or HTTPS video URL.");
            return;
        }

        Set<String> hosts = new LinkedHashSet<>();
        hosts.add(host);
        for (String entry : additionalHosts.getText().toString().split(",")) {
            String value = entry.trim();
            if (!value.isEmpty()) hosts.add(value);
        }
        String path = uri.getPath();
        boolean hls = forceHls.isChecked()
                || (path != null && path.toLowerCase(Locale.ROOT).endsWith(".m3u8"));
        File cache = new File(getCacheDir(), CACHE_DIRECTORY);
        getSystemService(InputMethodManager.class).hideSoftInputFromWindow(
                sourceInput.getWindowToken(), 0);
        player.stop();
        play.setEnabled(false);
        status.setText("Starting proxy…");
        PROXY.execute(() -> {
            try {
                closeProxy();
                ProxyCache.start(cache, 64L * 1024 * 1024, new ArrayList<>(hosts));
                proxyStarted = true;
                String address = ProxyCache.playbackUrl(source);
                runOnUiThread(() -> {
                    if (isDestroyed()) return;
                    MediaItem.Builder item = new MediaItem.Builder().setUri(address);
                    if (hls) item.setMimeType(MimeTypes.APPLICATION_M3U8);
                    player.setMediaItem(item.build());
                    player.prepare();
                    play.setEnabled(true);
                    player.setPlayWhenReady(started);
                });
            } catch (RuntimeException | LinkageError error) {
                runOnUiThread(() -> {
                    if (isDestroyed()) return;
                    play.setEnabled(true);
                    status.setText("Could not start playback. Check the URL and origin hosts.");
                });
            }
        });
    }

    @Override
    protected void onStart() {
        super.onStart();
        started = true;
    }

    @Override
    protected void onStop() {
        started = false;
        player.pause();
        super.onStop();
    }

    private void closeProxy() {
        if (proxyStarted) {
            ProxyCache.close();
            proxyStarted = false;
        }
    }

    @Override
    protected void onDestroy() {
        player.release();
        PROXY.execute(this::closeProxy);
        super.onDestroy();
    }
}
