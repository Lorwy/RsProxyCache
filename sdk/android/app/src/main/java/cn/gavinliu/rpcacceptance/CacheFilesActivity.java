package cn.gavinliu.rpcacceptance;

import android.app.Activity;
import android.os.Bundle;
import android.text.format.Formatter;
import android.widget.ArrayAdapter;
import android.widget.Button;
import android.widget.LinearLayout;
import android.widget.ListView;
import android.widget.TextView;
import java.io.File;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.Comparator;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;

public final class CacheFilesActivity extends Activity {
    private final ExecutorService scanner = Executors.newSingleThreadExecutor();
    private File root;
    private File directory;
    private TextView path;
    private Button up;
    private ListView list;
    private int request;

    @Override
    protected void onCreate(Bundle state) {
        super.onCreate(state);
        root = new File(getCacheDir(), SampleActivity.CACHE_DIRECTORY);
        LinearLayout content = new LinearLayout(this);
        content.setFitsSystemWindows(true);
        content.setOrientation(LinearLayout.VERTICAL);
        int padding = Math.round(16 * getResources().getDisplayMetrics().density);
        content.setPadding(padding, padding, padding, padding);

        TextView title = new TextView(this);
        title.setText("Cache files");
        title.setTextSize(22);
        content.addView(title);
        path = new TextView(this);
        content.addView(path);
        up = new Button(this);
        up.setText("Up");
        up.setOnClickListener(view -> show(directory.getParentFile()));
        content.addView(up);
        Button refresh = new Button(this);
        refresh.setText("Refresh");
        refresh.setOnClickListener(view -> show(directory));
        content.addView(refresh);
        list = new ListView(this);
        content.addView(list, new LinearLayout.LayoutParams(
                LinearLayout.LayoutParams.MATCH_PARENT, 0, 1));
        setContentView(content);
        show(root);
    }

    private void show(File folder) {
        directory = folder;
        path.setText(folder.getAbsolutePath());
        up.setEnabled(!folder.equals(root));
        int current = ++request;
        list.setAdapter(null);
        list.setOnItemClickListener(null);
        scanner.execute(() -> {
            File[] entries = folder.listFiles();
            ArrayList<String> rows = new ArrayList<>();
            ArrayList<File> folders = new ArrayList<>();
            if (entries != null) {
                Arrays.sort(entries, Comparator.comparing(File::getName));
                for (File entry : entries) {
                    boolean isDirectory = entry.isDirectory();
                    folders.add(isDirectory ? entry : null);
                    rows.add(isDirectory ? entry.getName() + "/" : entry.getName() + "\n"
                            + Formatter.formatFileSize(this, entry.length()));
                }
            }
            if (rows.isEmpty()) {
                rows.add(entries == null && folder.exists()
                        ? "Unable to read this directory" : "No cache files yet");
            }
            runOnUiThread(() -> {
                if (isDestroyed() || current != request) return;
                list.setAdapter(new ArrayAdapter<>(this, android.R.layout.simple_list_item_1, rows));
                list.setOnItemClickListener((parent, view, position, id) -> {
                    if (position >= folders.size()) return;
                    File child = folders.get(position);
                    if (child != null) show(child);
                });
            });
        });
    }

    @Override
    protected void onDestroy() {
        scanner.shutdownNow();
        super.onDestroy();
    }
}
