package com.github.egui_mobile;

import android.text.Editable;
import android.util.Log;
import android.view.KeyEvent;
import android.view.inputmethod.BaseInputConnection;
import android.view.inputmethod.CompletionInfo;
import android.view.inputmethod.CorrectionInfo;
import android.view.inputmethod.InputConnection;
import android.view.inputmethod.InputConnectionWrapper;
import android.view.inputmethod.SurroundingText;
import android.view.inputmethod.TextAttribute;

/** Forwards IME InputConnection calls into a queue drained by Rust.
 *
 * Every text-mutating entry point is overridden, including the API 33+ TextAttribute overloads —
 * InputConnectionWrapper forwards those straight to the target, so leaving them unoverridden
 * silently drops the op for Rust (Samsung Keyboard commits through them).
 *
 * All indices crossing to Rust are code-point (char) offsets, converted from UTF-16 here. */
public class EguiImeBridge extends InputConnectionWrapper {
    static final boolean TRACE = true;
    private final EguiNativeActivity activity;

    public EguiImeBridge(InputConnection target, EguiNativeActivity activity) {
        super(target, true);
        this.activity = activity;
    }

    private void trace(String op) {
        if (TRACE) Log.i("EguiIme", "IC " + op + " => " + activity.imeStateDump());
    }

    private static String clip(CharSequence s) {
        if (s == null) return "<null>";
        String t = s.toString();
        return t.length() > 60 ? t.substring(0, 30) + "…" + t.substring(t.length() - 25) : t;
    }

    // ---- committed / composing text ----

    private boolean commitInner(CharSequence text, int newCursorPosition, TextAttribute attr) {
        // commitText moves the caret and fires onSelectionChanged — those S events race ahead of
        // egui applying T and snap the cursor to index 0 / drop further keystrokes.
        activity.suppressSelectionEnqueue = true;
        boolean ret;
        try {
            ret = attr != null
                    ? super.commitText(text, newCursorPosition, attr)
                    : super.commitText(text, newCursorPosition);
        } finally {
            activity.suppressSelectionEnqueue = false;
        }
        // Empty commit included: it clears an active composition (egui guards the no-op case).
        activity.enqueue("T\t" + (text != null ? text : ""));
        trace("commitText(\"" + clip(text) + "\", " + newCursorPosition + (attr != null ? ", attr" : "") + ")");
        return ret;
    }

    @Override
    public boolean commitText(CharSequence text, int newCursorPosition) {
        return commitInner(text, newCursorPosition, null);
    }

    @Override
    public boolean commitText(CharSequence text, int newCursorPosition, TextAttribute textAttribute) {
        return commitInner(text, newCursorPosition, textAttribute);
    }

    private boolean composeInner(CharSequence text, int newCursorPosition, TextAttribute attr) {
        activity.suppressSelectionEnqueue = true;
        boolean ret;
        try {
            ret = attr != null
                    ? super.setComposingText(text, newCursorPosition, attr)
                    : super.setComposingText(text, newCursorPosition);
        } finally {
            activity.suppressSelectionEnqueue = false;
        }
        activity.enqueue("C\t" + (text != null ? text : ""));
        trace("setComposingText(\"" + clip(text) + "\", " + newCursorPosition + (attr != null ? ", attr" : "") + ")");
        return ret;
    }

    @Override
    public boolean setComposingText(CharSequence text, int newCursorPosition) {
        return composeInner(text, newCursorPosition, null);
    }

    @Override
    public boolean setComposingText(CharSequence text, int newCursorPosition, TextAttribute textAttribute) {
        return composeInner(text, newCursorPosition, textAttribute);
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
        trace("finishComposingText()");
        return ret;
    }

    // ---- composing region / replace ----

    private boolean composeRegionInner(int start, int end, TextAttribute attr) {
        activity.suppressSelectionEnqueue = true;
        boolean ret;
        try {
            ret = attr != null
                    ? super.setComposingRegion(start, end, attr)
                    : super.setComposingRegion(start, end);
        } finally {
            activity.suppressSelectionEnqueue = false;
        }
        // Read back the applied span (EditText clamps) and ship its text so Rust can rebuild the
        // composition. Cleared/invalid span maps to F (keep text, end composition).
        Editable ed = activity.imeEditable();
        int cs = ed != null ? BaseInputConnection.getComposingSpanStart(ed) : -1;
        int ce = ed != null ? BaseInputConnection.getComposingSpanEnd(ed) : -1;
        if (ed != null && cs >= 0 && ce >= cs) {
            String regionText = ed.subSequence(cs, ce).toString();
            int cpStart = Character.codePointCount(ed, 0, cs);
            int cpEnd = cpStart + Character.codePointCount(ed, cs, ce);
            activity.enqueue("R\t" + cpStart + "\t" + cpEnd + "\t" + regionText);
        } else {
            activity.enqueue("F\t");
        }
        trace("setComposingRegion(" + start + ", " + end + (attr != null ? ", attr" : "") + ")");
        return ret;
    }

