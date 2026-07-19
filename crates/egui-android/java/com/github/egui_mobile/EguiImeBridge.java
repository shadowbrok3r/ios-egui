package com.github.egui_mobile;

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
        // commitText moves the caret and fires onSelectionChanged — those S events race ahead of
        // egui applying T and snap the cursor to index 0 / drop further keystrokes.
        activity.suppressSelectionEnqueue = true;
        boolean ret;
        try {
            ret = super.commitText(text, newCursorPosition);
        } finally {
            activity.suppressSelectionEnqueue = false;
        }
        // Empty commit included: it clears an active composition (egui guards the no-op case).
        activity.enqueue("T\t" + (text != null ? text : ""));
        return ret;
    }

    @Override
    public boolean setComposingText(CharSequence text, int newCursorPosition) {
        activity.suppressSelectionEnqueue = true;
        boolean ret;
        try {
            ret = super.setComposingText(text, newCursorPosition);
        } finally {
            activity.suppressSelectionEnqueue = false;
        }
        activity.enqueue("C\t" + (text != null ? text : ""));
        return ret;
    }

    @Override
    public boolean finishComposingText() {
        activity.suppressSelectionEnqueue = true;
        boolean ret;
        try {
            ret = super.finishComposingText();
        } finally {
            activity.suppressSelectionEnqueue = false;
        }
        // "F", not an empty preedit: finishComposingText KEEPS the composing text. Mapping it
        // to Preedit("") made egui delete the just-typed word when Gboard finished on space.
        activity.enqueue("F\t");
        return ret;
    }

    @Override
    public boolean deleteSurroundingText(int beforeLength, int afterLength) {
        activity.suppressSelectionEnqueue = true;
        boolean ret;
        try {
            ret = super.deleteSurroundingText(beforeLength, afterLength);
        } finally {
            activity.suppressSelectionEnqueue = false;
        }
        activity.enqueue("D\t" + beforeLength + "\t" + afterLength);
        return ret;
    }

    // No sendKeyEvent override: IME key events (DEL, DPAD, ENTER) are dispatched into the
    // window and reach egui once via the native input queue (android-activity -> winit).
    // Enqueuing them here as well delivered every backspace/arrow twice.
}
