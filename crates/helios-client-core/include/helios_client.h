#ifndef HELIOS_CLIENT_H
#define HELIOS_CLIENT_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct HeliosClientHandle HeliosClientHandle;

typedef struct HeliosTelemetryC {
    float rtt_ms;
    float packet_loss_pct;
    uint32_t video_kbps;
    float fps;
    bool using_relay;
} HeliosTelemetryC;

typedef void (*HeliosRgbaFrameCallback)(
    void *user_data,
    uint32_t width,
    uint32_t height,
    const uint8_t *rgba_ptr,
    size_t rgba_len,
    HeliosTelemetryC telemetry
);

/**
 * codec: 0 = H264, 1 = H265 (HEVC), 2 = AV1.
 */
typedef void (*HeliosVideoFrameCallback)(
    void *user_data,
    uint32_t width,
    uint32_t height,
    uint8_t codec,
    bool is_keyframe,
    uint64_t timestamp_us,
    const uint8_t *data_ptr,
    size_t data_len,
    HeliosTelemetryC telemetry
);

/**
 * state: 0 = Disconnected, 1 = Connecting, 2 = ConnectedP2P, 3 = ConnectedRelay, 4 = Error.
 */
typedef void (*HeliosStateCallback)(
    void *user_data,
    uint8_t state,
    const char *message
);

/**
 * Invoked when the Windows Host sends a new cursor bitmap (decompressed RGBA bytes)
 * so macOS can render the pointer locally via `NSCursor` with zero latency.
 */
typedef void (*HeliosCursorShapeCallback)(
    void *user_data,
    uint16_t width,
    uint16_t height,
    uint16_t hotspot_x,
    uint16_t hotspot_y,
    const uint8_t *rgba_ptr,
    size_t rgba_len
);

/**
 * Invoked when the Windows Host responds to `helios_client_request_clipboard`.
 */
typedef void (*HeliosClipboardCallback)(
    void *user_data,
    const char *utf8_text
);

char *helios_init_device_id(const char *path, const char *name);

char *helios_client_list_hosts(
    const char *relay_url,
    const char *identity_path,
    const char *client_name,
    const char *pairing_token
);

/**
 * initial_mode: 0 = Auto, 1 = SurvivalText, 2 = LowBandwidth, 3 = Balanced, 4 = Smooth.
 */
HeliosClientHandle *helios_client_connect(
    const char *relay_url,
    const char *identity_path,
    const char *client_name,
    const char *pairing_token,
    const char *access_token,
    const char *target_host_id,
    uint8_t initial_mode,
    void *user_data,
    HeliosRgbaFrameCallback on_rgba_frame,
    HeliosVideoFrameCallback on_video_frame,
    HeliosStateCallback on_state_change,
    HeliosCursorShapeCallback on_cursor_shape,
    HeliosClipboardCallback on_clipboard
);

bool helios_client_send_mouse_abs(HeliosClientHandle *handle, float x, float y);
bool helios_client_send_mouse_button(HeliosClientHandle *handle, uint8_t button, bool pressed);
bool helios_client_send_mouse_wheel(HeliosClientHandle *handle, int32_t dx, int32_t dy);
bool helios_client_send_key(HeliosClientHandle *handle, uint16_t vk_code, bool pressed);
bool helios_client_send_ctrl_alt_del(HeliosClientHandle *handle);
bool helios_client_set_quality_mode(HeliosClientHandle *handle, uint8_t mode);
bool helios_client_request_keyframe(HeliosClientHandle *handle);
bool helios_client_send_clipboard(HeliosClientHandle *handle, const char *utf8_text);
bool helios_client_request_clipboard(HeliosClientHandle *handle);
bool helios_client_switch_monitor(HeliosClientHandle *handle, uint32_t monitor_index);

void helios_client_disconnect(HeliosClientHandle *handle);
void helios_string_free(char *ptr);

#ifdef __cplusplus
}
#endif

#endif /* HELIOS_CLIENT_H */
