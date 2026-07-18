package com.github.egui_mobile;

import android.app.NativeActivity;
import android.content.Context;
import android.text.InputType;
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
    private final ConcurrentLinkedQueue<String> pending = new ConcurrentLinkedQueue<>();

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
                        boolean textChanged = !cur.equals(text);
                        if (textChanged) {
                            edit.setText(text);
                        }
                        CharSequence after = edit.getText();
                        int len = after != null ? after.length() : 0;
                        int s = Math.max(0, Math.min(start, len));
                        int e = Math.max(0, Math.min(end, len));
                        if (edit.getSelectionStart() != s || edit.getSelectionEnd() != e) {
                            edit.setSelection(s, e);
                        }
                        if (textChanged && edit.hasFocus()) {
                            InputMethodManager imm =
                                    (InputMethodManager)
                                            getSystemService(Context.INPUT_METHOD_SERVICE);
                            imm.restartInput(edit);
                        }
                    } finally {
                        updatingFromNative = false;
                    }
                });
    }

    public void showIme() {
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
                    InputMethodManager imm =
                            (InputMethodManager) getSystemService(Context.INPUT_METHOD_SERVICE);
                    // Explicit show (flags=0) so winit's implicit-only hide on the decor view cannot dismiss it.
                    imm.showSoftInput(edit, 0);
                });
    }

    public void hideIme() {
        runOnUiThread(
                () -> {
                    EditText edit = imeEdit;
                    InputMethodManager imm =
                            (InputMethodManager) getSystemService(Context.INPUT_METHOD_SERVICE);
                    if (edit != null) {
                        imm.hideSoftInputFromWindow(edit.getWindowToken(), 0);
                        edit.clearFocus();
                        edit.setVisibility(View.GONE);
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
