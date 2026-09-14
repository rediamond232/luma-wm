#pragma once

#include <stdint.h>

#define LUMA_VK_EXPORT_MAGIC 0x4c564b45u /* LVKE */
#define LUMA_VK_ACK_MAGIC    0x4c564b41u /* LVKA */
#define LUMA_VK_PROTOCOL_VERSION 2u

/* Sent over SOCK_DGRAM with exactly two SCM_RIGHTS descriptors: DMA-BUF, then
 * sync_file fence.  FDs are transferred, not borrowed. */
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
};

struct luma_vk_ack_message {
    uint32_t magic;
    uint16_t version;
    uint16_t size;
    uint64_t generation;
    uint32_t slot;
    uint32_t reserved;
};
