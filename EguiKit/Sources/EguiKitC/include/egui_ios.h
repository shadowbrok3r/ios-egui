// egui-ios C ABI — version 2. APPEND-ONLY: never reorder or change existing signatures.
// v2: adds egui_ios_set_keyboard_height.
// Implemented by the Rust staticlib via the `egui_ios::app!` macro. All calls are main-thread
// only. Coordinates are in points; sizes are in pixels. Returned `char*` must be released with
// egui_ios_string_free.
#pragma once

#include <stdint.h>
#include <stdbool.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef void *EguiHandle;

// Request kinds returned by egui_ios_poll_request via out_kind.
enum {
    EGUI_REQ_SHARE_FILE = 0,
    EGUI_REQ_NOTIFY = 1,
    EGUI_REQ_SET_KEYBOARD = 2,
    EGUI_REQ_HAPTIC = 3,
    EGUI_REQ_OPEN_URL = 4,
    EGUI_REQ_PICK_FILE = 5,
    EGUI_REQ_REQUEST_CAMERA_PERMISSION = 6,
    EGUI_REQ_REQUEST_MIC_PERMISSION = 7,
    EGUI_REQ_START_CAMERA_PREVIEW = 8,
    EGUI_REQ_STOP_CAMERA_PREVIEW = 9,
    EGUI_REQ_SET_CLIPBOARD = 10,
};

// Permission kinds for egui_ios_on_permission_result.
enum {
    EGUI_PERMISSION_CAMERA = 0,
    EGUI_PERMISSION_MICROPHONE = 1,
};

// ── Version ──────────────────────────────────────────────────────────────
uint32_t egui_ios_abi_version(void);

// ── Lifecycle ────────────────────────────────────────────────────────────
EguiHandle egui_ios_new(void *ca_metal_layer, uint32_t w_px, uint32_t h_px, float ppp);
void egui_ios_free(EguiHandle h);
void egui_ios_resize(EguiHandle h, uint32_t w_px, uint32_t h_px);
void egui_ios_set_pixels_per_point(EguiHandle h, float ppp);
void egui_ios_render(EguiHandle h, double time_seconds);

// ── Touch (points) ───────────────────────────────────────────────────────
void egui_ios_touch_began(EguiHandle h, float x_pt, float y_pt);
void egui_ios_touch_moved(EguiHandle h, float x_pt, float y_pt);
void egui_ios_touch_ended(EguiHandle h, float x_pt, float y_pt);
void egui_ios_touch_cancelled(EguiHandle h, float x_pt, float y_pt);

// ── Keyboard (soft + hardware) ───────────────────────────────────────────
bool egui_ios_wants_keyboard(EguiHandle h);
void egui_ios_insert_text(EguiHandle h, const char *utf8);
void egui_ios_delete_backward(EguiHandle h);
void egui_ios_key_event(EguiHandle h, int32_t hid_key_code, int32_t modifier_flags, bool pressed);

// ── Pointer / trackpad ───────────────────────────────────────────────────
void egui_ios_scroll(EguiHandle h, float dx_pt, float dy_pt);
void egui_ios_pointer_moved(EguiHandle h, float x_pt, float y_pt);
void egui_ios_pointer_gone(EguiHandle h);

// ── Environment (Swift → Rust) ───────────────────────────────────────────
void egui_ios_set_documents_dir(EguiHandle h, const char *path);
void egui_ios_set_safe_area_insets(EguiHandle h, float top, float bottom, float left, float right);
// Height in points of the soft keyboard overlapping the view (0 when hidden).
void egui_ios_set_keyboard_height(EguiHandle h, float pts);
void egui_ios_set_app_active(EguiHandle h, bool active);

// ── Capability requests (app → Swift, poll) ──────────────────────────────
bool egui_ios_poll_request(EguiHandle h, int32_t *out_kind);
char *egui_ios_request_str_a(EguiHandle h);
char *egui_ios_request_str_b(EguiHandle h);
int32_t egui_ios_request_int(EguiHandle h);

// ── Capability results (Swift → Rust) ────────────────────────────────────
void egui_ios_on_file_picked(EguiHandle h, const char *path);
void egui_ios_on_permission_result(EguiHandle h, int32_t kind, bool granted);
void egui_ios_on_mic_level(EguiHandle h, float level);

// ── Sync callbacks ───────────────────────────────────────────────────────
void egui_ios_register_haptic_cb(EguiHandle h, void (*cb)(int32_t));

// ── Strings ──────────────────────────────────────────────────────────────
void egui_ios_string_free(char *s);

// ── Diagnostics ──────────────────────────────────────────────────────────
// Last init/panic message (or NULL). Caller frees with egui_ios_string_free.
char *egui_ios_last_error(void);

#ifdef __cplusplus
}
#endif
