package com.github.egui_mobile;

import android.app.NativeActivity;
import android.content.Context;
import android.content.pm.ActivityInfo;
import android.content.pm.PackageManager;
import android.os.Bundle;
import android.os.SystemClock;
import android.text.Editable;
import android.text.InputType;
import android.util.Log;
import android.view.ActionMode;
import android.view.KeyEvent;
import android.view.Menu;
import android.view.MenuItem;
import android.view.View;
import android.view.WindowInsets;
import android.view.inputmethod.BaseInputConnection;
import android.view.inputmethod.EditorInfo;
import android.view.inputmethod.InputConnection;
import android.view.inputmethod.InputMethodManager;
import android.widget.EditText;
import android.widget.FrameLayout;
import java.util.ArrayList;
import java.util.concurrent.ConcurrentLinkedQueue;

/** NativeActivity with a hidden EditText so the IME gets a real InputConnection. */
public class EguiNativeActivity extends NativeActivity {
    static final boolean TRACE = EguiImeBridge.TRACE;
    private EditText imeEdit;
    private volatile boolean updatingFromNative;
    /** True while commitText/delete/etc. so caret moves do not enqueue racing S events. */
    volatile boolean suppressSelectionEnqueue;
    private volatile boolean softImeRequested;
    private long lastShowUptimeMs;
    private final ConcurrentLinkedQueue<String> pending = new ConcurrentLinkedQueue<>();
    /** InputConnection batch depth; selection enqueues are deferred until the batch closes. */
    private int batchDepth;
    private boolean batchSawSelChange;
    /** The keyboard went away without the app asking (back button/gesture). */
    private volatile boolean imeDismissed;
    private boolean imeInsetVisible;