    @Override
    public boolean setComposingRegion(int start, int end) {
        return composeRegionInner(start, end, null);
    }

    @Override
    public boolean setComposingRegion(int start, int end, TextAttribute textAttribute) {
        return composeRegionInner(start, end, textAttribute);
    }

    @Override
    public boolean replaceText(int start, int end, CharSequence text, int newCursorPosition, TextAttribute textAttribute) {
        Editable ed = activity.imeEditable();
        int cpStart = -1;
        int cpEnd = -1;
        if (ed != null) {
            int len = ed.length();
            int s = Math.max(0, Math.min(start, len));
            int e = Math.max(s, Math.min(end, len));
            cpStart = Character.codePointCount(ed, 0, s);
            cpEnd = cpStart + Character.codePointCount(ed, s, e);
        }
        activity.suppressSelectionEnqueue = true;
        boolean ret;
        try {
            ret = super.replaceText(start, end, text, newCursorPosition, textAttribute);
        } finally {
            activity.suppressSelectionEnqueue = false;
        }
        if (cpStart >= 0) {
            activity.enqueue("X\t" + cpStart + "\t" + cpEnd + "\t" + (text != null ? text : ""));
        }
        trace("replaceText(" + start + ".." + end + ", \"" + clip(text) + "\", " + newCursorPosition + ")");
        return ret;
    }

    // ---- deletion ----

    /** Code points deleted by deleteSurroundingText(beforeU16, afterU16) around the union of
     * selection and composing span (AOSP semantics), as {beforeCp, afterCp}. */
    private int[] deleteSpansCp(int beforeLength, int afterLength) {
        Editable ed = activity.imeEditable();
        if (ed == null) return new int[] {Math.max(0, beforeLength), Math.max(0, afterLength)};
        int a = activity.imeSelStart();
        int b = activity.imeSelEnd();
        if (a > b) { int t = a; a = b; b = t; }
        int ca = BaseInputConnection.getComposingSpanStart(ed);
        int ce = BaseInputConnection.getComposingSpanEnd(ed);
        if (ca >= 0 && ce >= 0) {
            if (ce < ca) { int t = ca; ca = ce; ce = t; }
            if (ca < a) a = ca;
            if (ce > b) b = ce;
        }
        int beforeStart = Math.max(0, a - Math.max(0, beforeLength));
        int afterEnd = Math.min(ed.length(), b + Math.max(0, afterLength));
        return new int[] {
            Character.codePointCount(ed, beforeStart, a),
            Character.codePointCount(ed, b, afterEnd),
        };
    }

    @Override
    public boolean deleteSurroundingText(int beforeLength, int afterLength) {
        int[] cp = deleteSpansCp(beforeLength, afterLength);
        activity.suppressSelectionEnqueue = true;
        boolean ret;
        try {
            ret = super.deleteSurroundingText(beforeLength, afterLength);
        } finally {
            activity.suppressSelectionEnqueue = false;
        }
        activity.enqueue("D\t" + cp[0] + "\t" + cp[1]);
        trace("deleteSurroundingText(" + beforeLength + ", " + afterLength + ") cp=" + cp[0] + "," + cp[1]);
        return ret;
    }

    @Override
    public boolean deleteSurroundingTextInCodePoints(int beforeLength, int afterLength) {
        activity.suppressSelectionEnqueue = true;
        boolean ret;
        try {
            ret = super.deleteSurroundingTextInCodePoints(beforeLength, afterLength);
        } finally {
            activity.suppressSelectionEnqueue = false;
        }
        activity.enqueue("D\t" + Math.max(0, beforeLength) + "\t" + Math.max(0, afterLength));
        trace("deleteSurroundingTextInCodePoints(" + beforeLength + ", " + afterLength + ")");
        return ret;
    }

    // ---- batching / selection / misc (trace + batch bookkeeping) ----

    @Override
    public boolean beginBatchEdit() {
        activity.imeBatchBegin();
        boolean ret = super.beginBatchEdit();
        if (TRACE) Log.i("EguiIme", "IC beginBatchEdit");
        return ret;
    }

