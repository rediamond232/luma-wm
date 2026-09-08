#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <xcb/xcb.h>

static xcb_atom_t atom(xcb_connection_t *connection, const char *name) {
    xcb_intern_atom_cookie_t cookie =
        xcb_intern_atom(connection, 0, (uint16_t)strlen(name), name);
    xcb_intern_atom_reply_t *reply = xcb_intern_atom_reply(connection, cookie, NULL);
    if (reply == NULL) {
        return XCB_ATOM_NONE;
    }
    xcb_atom_t value = reply->atom;
    free(reply);
    return value;
}

int main(void) {
    int screen_number = 0;
    xcb_connection_t *connection = xcb_connect(NULL, &screen_number);
    if (xcb_connection_has_error(connection)) {
        fputs("could not connect to X11 display\n", stderr);
        return 1;
    }
    const xcb_setup_t *setup = xcb_get_setup(connection);
    xcb_screen_iterator_t screens = xcb_setup_roots_iterator(setup);
    for (int index = 0; index < screen_number; index++) {
        xcb_screen_next(&screens);
    }
    xcb_screen_t *screen = screens.data;
    if (screen == NULL) {
        xcb_disconnect(connection);
        return 1;
    }

    xcb_window_t window = xcb_generate_id(connection);
    uint32_t values[] = {0x00d04040, XCB_EVENT_MASK_EXPOSURE | XCB_EVENT_MASK_STRUCTURE_NOTIFY};
    xcb_create_window(connection, XCB_COPY_FROM_PARENT, window, screen->root,
                      0, 0, 360, 220, 0, XCB_WINDOW_CLASS_INPUT_OUTPUT,
                      screen->root_visual, XCB_CW_BACK_PIXEL | XCB_CW_EVENT_MASK, values);

    static const char wm_class[] = "wm-x11-close\0org.customwm.X11Close\0";
    static const char title[] = "WM X11 closing fixture";
    xcb_change_property(connection, XCB_PROP_MODE_REPLACE, window, XCB_ATOM_WM_CLASS,
                        XCB_ATOM_STRING, 8, sizeof(wm_class) - 1, wm_class);
    xcb_change_property(connection, XCB_PROP_MODE_REPLACE, window, XCB_ATOM_WM_NAME,
                        XCB_ATOM_STRING, 8, sizeof(title) - 1, title);

    xcb_atom_t protocols = atom(connection, "WM_PROTOCOLS");
    xcb_atom_t delete_window = atom(connection, "WM_DELETE_WINDOW");
    if (protocols == XCB_ATOM_NONE || delete_window == XCB_ATOM_NONE) {
        xcb_disconnect(connection);
        return 1;
    }
    xcb_change_property(connection, XCB_PROP_MODE_REPLACE, window, protocols,
                        XCB_ATOM_ATOM, 32, 1, &delete_window);
    xcb_map_window(connection, window);
    xcb_flush(connection);

    for (;;) {
        xcb_generic_event_t *event = xcb_wait_for_event(connection);
        if (event == NULL) {
            break;
        }
        uint8_t type = event->response_type & 0x7f;
        if (type == XCB_CLIENT_MESSAGE) {
            xcb_client_message_event_t *message = (xcb_client_message_event_t *)event;
            if (message->type == protocols && message->data.data32[0] == delete_window) {
                free(event);
                break;
            }
        }
        free(event);
    }
    xcb_unmap_window(connection, window);
    xcb_destroy_window(connection, window);
    xcb_flush(connection);
    xcb_disconnect(connection);
    return 0;
}
