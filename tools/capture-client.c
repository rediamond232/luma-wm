#define _GNU_SOURCE
#include <assert.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <unistd.h>
#include <wayland-client.h>
#include "capture-source.h"
#include "copy-capture.h"
static struct wl_shm *shm;
static struct wl_output *output;
static struct ext_output_image_capture_source_manager_v1 *sources;
static struct ext_image_copy_capture_manager_v1 *manager;
static uint32_t width, height;
static int constraints, completed, failed, format_ok;
static uint32_t failure_reason;
static void geometry(void *d, struct wl_output *o, int32_t x, int32_t y, int32_t w, int32_t h, int32_t sub, const char *make, const char *model, int32_t transform) {}
static void mode(void *d, struct wl_output *o, uint32_t flags, int32_t w, int32_t h, int32_t refresh) {}
static const struct wl_output_listener output_listener = {.geometry=geometry,.mode=mode};
static void global(void *d, struct wl_registry *r, uint32_t id, const char *interface, uint32_t version) {
    if (!strcmp(interface,"wl_shm")) shm=wl_registry_bind(r,id,&wl_shm_interface,1);
    if (!strcmp(interface,"wl_output") && !output) { output=wl_registry_bind(r,id,&wl_output_interface,1); wl_output_add_listener(output,&output_listener,NULL); }
    if (!strcmp(interface,"ext_output_image_capture_source_manager_v1")) sources=wl_registry_bind(r,id,&ext_output_image_capture_source_manager_v1_interface,1);
    if (!strcmp(interface,"ext_image_copy_capture_manager_v1")) manager=wl_registry_bind(r,id,&ext_image_copy_capture_manager_v1_interface,1);
}
static void removed(void *d, struct wl_registry *r, uint32_t id) {}
static const struct wl_registry_listener registry_listener={global,removed};
static void size(void *d, struct ext_image_copy_capture_session_v1 *s, uint32_t w,uint32_t h) {width=w;height=h;}
static void format(void *d, struct ext_image_copy_capture_session_v1 *s,uint32_t f) {if(f==WL_SHM_FORMAT_ARGB8888) format_ok=1;}
static void device(void *d,struct ext_image_copy_capture_session_v1 *s,struct wl_array *a) {}
static void dmaformat(void *d,struct ext_image_copy_capture_session_v1 *s,uint32_t f,struct wl_array *a) {}
static void done(void *d,struct ext_image_copy_capture_session_v1 *s) {constraints++;}
static void stopped(void *d,struct ext_image_copy_capture_session_v1 *s) {failed=1;}
static const struct ext_image_copy_capture_session_v1_listener session_listener={size,format,device,dmaformat,done,stopped};
static void transform(void *d,struct ext_image_copy_capture_frame_v1 *f,uint32_t t) {assert(t==WL_OUTPUT_TRANSFORM_NORMAL);}
static void damage(void *d,struct ext_image_copy_capture_frame_v1 *f,int32_t x,int32_t y,int32_t w,int32_t h) {}
static void timestamp(void *d,struct ext_image_copy_capture_frame_v1 *f,uint32_t hi,uint32_t lo,uint32_t ns) {assert(ns<1000000000);}
static void ready(void *d,struct ext_image_copy_capture_frame_v1 *f) {completed=1;}
static void failure(void *d,struct ext_image_copy_capture_frame_v1 *f,uint32_t reason) {failure_reason=reason;failed=1;}
static const struct ext_image_copy_capture_frame_v1_listener frame_listener={transform,damage,timestamp,ready,failure};
int main(int argc,char **argv) {
    assert(argc==2 || (argc==3 && (!strcmp(argv[2],"--resize") || !strcmp(argv[2],"--cursor")))); alarm(20);
    struct wl_display *display=wl_display_connect(NULL); assert(display);
    struct wl_registry *registry=wl_display_get_registry(display);
    wl_registry_add_listener(registry,&registry_listener,NULL);
    assert(wl_display_roundtrip(display)>=0); assert(shm && output && sources && manager);
    struct ext_image_capture_source_v1 *source=ext_output_image_capture_source_manager_v1_create_source(sources,output);
    struct ext_image_copy_capture_session_v1 *session=ext_image_copy_capture_manager_v1_create_session(manager,source,
        argc==3 && !strcmp(argv[2],"--cursor") ? EXT_IMAGE_COPY_CAPTURE_MANAGER_V1_OPTIONS_PAINT_CURSORS : 0);
    ext_image_copy_capture_session_v1_add_listener(session,&session_listener,NULL);
    while(!constraints && !failed) assert(wl_display_dispatch(display)>=0);
    assert(!failed && format_ok && width && height && width<16384 && height<16384);
    size_t stride=width*4+16, offset=64, bytes=offset+stride*height;
    int fd=memfd_create("wm-capture-test",MFD_CLOEXEC); assert(fd>=0 && ftruncate(fd,bytes)==0);
    uint8_t *pixels=mmap(NULL,bytes,PROT_READ|PROT_WRITE,MAP_SHARED,fd,0); assert(pixels!=MAP_FAILED); memset(pixels,0xcd,bytes);
    struct wl_shm_pool *pool=wl_shm_create_pool(shm,fd,bytes);
    if(argc==3 && !strcmp(argv[2],"--resize")) {
        uint32_t old_width=width, old_height=height;
        int old_constraints=constraints;
        struct wl_buffer *old_buffer=wl_shm_pool_create_buffer(pool,offset,width,height,stride,WL_SHM_FORMAT_ARGB8888);
        struct ext_image_copy_capture_frame_v1 *old_frame=ext_image_copy_capture_session_v1_create_frame(session);
        ext_image_copy_capture_frame_v1_add_listener(old_frame,&frame_listener,NULL);
        ext_image_copy_capture_frame_v1_attach_buffer(old_frame,old_buffer);
        // Ensure the server created the frame with the old constraints before
        // asking the harness to resize its own nested compositor window.
        assert(wl_display_roundtrip(display)>=0);
        char *marker=NULL; assert(asprintf(&marker,"%s.resize-ready",argv[1])>=0);
        FILE *file=fopen(marker,"w"); assert(file); assert(fclose(file)==0); free(marker);
        while(!failed && (constraints==old_constraints || (width==old_width && height==old_height)))
            assert(wl_display_dispatch(display)>=0);
        assert(!failed);
        ext_image_copy_capture_frame_v1_capture(old_frame);
        while(!completed && !failed) assert(wl_display_dispatch(display)>=0);
        assert(!completed && failed && failure_reason==EXT_IMAGE_COPY_CAPTURE_FRAME_V1_FAILURE_REASON_BUFFER_CONSTRAINTS);
        for(size_t i=0;i<bytes;i++) assert(pixels[i]==0xcd);
        ext_image_copy_capture_frame_v1_destroy(old_frame);
        wl_buffer_destroy(old_buffer); wl_shm_pool_destroy(pool);
        munmap(pixels,bytes); close(fd);
        failed=0;
        assert(width && height && width<16384 && height<16384);
        stride=width*4+16; bytes=offset+stride*height;
        fd=memfd_create("wm-capture-resized",MFD_CLOEXEC); assert(fd>=0 && ftruncate(fd,bytes)==0);
        pixels=mmap(NULL,bytes,PROT_READ|PROT_WRITE,MAP_SHARED,fd,0); assert(pixels!=MAP_FAILED); memset(pixels,0xcd,bytes);
        pool=wl_shm_create_pool(shm,fd,bytes);
    }
    // An incompatible buffer must fail without changing any pool bytes, and
    // the same capture session must remain usable for the next valid frame.
    assert(width>1);
    struct wl_buffer *bad_buffer=wl_shm_pool_create_buffer(pool,offset,width-1,height,stride,WL_SHM_FORMAT_ARGB8888);
    struct ext_image_copy_capture_frame_v1 *bad_frame=ext_image_copy_capture_session_v1_create_frame(session);
    ext_image_copy_capture_frame_v1_add_listener(bad_frame,&frame_listener,NULL);
    ext_image_copy_capture_frame_v1_attach_buffer(bad_frame,bad_buffer);
    ext_image_copy_capture_frame_v1_capture(bad_frame);
    while(!completed && !failed) assert(wl_display_dispatch(display)>=0);
    assert(!completed && failed && failure_reason==EXT_IMAGE_COPY_CAPTURE_FRAME_V1_FAILURE_REASON_BUFFER_CONSTRAINTS);
    for(size_t i=0;i<bytes;i++) assert(pixels[i]==0xcd);
    ext_image_copy_capture_frame_v1_destroy(bad_frame);
    wl_buffer_destroy(bad_buffer);
    failed=0;
    struct wl_buffer *buffer=wl_shm_pool_create_buffer(pool,offset,width,height,stride,WL_SHM_FORMAT_ARGB8888);
    struct ext_image_copy_capture_frame_v1 *frame=ext_image_copy_capture_session_v1_create_frame(session);
    ext_image_copy_capture_frame_v1_add_listener(frame,&frame_listener,NULL);
    ext_image_copy_capture_frame_v1_attach_buffer(frame,buffer);
    ext_image_copy_capture_frame_v1_damage_buffer(frame,0,0,width,height);
    ext_image_copy_capture_frame_v1_capture(frame);
    while(!completed && !failed) assert(wl_display_dispatch(display)>=0);
    assert(completed && !failed);
    for(size_t i=0;i<offset;i++) assert(pixels[i]==0xcd);
    FILE *file=fopen(argv[1],"wb"); assert(file); fprintf(file,"P6\n%u %u\n255\n",width,height);
    for(uint32_t y=0;y<height;y++) {
        for(uint32_t x=0;x<width;x++) {uint8_t *p=pixels+offset+y*stride+x*4; uint8_t rgb[]={p[2],p[1],p[0]}; assert(fwrite(rgb,1,3,file)==3);}
        for(size_t x=width*4;x<stride;x++) assert(pixels[offset+y*stride+x]==0xcd);
    }
    assert(fclose(file)==0);
    char *receipt=NULL; assert(asprintf(&receipt,"%s.ok",argv[1])>=0);
    file=fopen(receipt,"w"); assert(file); assert(fclose(file)==0); free(receipt);
    ext_image_copy_capture_frame_v1_destroy(frame); ext_image_copy_capture_session_v1_destroy(session);
    wl_buffer_destroy(buffer); wl_shm_pool_destroy(pool); munmap(pixels,bytes); close(fd);
    wl_display_disconnect(display); return 0;
}