    @Override
    public boolean endBatchEdit() {
        boolean ret = super.endBatchEdit();
        activity.imeBatchEnd();
        if (TRACE) Log.i("EguiIme", "IC endBatchEdit => " + activity.imeStateDump());
        return ret;
    }

    @Override
    public boolean setSelection(int start, int end) {
        // Selection is forwarded via EditText.onSelectionChanged (covers IC + other IME paths).
        boolean ret = super.setSelection(start, end);
        trace("setSelection(" + start + ", " + end + ")");
        return ret;
    }

    @Override
    public boolean sendKeyEvent(KeyEvent event) {
        // DEL/FORWARD_DEL: mirror into the hidden Editable and ride the IC queue instead of the
        // native window dispatch — the dispatched key lands frames later and reorders against
        // commitText/setComposingText, which is how a backspace hold types behind the caret.
        // Other keys (DPAD, ENTER) keep the native path and reach egui once via the input queue.
        if (event != null) {
            int code = event.getKeyCode();
            if (code == KeyEvent.KEYCODE_DEL || code == KeyEvent.KEYCODE_FORWARD_DEL) {
                if (event.getAction() == KeyEvent.ACTION_DOWN) {
                    activity.mirrorDeleteKey(code == KeyEvent.KEYCODE_DEL);
                    activity.enqueue("K\t" + code);
                    if (TRACE) trace("sendKeyEvent(" + KeyEvent.keyCodeToString(code) + ") queued");
                }
                return true;
            }
        }
        boolean ret = super.sendKeyEvent(event);
        if (TRACE && event != null && event.getAction() == KeyEvent.ACTION_DOWN) {
            trace("sendKeyEvent(" + KeyEvent.keyCodeToString(event.getKeyCode()) + ")");
        }
        return ret;
    }

    @Override
    public boolean performEditorAction(int editorAction) {
        boolean ret = super.performEditorAction(editorAction);
        trace("performEditorAction(" + editorAction + ")");
        return ret;
    }

    @Override
    public boolean performContextMenuAction(int id) {
        boolean ret = super.performContextMenuAction(id);
        trace("performContextMenuAction(" + id + ")");
        return ret;
    }

    @Override
    public boolean commitCompletion(CompletionInfo text) {
        boolean ret = super.commitCompletion(text);
        trace("commitCompletion(" + (text != null ? clip(text.getText()) : "<null>") + ")");
        return ret;
    }

    @Override
    public boolean commitCorrection(CorrectionInfo correctionInfo) {
        boolean ret = super.commitCorrection(correctionInfo);
        trace("commitCorrection(" + correctionInfo + ")");
        return ret;
    }

    @Override
    public CharSequence getTextBeforeCursor(int n, int flags) {
        CharSequence ret = super.getTextBeforeCursor(n, flags);
        if (TRACE) Log.i("EguiIme", "IC getTextBeforeCursor(" + n + ") -> \"" + clip(ret) + "\"");
        return ret;
    }

    @Override
    public CharSequence getTextAfterCursor(int n, int flags) {
        CharSequence ret = super.getTextAfterCursor(n, flags);
        if (TRACE) Log.i("EguiIme", "IC getTextAfterCursor(" + n + ") -> \"" + clip(ret) + "\"");
        return ret;
    }

    @Override
    public CharSequence getSelectedText(int flags) {
        CharSequence ret = super.getSelectedText(flags);
        if (TRACE) Log.i("EguiIme", "IC getSelectedText -> \"" + clip(ret) + "\"");
        return ret;
    }

    @Override
    public SurroundingText getSurroundingText(int beforeLength, int afterLength, int flags) {
        SurroundingText ret = super.getSurroundingText(beforeLength, afterLength, flags);
        if (TRACE) {
            Log.i("EguiIme", "IC getSurroundingText(" + beforeLength + ", " + afterLength + ") -> "
                    + (ret != null ? "\"" + clip(ret.getText()) + "\" sel=" + ret.getSelectionStart() + ".." + ret.getSelectionEnd() : "<null>"));
        }
        return ret;
    }

    @Override
    public boolean requestCursorUpdates(int cursorUpdateMode) {
        boolean ret = super.requestCursorUpdates(cursorUpdateMode);
        if (TRACE) Log.i("EguiIme", "IC requestCursorUpdates(" + cursorUpdateMode + ")");
        return ret;
    }
}
