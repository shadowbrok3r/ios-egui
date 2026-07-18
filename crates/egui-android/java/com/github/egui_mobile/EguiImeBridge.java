package com.github.egui_mobile;

import android.view.KeyEvent;
import android.view.inputmethod.InputConnection;
import android.view.inputmethod.InputConnectionWrapper;

/** Forwards IME InputConnection calls into a queue drained by Rust. */
public class EguiImeBridge extends InputConnectionWrapper {
    private final EguiNativeActivity activity;

    public EguiImeBridge(InputConnection target, EguiNativeActivity activity) {
        super(target, true);
        this.activity = activity;
    }

    @Override
    public boolean setSelection(int start, int end) {
        // Selection is forwarded via EditText.onSelectionChanged (covers IC + other IME paths).
        return super.setSelection(start, end);
    }

    @Override
    public boolean commitText(CharSequence text, int newCursorPosition) {
        boolean ret = super.commitText(text, newCursorPosition);
        if (text != null && text.length() > 0) {
            activity.enqueue("T\t" + text);
        }
        return ret;
    }

    @Override
    public boolean setComposingText(CharSequence text, int newCursorPosition) {
        boolean ret = super.setComposingText(text, newCursorPosition);
        activity.enqueue("C\t" + (text != null ? text : ""));
        return ret;
    }

    @Override
    public boolean finishComposingText() {
        activity.enqueue("C\t");
        return super.finishComposingText();
    }

    @Override
    public boolean deleteSurroundingText(int beforeLength, int afterLength) {
        boolean ret = super.deleteSurroundingText(beforeLength, afterLength);
        activity.enqueue("D\t" + beforeLength + "\t" + afterLength);
        return ret;
    }

    @Override
    public boolean sendKeyEvent(KeyEvent event) {
        if (event.getAction() == KeyEvent.ACTION_DOWN) {
            int code = event.getKeyCode();
            switch (code) {
                case KeyEvent.KEYCODE_DPAD_LEFT:
                case KeyEvent.KEYCODE_DPAD_RIGHT:
                case KeyEvent.KEYCODE_DPAD_UP:
                case KeyEvent.KEYCODE_DPAD_DOWN:
                case KeyEvent.KEYCODE_DEL:
                case KeyEvent.KEYCODE_FORWARD_DEL:
                    activity.enqueue("K\t" + code);
                    break;
                default:
                    break;
            }
        }
        return super.sendKeyEvent(event);
    }
}
