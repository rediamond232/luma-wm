#define _POSIX_C_SOURCE 200809L

#include <errno.h>
#include <inttypes.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>
#include <xcb/composite.h>
#include <xcb/dri3.h>
#include <xcb/xcb.h>

int main(int argc, char **argv) {
    if (argc != 2) {
        fprintf(stderr, "Usage: %s WINDOW_ID\n", argv[0]);
        return 64;
    }
    char *end = NULL;
    errno = 0;
    unsigned long parsed = strtoul(argv[1], &end, 0);
    if (errno != 0 || end == argv[1] || *end != '\0' || parsed == 0 || parsed > UINT32_MAX) {
        fputs("WINDOW_ID must be a nonzero 32-bit X11 resource ID\n", stderr);
        return 64;
    }
    xcb_window_t window = (xcb_window_t)parsed;
    int screen = 0;
    xcb_connection_t *connection = xcb_connect(NULL, &screen);
    if (connection == NULL || xcb_connection_has_error(connection)) {
        fputs("cannot connect to the X11 display\n", stderr);
        return 69;
    }
    const xcb_query_extension_reply_t *composite =
        xcb_get_extension_data(connection, &xcb_composite_id);
    const xcb_query_extension_reply_t *dri3 =
        xcb_get_extension_data(connection, &xcb_dri3_id);
    if (composite == NULL || !composite->present || dri3 == NULL || !dri3->present) {
        fputs("X Composite or DRI3 is unavailable\n", stderr);
        xcb_disconnect(connection);
        return 69;
    }

    xcb_generic_error_t *error = NULL;
    xcb_get_geometry_reply_t *geometry =
        xcb_get_geometry_reply(connection, xcb_get_geometry(connection, window), &error);
    if (geometry == NULL) {
        fprintf(stderr, "cannot query window geometry (X11 error %u)\n",
                error == NULL ? 0 : error->error_code);
        free(error);
        xcb_disconnect(connection);
        return 65;
    }
    free(error);
    error = NULL;

    xcb_void_cookie_t redirect = xcb_composite_redirect_window_checked(
        connection, window, XCB_COMPOSITE_REDIRECT_AUTOMATIC);
    error = xcb_request_check(connection, redirect);
    int redirected = error == NULL;
    if (error != NULL && error->error_code != XCB_ACCESS) {
        fprintf(stderr, "cannot redirect the window (X11 error %u)\n", error->error_code);
        free(error);
        free(geometry);
        xcb_disconnect(connection);
        return 70;
    }
    free(error);

    xcb_pixmap_t pixmap = xcb_generate_id(connection);
    xcb_void_cookie_t named =
        xcb_composite_name_window_pixmap_checked(connection, window, pixmap);
    error = xcb_request_check(connection, named);
    if (error != NULL) {
        fprintf(stderr, "cannot name the redirected window pixmap (X11 error %u)\n",
                error->error_code);
        free(error);
        if (redirected) {
            xcb_composite_unredirect_window(connection, window,
                                            XCB_COMPOSITE_REDIRECT_AUTOMATIC);
        }
        free(geometry);
        xcb_disconnect(connection);
        return 71;
    }

    xcb_dri3_buffers_from_pixmap_cookie_t cookie =
        xcb_dri3_buffers_from_pixmap(connection, pixmap);
    xcb_dri3_buffers_from_pixmap_reply_t *reply =
        xcb_dri3_buffers_from_pixmap_reply(connection, cookie, &error);
    if (reply == NULL || error != NULL || reply->nfd == 0) {
        fprintf(stderr, "DRI3 DMA-BUF export failed (X11 error %u)\n",
                error == NULL ? 0 : error->error_code);
        free(error);
        free(reply);
        xcb_free_pixmap(connection, pixmap);
        if (redirected) {
            xcb_composite_unredirect_window(connection, window,
                                            XCB_COMPOSITE_REDIRECT_AUTOMATIC);
        }
        free(geometry);
        xcb_disconnect(connection);
        return 72;
    }

    int *fds = xcb_dri3_buffers_from_pixmap_reply_fds(connection, reply);
    uint32_t *strides = xcb_dri3_buffers_from_pixmap_strides(reply);
    uint32_t *offsets = xcb_dri3_buffers_from_pixmap_offsets(reply);
    printf("window=%#x geometry=%ux%u export=%ux%u depth=%u bpp=%u planes=%u "
           "modifier=%#" PRIx64 "\n",
           window, geometry->width, geometry->height, reply->width, reply->height,
           reply->depth, reply->bpp, reply->nfd, reply->modifier);
    for (uint8_t index = 0; index < reply->nfd; ++index) {
        struct stat metadata;
        if (fstat(fds[index], &metadata) == 0) {
            printf("plane=%u fd=%d stride=%u offset=%u size=%lld device=%ju inode=%ju\n",
                   index, fds[index], strides[index], offsets[index],
                   (long long)metadata.st_size, (uintmax_t)metadata.st_dev,
                   (uintmax_t)metadata.st_ino);
        }
        close(fds[index]);
    }
    free(reply);
    xcb_free_pixmap(connection, pixmap);
    if (redirected) {
        xcb_composite_unredirect_window(connection, window,
                                        XCB_COMPOSITE_REDIRECT_AUTOMATIC);
    }
    xcb_flush(connection);
    free(geometry);
    xcb_disconnect(connection);
    return 0;
}
