#pragma once

#include <stdint.h>

#define LUMA_VK_EXPORT_MAGIC 0x4c564b45u /* LVKE */
#define LUMA_VK_ACK_MAGIC    0x4c564b41u /* LVKA */
#define LUMA_VK_PROTOCOL_VERSION 3u
#define LUMA_EXPORT_COPY_COMPLETE 1u
#define LUMA_EXPORT_FLIP_Y 2u
#define LUMA_EXPORT_OPAQUE_MEMORY 4u

/* Sent over SOCK_DGRAM with exactly two SCM_RIGHTS descriptors: DMA-BUF, then
 * sync_file fence. COPY_COMPLETE in reserved omits the fence descriptor;
 * the sender has already observed GPU completion. FLIP_Y requests a vertical
 * flip at the receiver. OPAQUE_MEMORY instead sends an opaque memory FD and
 * one reusable external semaphore FD; allocation_size and device_uuid then
 * replace DMA-BUF layout fields. FDs are transferred, not borrowed. */
struct luma_vk_export_message {
    uint32_t magic;
    uint16_t version;
    uint16_t size;
    uint64_t generation;
    uint64_t pts_ns;
    uint64_t stream_id;
    uint32_t slot;
    uint32_t width;
    uint32_t height;
    uint32_t drm_fourcc;
    uint32_t offset;
    uint32_t pitch;
    uint32_t reserved;
    uint64_t modifier;
    uint64_t allocation_size;
    uint8_t device_uuid[16];
};

struct luma_vk_ack_message {
    uint32_t magic;
    uint16_t version;
    uint16_t size;
    uint64_t generation;
    uint32_t slot;
    uint32_t reserved;
};
