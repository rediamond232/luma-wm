#define _POSIX_C_SOURCE 200809L

#include <errno.h>
#include <poll.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <xcb/present.h>
#include <xcb/xcb.h>

static uint64_t monotonic_ns(void) {
    struct timespec value;
    if (clock_gettime(CLOCK_MONOTONIC, &value) != 0) return 0;
    return (uint64_t)value.tv_sec * UINT64_C(1000000000) + (uint64_t)value.tv_nsec;
}

static xcb_window_t active_window(xcb_connection_t *connection, xcb_screen_t *screen) {
    static const char name[] = "_NET_ACTIVE_WINDOW";
    xcb_intern_atom_cookie_t atom_cookie =
        xcb_intern_atom(connection, 0, sizeof(name) - 1, name);
    xcb_intern_atom_reply_t *atom = xcb_intern_atom_reply(connection, atom_cookie, NULL);
    if (atom == NULL) return XCB_WINDOW_NONE;
    xcb_get_property_cookie_t property_cookie = xcb_get_property(
        connection, 0, screen->root, atom->atom, XCB_ATOM_WINDOW, 0, 1);
    free(atom);
    xcb_get_property_reply_t *property =
        xcb_get_property_reply(connection, property_cookie, NULL);
    xcb_window_t window = XCB_WINDOW_NONE;
    if (property != NULL && property->format == 32 &&
        xcb_get_property_value_length(property) == (int)sizeof(window)) {
        memcpy(&window, xcb_get_property_value(property), sizeof(window));
    }
    free(property);
    return window;
}

