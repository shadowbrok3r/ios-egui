package com.github.egui_mobile;

import android.app.NativeActivity;
import android.content.Context;
import android.os.SystemClock;
import android.text.InputType;
import android.view.ActionMode;
import android.view.Menu;
import android.view.MenuItem;
import android.view.View;
import android.view.inputmethod.EditorInfo;
import android.view.inputmethod.InputConnection;
import android.view.inputmethod.InputMethodManager;
import android.widget.EditText;
import android.widget.FrameLayout;
import java.util.ArrayList;
import java.util.concurrent.ConcurrentLinkedQueue;

/** NativeActivity with a hidden EditText so Gboard gets a real InputConnection. */
public class EguiNativeActivity extends NativeActivity {
    private EditText imeEdit;
    private volatile boolean updatingFromNative;
    private volatile boolean softImeRequested;
    private long lastShowUptimeMs;
    private final ConcurrentLinkedQueue<String> pending = new ConcurrentLinkedQueue<>();

    /** Suppress Android's selection/insertion ActionMode — it dismisses the soft keyboard. */
    private static final ActionMode.Callback NO_ACTION_MODE =
            new ActionMode.Callback() {
                @Override
                public boolean onCreateActionMode(ActionMode mode, Menu menu) {
                    return false;
                }

                @Override
                public boolean onPrepareActionMode(ActionMode mode, Menu menu) {
                    return false;
                }

                @Override
                public boolean onActionItemClicked(ActionMode mode, MenuItem item) {
                    return false;
                }

                @Override
                public void onDestroyActionMode(ActionMode mode) {}
            };

    public void ensureImeView() {
        if (imeEdit != null) {
            return;
        }
        final EguiNativeActivity self = this;
        EditText edit = new EditText(this) {
            @Override
            public InputConnection onCreateInputConnection(EditorInfo outAttrs) {
                InputConnection base = super.onCreateInputConnection(outAttrs);
                if (base == null) {
                    return null;
                }
                outAttrs.imeOptions = outAttrs.imeOptions | EditorInfo.IME_FLAG_NO_FULLSCREEN;
                outAttrs.inputType =
                        InputType.TYPE_CLASS_TEXT | InputType.TYPE_TEXT_FLAG_MULTI_LINE;
                return new EguiImeBridge(base, self);
            }

            @Override
            protected void onSelectionChanged(int selStart, int selEnd) {
                super.onSelectionChanged(selStart, selEnd);
                // Backup path: some IMEs move the caret without InputConnection.setSelection.
                if (!updatingFromNative) {
                    enqueue("S\t" + selStart + "\t" + selEnd);
                }
            }
        };
        edit.setBackgroundColor(0);
        edit.setAlpha(0f);
        edit.setFocusable(true);
        edit.setFocusableInTouchMode(true);
        edit.setCursorVisible(false);
        edit.setTextIsSelectable(true);
        // egui draws Paste/Copy/Cut/Select-all; Android's ActionMode closes the IME on Select All.
        edit.setCustomSelectionActionModeCallback(NO_ACTION_MODE);
        edit.setCustomInsertionActionModeCallback(NO_ACTION_MODE);
        // 1×1 on-screen (not off-screen): some IMEs refuse InputConnection for views outside the window.
        FrameLayout.LayoutParams params = new FrameLayout.LayoutParams(1, 1);
        addContentView(edit, params);
        imeEdit = edit;
    }

    public void setImeState(String text, int start, int end) {
        runOnUiThread(
                () -> {
                    ensureImeView();
                    EditText edit = imeEdit;
                    if (edit == null) {
                        return;
                    }
                    updatingFromNative = true;
                    try {
                        CharSequence curCs = edit.getText();
                        String cur = curCs != null ? curCs.toString() : "";
                        if (!cur.equals(text)) {
                            edit.setText(text);
                        }
                        CharSequence after = edit.getText();
                        int len = after != null ? after.length() : 0;
                        int s = Math.max(0, Math.min(start, len));
                        int e = Math.max(0, Math.min(end, len));
                        if (edit.getSelectionStart() != s || edit.getSelectionEnd() != e) {
                            edit.setSelection(s, e);
                        }
                        // Do not restartInput / showSoftInput here — that fights the IME and
                        // causes show/hide flicker when Select All expands the selection.
                    } finally {
                        updatingFromNative = false;
                    }
                });
    }

    /** Bind the hidden EditText without requesting a new IME show animation. */
    public void bindIme() {
        runOnUiThread(
                () -> {
                    ensureImeView();
                    EditText edit = imeEdit;
                    if (edit == null) {
                        return;
                    }
                    edit.setVisibility(View.VISIBLE);
                    if (!edit.hasFocus()) {
                        edit.requestFocus();
                    }
                });
    }

    public void showIme() {
        runOnUiThread(() -> showImeInner(false));
    }

    /** Bypass the show throttle — used when winit hid the IME under the EditText bridge. */
    public void showImeForce() {
        runOnUiThread(() -> showImeInner(true));
    }

    private void showImeInner(boolean force) {
        ensureImeView();
        EditText edit = imeEdit;
        if (edit == null) {
            return;
        }
        edit.setVisibility(View.VISIBLE);
        if (!edit.hasFocus()) {
            edit.requestFocus();
        }
        long now = SystemClock.uptimeMillis();
        // Rising-edge / throttled show — calling showSoftInput every frame cancels
        // the IME animation and flickers the keyboard (see logcat ImeTracker).
        if (!force && softImeRequested && now - lastShowUptimeMs < 400) {
            return;
        }
        softImeRequested = true;
        lastShowUptimeMs = now;
        InputMethodManager imm =
                (InputMethodManager) getSystemService(Context.INPUT_METHOD_SERVICE);
        imm.showSoftInput(edit, 0);
    }

    public void hideIme() {
        runOnUiThread(
                () -> {
                    EditText edit = imeEdit;
                    InputMethodManager imm =
                            (InputMethodManager) getSystemService(Context.INPUT_METHOD_SERVICE);
                    softImeRequested = false;
                    lastShowUptimeMs = 0;
                    if (edit != null) {
                        imm.hideSoftInputFromWindow(edit.getWindowToken(), 0);
                        // Keep the view attached and focusable so the next showIme is reliable.
                        // GONE + clearFocus drops the InputConnection and lets the DecorView steal
                        // IME service, after which showSoftInput on the EditText is ignored.
                    }
                });
    }

    public String[] takePending() {
        ArrayList<String> out = new ArrayList<>();
        while (true) {
            String e = pending.poll();
            if (e == null) {
                break;
            }
            out.add(e);
        }
        return out.toArray(new String[0]);
    }

    void enqueue(String event) {
        if (!updatingFromNative) {
            pending.offer(event);
        }
    }
}
