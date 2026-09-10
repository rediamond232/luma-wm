#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
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

static xcb_window_t create_window(xcb_connection_t *connection, xcb_screen_t *screen,
                                  int16_t x, int16_t y, uint16_t width, uint16_t height,
                                  uint32_t color, const char *title, const char *class_name,
                                  const char *type_name, xcb_window_t transient_for) {
    xcb_window_t window = xcb_generate_id(connection);
    uint32_t values[] = {color, XCB_EVENT_MASK_EXPOSURE | XCB_EVENT_MASK_STRUCTURE_NOTIFY};
    xcb_create_window(connection, XCB_COPY_FROM_PARENT, window, screen->root,
                      x, y, width, height, 0, XCB_WINDOW_CLASS_INPUT_OUTPUT,
                      screen->root_visual, XCB_CW_BACK_PIXEL | XCB_CW_EVENT_MASK, values);
    xcb_change_property(connection, XCB_PROP_MODE_REPLACE, window, XCB_ATOM_WM_NAME,
                        XCB_ATOM_STRING, 8, strlen(title), title);
    xcb_change_property(connection, XCB_PROP_MODE_REPLACE, window, XCB_ATOM_WM_CLASS,
                        XCB_ATOM_STRING, 8, strlen(class_name) + 1, class_name);

    if (type_name != NULL) {
        xcb_atom_t type_property = atom(connection, "_NET_WM_WINDOW_TYPE");
        xcb_atom_t type = atom(connection, type_name);
        xcb_change_property(connection, XCB_PROP_MODE_REPLACE, window, type_property,
                            XCB_ATOM_ATOM, 32, 1, &type);
    }
    if (transient_for != XCB_WINDOW_NONE) {
        xcb_change_property(connection, XCB_PROP_MODE_REPLACE, window, XCB_ATOM_WM_TRANSIENT_FOR,
                            XCB_ATOM_WINDOW, 32, 1, &transient_for);
    }
    xcb_map_window(connection, window);
    return window;
}

int main(void) {
    int screen_number = 0;
    xcb_connection_t *connection = xcb_connect(NULL, &screen_number);
    if (xcb_connection_has_error(connection)) {
        fputs("could not connect to X11 display\n", stderr);
        return 1;
    }
    xcb_screen_iterator_t screens = xcb_setup_roots_iterator(xcb_get_setup(connection));
    for (int index = 0; index < screen_number; index++) {
        xcb_screen_next(&screens);
    }
    xcb_screen_t *screen = screens.data;
    if (screen == NULL) {
        xcb_disconnect(connection);
        return 1;
    }

    xcb_window_t parent = create_window(connection, screen, 40, 40, 420, 260, 0x00364f6b,
                                        "X11 normal fixture", "luma-x11-normal", NULL,
                                        XCB_WINDOW_NONE);
    create_window(connection, screen, 0, 0, 300, 140, 0x00a13d4f,
                  "X11 warning fixture", "luma-x11-warning", "_NET_WM_WINDOW_TYPE_DIALOG",
                  parent);
    xcb_window_t notification = create_window(
        connection, screen, 120, 80, 240, 100, 0x00d18b35,
        "X11 notification fixture", "luma-x11-notification",
        "_NET_WM_WINDOW_TYPE_NOTIFICATION", XCB_WINDOW_NONE);
    xcb_flush(connection);

    // Toolkits commonly settle a popup's final anchor after mapping it. The
    // compositor must honor this move for popup-like surfaces only.
    sleep(1);
    uint32_t popup_position[] = {620, 80};
    xcb_configure_window(connection, notification, XCB_CONFIG_WINDOW_X | XCB_CONFIG_WINDOW_Y,
                         popup_position);
    xcb_flush(connection);

    for (;;) {
        xcb_generic_event_t *event = xcb_wait_for_event(connection);
        if (event == NULL) {
            break;
        }
        free(event);
    }
    xcb_disconnect(connection);
    return 0;
}