int main(int argc, char **argv) {
    if (argc < 2 || argc > 3) {
        fprintf(stderr, "Usage: %s --active|WINDOW_ID [SECONDS]\n", argv[0]);
        return 64;
    }
    char *end = NULL;
    unsigned long seconds = argc == 3 ? strtoul(argv[2], &end, 10) : 10;
    if (argc == 3 && (end == argv[2] || *end != '\0' || seconds == 0 || seconds > 300)) {
        fputs("SECONDS must be between 1 and 300\n", stderr);
        return 64;
    }

    int screen_number = 0;
    xcb_connection_t *connection = xcb_connect(NULL, &screen_number);
    if (connection == NULL || xcb_connection_has_error(connection)) {
        fputs("cannot connect to the X11 display\n", stderr);
        return 69;
    }
    const xcb_setup_t *setup = xcb_get_setup(connection);
    xcb_screen_iterator_t screens = xcb_setup_roots_iterator(setup);
    for (int index = 0; index < screen_number && screens.rem != 0; ++index) {
        xcb_screen_next(&screens);
    }
    if (screens.rem == 0) {
        xcb_disconnect(connection);
        fputs("X11 screen is unavailable\n", stderr);
        return 69;
    }

    xcb_window_t window = XCB_WINDOW_NONE;
    if (strcmp(argv[1], "--active") == 0) {
        window = active_window(connection, screens.data);
    } else {
        errno = 0;
        unsigned long parsed = strtoul(argv[1], &end, 0);
        if (errno == 0 && end != argv[1] && *end == '\0' && parsed <= UINT32_MAX) {
            window = (xcb_window_t)parsed;
        }
    }
    if (window == XCB_WINDOW_NONE) {
        xcb_disconnect(connection);
        fputs("could not resolve an X11 target window\n", stderr);
        return 65;
    }

    const xcb_query_extension_reply_t *present =
        xcb_get_extension_data(connection, &xcb_present_id);
    if (present == NULL || !present->present) {
        xcb_disconnect(connection);
        fputs("X11 Present extension is unavailable\n", stderr);
        return 69;
    }
    xcb_present_event_t event_id = xcb_generate_id(connection);
    xcb_void_cookie_t select = xcb_present_select_input_checked(
        connection, event_id, window,
        XCB_PRESENT_EVENT_MASK_COMPLETE_NOTIFY | XCB_PRESENT_EVENT_MASK_IDLE_NOTIFY);
    xcb_generic_error_t *error = xcb_request_check(connection, select);
    if (error != NULL) {
        fprintf(stderr, "Present subscription failed with X11 error %u\n", error->error_code);
        free(error);
        xcb_disconnect(connection);
        return 70;
    }
    xcb_flush(connection);

    const uint64_t started = monotonic_ns();
    const uint64_t deadline = started + seconds * UINT64_C(1000000000);
    uint64_t count = 0, first_ust = 0, last_ust = 0, largest_gap = 0;
    uint64_t copied = 0, flipped = 0, skipped = 0, suboptimal = 0;
    uint64_t idle = 0;
    xcb_pixmap_t pixmaps[256] = {0};
    size_t unique_pixmaps = 0;
    while (monotonic_ns() < deadline) {
        struct pollfd descriptor = {.fd = xcb_get_file_descriptor(connection), .events = POLLIN};
        uint64_t remaining = deadline - monotonic_ns();
        int timeout = remaining > UINT64_C(1000000000)
                          ? 1000
                          : (int)((remaining + UINT64_C(999999)) / UINT64_C(1000000));
        (void)poll(&descriptor, 1, timeout);
        xcb_generic_event_t *event = NULL;
        while ((event = xcb_poll_for_event(connection)) != NULL) {
            if ((event->response_type & 0x7fU) == XCB_GE_GENERIC) {
                xcb_present_complete_notify_event_t *complete =
                    (xcb_present_complete_notify_event_t *)event;
                if (complete->extension == present->major_opcode &&
                    complete->event_type == XCB_PRESENT_COMPLETE_NOTIFY &&
                    complete->event == event_id &&
                    complete->kind == XCB_PRESENT_COMPLETE_KIND_PIXMAP) {
                    if (first_ust == 0) first_ust = complete->ust;
                    if (last_ust != 0 && complete->ust > last_ust &&
                        complete->ust - last_ust > largest_gap) {
                        largest_gap = complete->ust - last_ust;
                    }
                    last_ust = complete->ust;
                    ++count;
                    if (complete->mode == XCB_PRESENT_COMPLETE_MODE_COPY) ++copied;
                    else if (complete->mode == XCB_PRESENT_COMPLETE_MODE_FLIP) ++flipped;
                    else if (complete->mode == XCB_PRESENT_COMPLETE_MODE_SKIP) ++skipped;
                    else if (complete->mode == XCB_PRESENT_COMPLETE_MODE_SUBOPTIMAL_COPY) ++suboptimal;
                } else if (complete->extension == present->major_opcode &&
                           complete->event_type == XCB_PRESENT_IDLE_NOTIFY) {
                    xcb_present_idle_notify_event_t *idle_event =
                        (xcb_present_idle_notify_event_t *)event;
                    ++idle;
                    size_t index = 0;
                    while (index < unique_pixmaps && pixmaps[index] != idle_event->pixmap) ++index;
                    if (index == unique_pixmaps && unique_pixmaps < 256)
                        pixmaps[unique_pixmaps++] = idle_event->pixmap;
                }
            }
            free(event);
        }
        if (xcb_connection_has_error(connection)) break;
    }
    const double wall_seconds = (double)(monotonic_ns() - started) / 1000000000.0;
    const double event_seconds = count > 1 && last_ust > first_ust
                                     ? (double)(last_ust - first_ust) / 1000000.0
                                     : 0.0;
    const double rate = event_seconds > 0.0 ? (double)(count - 1) / event_seconds
                                             : (wall_seconds > 0.0 ? (double)count / wall_seconds : 0.0);
    printf("window=%#x seconds=%.3f presents=%llu rate=%.2f fps max_gap=%.3f ms "
           "copy=%llu flip=%llu skip=%llu suboptimal=%llu idle=%llu pixmaps=%zu\n",
           window, wall_seconds, (unsigned long long)count, rate,
           (double)largest_gap / 1000.0, (unsigned long long)copied,
           (unsigned long long)flipped, (unsigned long long)skipped,
           (unsigned long long)suboptimal, (unsigned long long)idle, unique_pixmaps);
    xcb_disconnect(connection);
    return count == 0 ? 2 : 0;
}