    @Override
    protected void onCreate(Bundle savedInstanceState) {
        // NativeActivity dlopens the native lib directly, so ART never registers it and
        // nativeImeWake fails to resolve ("No implementation found"). loadLibrary registers
        // it with ART first; NativeActivity's own load then reuses the same handle.
        try {
            ActivityInfo ai =
                    getPackageManager()
                            .getActivityInfo(getComponentName(), PackageManager.GET_META_DATA);
            String libname = ai.metaData != null ? ai.metaData.getString("android.app.lib_name") : null;
            System.loadLibrary(libname != null ? libname : "main");
        } catch (Throwable t) {
            // nativeImeWake stays unresolved; Rust falls back to polling while the IME is up.
        }
        super.onCreate(savedInstanceState);
    }

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
                if (TRACE) Log.i("EguiIme", "onCreateInputConnection");
                return new EguiImeBridge(base, self);
            }

            @Override
            public boolean onKeyPreIme(int keyCode, KeyEvent event) {
                // Back with the keyboard up = dismiss. Predictive back (API 33+) routes this
                // through the IME's own OnBackInvokedCallback instead, so the insets listener
                // below is the primary signal and this is the pre-33 path.
                if (keyCode == KeyEvent.KEYCODE_BACK
                        && event != null
                        && event.getAction() == KeyEvent.ACTION_UP) {
                    noteImeDismissed();
                }
                return super.onKeyPreIme(keyCode, event);
            }

            @Override
            protected void onSelectionChanged(int selStart, int selEnd) {
                super.onSelectionChanged(selStart, selEnd);
                // Trackpad / explicit setSelection only — not caret churn from commitText.
                if (!updatingFromNative && !suppressSelectionEnqueue) {
                    if (batchDepth > 0) {
                        batchSawSelChange = true;
                    } else {
                        enqueueSelection();
                    }
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
        // The IME can go away with no input event the app can see (back button/gesture, IME's own
        // dismiss key). Watch the ime inset for a visible→hidden edge and report it as dismissal.
        if (android.os.Build.VERSION.SDK_INT >= android.os.Build.VERSION_CODES.R) {
            edit.setOnApplyWindowInsetsListener(
                    (v, insets) -> {
                        boolean visible = insets.isVisible(WindowInsets.Type.ime());
                        if (imeInsetVisible && !visible) {
                            noteImeDismissed();
                        }
                        imeInsetVisible = visible;
                        return v.onApplyWindowInsets(insets);
                    });
        }
        imeEdit = edit;
    }

    /** Latch a keyboard dismissal the app never asked for; Rust drains it and drops focus. */
    void noteImeDismissed() {
        imeDismissed = true;
        softImeRequested = false;
        lastShowUptimeMs = 0;
        if (TRACE) Log.i("EguiIme", "ime dismissed externally");
        if (!nativeWakeBroken) {
            try {
                nativeImeWake();
            } catch (Throwable t) {
                nativeWakeBroken = true;
            }
        }
    }

    /** Read and clear the external-dismissal latch. */
    public boolean takeImeDismissed() {
        boolean was = imeDismissed;
        imeDismissed = false;
        return was;
    }

    /** Current selection as an S event with code-point offsets. */
    private void enqueueSelection() {
        EditText edit = imeEdit;
        Editable ed = edit != null ? edit.getText() : null;
        if (ed == null) {
            return;
        }
        int s = Math.max(0, edit.getSelectionStart());
        int e = Math.max(0, edit.getSelectionEnd());
        enqueue("S\t" + Character.codePointCount(ed, 0, Math.min(s, ed.length()))
                + "\t" + Character.codePointCount(ed, 0, Math.min(e, ed.length())));
    }

    void imeBatchBegin() {
        batchDepth++;
    }

    void imeBatchEnd() {
        if (batchDepth > 0) {
            batchDepth--;
        }
        // Selection settled by the batch (e.g. trackpad moves inside begin/endBatchEdit);
        // mutating ops inside the batch make Rust drop it anyway.
        if (batchDepth == 0 && batchSawSelChange) {
            batchSawSelChange = false;
            if (!updatingFromNative && !suppressSelectionEnqueue) {
                enqueueSelection();
            }
        }
    }

    Editable imeEditable() {
        EditText edit = imeEdit;
        return edit != null ? edit.getText() : null;
    }

    /** Apply DEL/FORWARD_DEL to the hidden Editable: composing span, else selection, else one
     * code point — the same range egui deletes for the key from the native queue. */
    void mirrorDeleteKey(boolean backspace) {
        EditText edit = imeEdit;
        Editable ed = edit != null ? edit.getText() : null;
        if (ed == null) {
            return;
        }
        suppressSelectionEnqueue = true;
        try {
            int a = Math.max(0, edit.getSelectionStart());
            int b = Math.max(0, edit.getSelectionEnd());
            if (a > b) {
                int t = a;
                a = b;
                b = t;
            }
            int ca = BaseInputConnection.getComposingSpanStart(ed);
            int cb = BaseInputConnection.getComposingSpanEnd(ed);
            if (ca >= 0 && cb >= ca) {
                a = ca;
                b = cb;
            }
            if (a != b) {
                ed.delete(a, b);
            } else if (backspace && a > 0) {
                ed.delete(Character.offsetByCodePoints(ed, a, -1), a);
            } else if (!backspace && b < ed.length()) {
                ed.delete(b, Character.offsetByCodePoints(ed, b, 1));
            }
        } finally {
            suppressSelectionEnqueue = false;
        }
    }

    int imeSelStart() {
        EditText edit = imeEdit;
        return edit != null ? Math.max(0, edit.getSelectionStart()) : 0;
    }

    int imeSelEnd() {
        EditText edit = imeEdit;
        return edit != null ? Math.max(0, edit.getSelectionEnd()) : 0;
    }

    /** Compact EditText state for trace logs: text, selection, composing span (UTF-16 offsets). */
    String imeStateDump() {
        EditText edit = imeEdit;
        Editable ed = edit != null ? edit.getText() : null;
        if (edit == null || ed == null) {
            return "<no edit>";
        }
        String t = ed.toString();
        if (t.length() > 80) {
            t = t.substring(0, 40) + "…" + t.substring(t.length() - 35);
        }
        return "\"" + t + "\" sel=" + edit.getSelectionStart() + ".." + edit.getSelectionEnd()
                + " comp=" + BaseInputConnection.getComposingSpanStart(ed)
                + ".." + BaseInputConnection.getComposingSpanEnd(ed);
    }

    /** Clamped UTF-16 offset for a code-point offset into `ed`. */
    private static int cpToUtf16(Editable ed, int cp) {
        int len = ed.length();
        int cpLen = Character.codePointCount(ed, 0, len);
        if (cp <= 0) {
            return 0;
        }
        if (cp >= cpLen) {
            return len;
        }
        return Character.offsetByCodePoints(ed, 0, cp);
    }

    /** Replace EditText contents/selection from egui; start/end are code-point offsets. */
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
                        Editable after = edit.getText();
                        if (after == null) {
                            return;
                        }
                        int s = cpToUtf16(after, start);
                        int e = cpToUtf16(after, end);
                        if (edit.getSelectionStart() != s || edit.getSelectionEnd() != e) {
                            edit.setSelection(s, e);
                        }
                        if (TRACE) Log.i("EguiIme", "setImeState => " + imeStateDump());
                        // Do not restartInput / showSoftInput here — that fights the IME and
                        // causes show/hide flicker when Select All expands the selection.
                    } finally {
                        updatingFromNative = false;
                    }
                });
    }

    /** Move only the EditText caret (code-point offsets); optionally end composition first. */
    public void setImeSelection(int start, int end, boolean clearComposing) {
        runOnUiThread(
                () -> {
                    EditText edit = imeEdit;
                    Editable ed = edit != null ? edit.getText() : null;
                    if (edit == null || ed == null) {
                        return;
                    }
                    updatingFromNative = true;
                    try {
                        if (clearComposing) {
                            edit.clearComposingText();
                        }
                        int s = cpToUtf16(ed, start);
                        int e = cpToUtf16(ed, end);
                        if (edit.getSelectionStart() != s || edit.getSelectionEnd() != e) {
                            edit.setSelection(s, e);
                        }
                        if (TRACE) Log.i("EguiIme", "setImeSelection => " + imeStateDump());
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
        // A stale latch from before this show would immediately tear the new session down.
        imeDismissed = false;
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
                    // Our own hide must not latch as an external dismissal when the ime inset
                    // edge lands a few frames later.
                    imeDismissed = false;
                    imeInsetVisible = false;
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
            if (!nativeWakeBroken) {
                try {
                    nativeImeWake();
                } catch (Throwable t) {
                    // Older native lib without the export — Rust falls back to polling.
                    nativeWakeBroken = true;
                    if (TRACE) Log.i("EguiIme", "nativeImeWake unavailable: " + t);
                }
            }
        }
    }

    /** Wakes the sleeping render loop so a queued IME event is applied this frame, not on the
     * next unrelated touch/key. Implemented in Rust (egui-android ime_bridge). */
    private static native void nativeImeWake();

    private static volatile boolean nativeWakeBroken;
}
