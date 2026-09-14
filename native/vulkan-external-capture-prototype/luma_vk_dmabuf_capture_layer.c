/*
 * GPU-only Vulkan present capture layer.
 *
 * It is selected explicitly through VK_INSTANCE_LAYERS by a launcher profile,
 * intercepts only Vulkan dispatch, and does not access application memory.
 */
#define _POSIX_C_SOURCE 200809L
#include <vulkan/vulkan.h>
#include <vulkan/vk_layer.h>

#include "luma_vk_export_protocol.h"

#include <libdrm/drm_fourcc.h>

#include <errno.h>
#include <stdarg.h>
#include <pthread.h>
#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <time.h>
#include <unistd.h>

#define LUMA_MAX_DEVICES 8
#define LUMA_MAX_QUEUES 32
#define LUMA_MAX_SWAPCHAINS 16
#define LUMA_RING 4
struct device_entry;
struct capture_slot {
    VkImage image;
    VkDeviceMemory memory;
    VkSemaphore present_ready;
    VkSemaphore capture_ready;
    VkCommandBuffer command_buffer;
    VkCommandPool command_pool;
    int dmabuf_fd;
    int fence_fd;
    uint64_t generation;
    uint64_t pts_ns;
    uint32_t drm_fourcc;
    uint32_t offset;
    uint32_t pitch;
    uint64_t modifier;
    bool busy;
    bool sent;
    bool initialized;
};
struct swapchain_entry {
    VkSwapchainKHR handle;
    uint64_t identity;
    struct device_entry *device;
    VkExtent2D extent;
    VkFormat format;
    VkImage images[16];
    uint32_t image_count;
    struct capture_slot slot[LUMA_RING];
    uint32_t cursor;
    bool export_ready;
};
struct device_entry {
    VkDevice handle;
    VkPhysicalDevice physical;
    PFN_vkGetDeviceProcAddr gdpa;
    PFN_vkDestroyDevice destroy_device;
    PFN_vkGetDeviceQueue get_queue;
    PFN_vkGetDeviceQueue2 get_queue2;
    PFN_vkCreateSwapchainKHR create_swapchain;
    PFN_vkDestroySwapchainKHR destroy_swapchain;
    PFN_vkGetSwapchainImagesKHR get_swapchain_images;
    PFN_vkQueuePresentKHR queue_present;
    PFN_vkQueueSubmit queue_submit;
    PFN_vkCreateCommandPool create_command_pool;
    PFN_vkDestroyCommandPool destroy_command_pool;
    PFN_vkAllocateCommandBuffers allocate_command_buffers;
    PFN_vkFreeCommandBuffers free_command_buffers;
    PFN_vkBeginCommandBuffer begin_command_buffer;
    PFN_vkEndCommandBuffer end_command_buffer;
    PFN_vkResetCommandBuffer reset_command_buffer;
    PFN_vkCmdPipelineBarrier cmd_pipeline_barrier;
    PFN_vkCmdCopyImage cmd_copy_image;
    PFN_vkCreateImage create_image;
    PFN_vkDestroyImage destroy_image;
    PFN_vkGetImageMemoryRequirements get_image_memory_requirements;
    PFN_vkGetImageSubresourceLayout get_image_subresource_layout;
    PFN_vkGetImageDrmFormatModifierPropertiesEXT get_image_modifier_properties;
    PFN_vkAllocateMemory allocate_memory;
    PFN_vkFreeMemory free_memory;
    PFN_vkBindImageMemory bind_image_memory;
    PFN_vkCreateSemaphore create_semaphore;
    PFN_vkDestroySemaphore destroy_semaphore;
    PFN_vkGetMemoryFdKHR get_memory_fd;
    PFN_vkGetSemaphoreFdKHR get_semaphore_fd;
    VkPhysicalDeviceMemoryProperties memory;
    bool external_export_enabled;
    bool drm_modifier_enabled;
};
struct queue_entry {
    VkQueue handle;
    struct device_entry *device;
    uint32_t family;
    VkCommandPool command_pool;
};

static pthread_mutex_t lock = PTHREAD_MUTEX_INITIALIZER;
static struct device_entry devices[LUMA_MAX_DEVICES];
static struct queue_entry queues[LUMA_MAX_QUEUES];
static struct swapchain_entry swaps[LUMA_MAX_SWAPCHAINS];
static PFN_vkGetInstanceProcAddr instance_gipa;
static VkInstance capture_instance;
static int export_socket = -2;
static uint64_t next_swap_identity = 1;

static void capture_debug(const char *format, ...) {
    const char *enabled = getenv("LUMA_GAME_CAPTURE_DEBUG");
    if (!enabled || !*enabled || strcmp(enabled, "0") == 0) return;
    va_list arguments;
    va_start(arguments, format);
    fputs("luma Vulkan layer: ", stderr);
    vfprintf(stderr, format, arguments);
    fputc('\n', stderr);
    va_end(arguments);
}

static uint64_t monotonic_ns(void) {
    struct timespec now;
    if (clock_gettime(CLOCK_MONOTONIC, &now) != 0) return 0;
    return (uint64_t)now.tv_sec * UINT64_C(1000000000) + (uint64_t)now.tv_nsec;
}

static VkLayerInstanceCreateInfo *instance_link(const void *next) {
    for (const VkBaseInStructure *n = next; n; n = n->pNext)
        if (n->sType == VK_STRUCTURE_TYPE_LOADER_INSTANCE_CREATE_INFO &&
            ((VkLayerInstanceCreateInfo *)(uintptr_t)n)->function == VK_LAYER_LINK_INFO)
            return (VkLayerInstanceCreateInfo *)(uintptr_t)n;
    return NULL;
}
static VkLayerDeviceCreateInfo *device_link(const void *next) {
    for (const VkBaseInStructure *n = next; n; n = n->pNext)
        if (n->sType == VK_STRUCTURE_TYPE_LOADER_DEVICE_CREATE_INFO &&
            ((VkLayerDeviceCreateInfo *)(uintptr_t)n)->function == VK_LAYER_LINK_INFO)
            return (VkLayerDeviceCreateInfo *)(uintptr_t)n;
    return NULL;
}
static struct device_entry *find_device(VkDevice d) {
    for (unsigned i = 0; i < LUMA_MAX_DEVICES; i++) if (devices[i].handle == d) return &devices[i];
    return NULL;
}
static struct queue_entry *find_queue(VkQueue q) {
    for (unsigned i = 0; i < LUMA_MAX_QUEUES; i++) if (queues[i].handle == q) return &queues[i];
    return NULL;
}
static struct swapchain_entry *find_swap(VkSwapchainKHR s) {
    for (unsigned i = 0; i < LUMA_MAX_SWAPCHAINS; i++) if (swaps[i].handle == s) return &swaps[i];
    return NULL;
}

static void release_slot(struct device_entry *device, struct capture_slot *slot) {
    if (slot->fence_fd >= 0) close(slot->fence_fd);
    if (slot->dmabuf_fd >= 0) close(slot->dmabuf_fd);
    if (slot->command_buffer && slot->command_pool && device->free_command_buffers)
        device->free_command_buffers(device->handle, slot->command_pool, 1,
                                     &slot->command_buffer);
    if (slot->present_ready && device->destroy_semaphore)
        device->destroy_semaphore(device->handle, slot->present_ready, NULL);
    if (slot->capture_ready && device->destroy_semaphore)
        device->destroy_semaphore(device->handle, slot->capture_ready, NULL);
    if (slot->image && device->destroy_image)
        device->destroy_image(device->handle, slot->image, NULL);
    if (slot->memory && device->free_memory)
        device->free_memory(device->handle, slot->memory, NULL);
    memset(slot, 0, sizeof(*slot));
    slot->dmabuf_fd = -1;
    slot->fence_fd = -1;
}

static void release_swap(struct swapchain_entry *swap) {
    if (!swap->handle || !swap->device) return;
    for (unsigned index = 0; index < LUMA_RING; ++index)
        release_slot(swap->device, &swap->slot[index]);
    memset(swap, 0, sizeof(*swap));
}

static int open_export_socket(void) {
    if (export_socket != -2) return export_socket;
    const char *path = getenv("LUMA_VK_CAPTURE_SOCKET");
    if (!path || !*path || strlen(path) >= sizeof(((struct sockaddr_un *)0)->sun_path)) {
        capture_debug("export socket path is missing or invalid");
        return (export_socket = -1);
    }
    int fd = socket(AF_UNIX, SOCK_DGRAM | SOCK_NONBLOCK | SOCK_CLOEXEC, 0);
    if (fd < 0) return (export_socket = -1);
    /* A connected Unix datagram socket needs its own address for the receiver
     * to return slot ACKs. Use a process-local abstract address: it leaves no
     * filesystem entry and cannot outlive this process. */
    struct sockaddr_un local;
    memset(&local, 0, sizeof(local));
    local.sun_family = AF_UNIX;
    const int local_length = snprintf(local.sun_path + 1, sizeof(local.sun_path) - 1,
                                      "luma-vk-%ld-%d", (long)getpid(), fd);
    if (local_length <= 0 || (size_t)local_length >= sizeof(local.sun_path) - 1 ||
        bind(fd, (struct sockaddr *)&local,
             (socklen_t)(offsetof(struct sockaddr_un, sun_path) + 1U +
                         (size_t)local_length)) != 0) {
        close(fd);
        return (export_socket = -1);
    }
    struct sockaddr_un addr;
    memset(&addr, 0, sizeof(addr));
    addr.sun_family = AF_UNIX;
    memcpy(addr.sun_path, path, strlen(path) + 1);
    if (connect(fd, (struct sockaddr *)&addr, sizeof(addr)) != 0) {
        capture_debug("cannot connect export socket: %s", strerror(errno));
        close(fd); return (export_socket = -1);
    }
    export_socket = fd;
    return fd;
}
static void drain_acks(void) {
    int fd = open_export_socket();
    if (fd < 0) return;
    struct luma_vk_ack_message ack;
    while (recv(fd, &ack, sizeof(ack), MSG_DONTWAIT) == (ssize_t)sizeof(ack)) {
        if (ack.magic != LUMA_VK_ACK_MAGIC || ack.version != LUMA_VK_PROTOCOL_VERSION) continue;
        for (unsigned s = 0; s < LUMA_MAX_SWAPCHAINS; s++) for (unsigned i = 0; i < LUMA_RING; i++) {
            struct capture_slot *slot = &swaps[s].slot[i];
            if (slot->busy && i == ack.slot && slot->generation == ack.generation) {
                slot->busy = false; slot->sent = false;
            }
        }
    }
}
static void emit_slot(struct swapchain_entry *swap, unsigned index) {
    struct capture_slot *slot = &swap->slot[index];
    int fd = open_export_socket();
    if (fd < 0 || slot->fence_fd < 0 || slot->dmabuf_fd < 0) return;
    struct luma_vk_export_message msg = {
        .magic = LUMA_VK_EXPORT_MAGIC, .version = LUMA_VK_PROTOCOL_VERSION, .size = sizeof(msg),
        .generation = slot->generation, .pts_ns = slot->pts_ns,
        .stream_id = swap->identity,
        .slot = index, .width = swap->extent.width,
        .height = swap->extent.height, .drm_fourcc = slot->drm_fourcc,
        .offset = slot->offset, .pitch = slot->pitch, .modifier = slot->modifier,
    };
    char control[CMSG_SPACE(sizeof(int) * 2)];
    struct iovec io = { .iov_base = &msg, .iov_len = sizeof(msg) };
    struct msghdr hdr;
    memset(&hdr, 0, sizeof(hdr));
    hdr.msg_iov = &io; hdr.msg_iovlen = 1; hdr.msg_control = control; hdr.msg_controllen = sizeof(control);
    struct cmsghdr *c = CMSG_FIRSTHDR(&hdr);
    c->cmsg_level = SOL_SOCKET; c->cmsg_type = SCM_RIGHTS; c->cmsg_len = CMSG_LEN(sizeof(int) * 2);
    int fds[2] = { dup(slot->dmabuf_fd), slot->fence_fd };
    if (fds[0] < 0) return;
    memcpy(CMSG_DATA(c), fds, sizeof(fds));
    if (sendmsg(fd, &hdr, MSG_DONTWAIT | MSG_NOSIGNAL) == (ssize_t)sizeof(msg)) {
        /* SCM_RIGHTS duplicates descriptors.  Close our copy after a successful
         * handoff; the consumer owns its received copies. */
        close(fds[0]);
        close(slot->fence_fd);
        slot->fence_fd = -1;
        slot->sent = true;
    } else {
        /* Keep the original sync_file FD to retry metadata handoff on the
         * next present; this layer never waits for a slow recorder. */
        close(fds[0]);
    }
}

static uint32_t export_memory_type(const struct device_entry *d, uint32_t bits) {
    for (uint32_t i = 0; i < d->memory.memoryTypeCount; i++)
        if ((bits & (1u << i)) && (d->memory.memoryTypes[i].propertyFlags & VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT)) return i;
    return UINT32_MAX;
}
static uint32_t drm_format(VkFormat format) {
    switch (format) {
        case VK_FORMAT_B8G8R8A8_UNORM:
        case VK_FORMAT_B8G8R8A8_SRGB: return DRM_FORMAT_ARGB8888;
        case VK_FORMAT_R8G8B8A8_UNORM:
        case VK_FORMAT_R8G8B8A8_SRGB: return DRM_FORMAT_ABGR8888;
        case VK_FORMAT_A2R10G10B10_UNORM_PACK32: return DRM_FORMAT_ARGB2101010;
        case VK_FORMAT_A2B10G10R10_UNORM_PACK32: return DRM_FORMAT_ABGR2101010;
        default: return 0;
    }
}
static uint64_t choose_drm_modifier(VkPhysicalDevice physical, VkFormat format) {
    PFN_vkGetPhysicalDeviceFormatProperties2 get_properties = instance_gipa
        ? (PFN_vkGetPhysicalDeviceFormatProperties2)instance_gipa(
              capture_instance, "vkGetPhysicalDeviceFormatProperties2")
        : NULL;
    if (!get_properties) return DRM_FORMAT_MOD_INVALID;
    VkDrmFormatModifierPropertiesListEXT list = {
        .sType = VK_STRUCTURE_TYPE_DRM_FORMAT_MODIFIER_PROPERTIES_LIST_EXT,
    };
    VkFormatProperties2 properties = {
        .sType = VK_STRUCTURE_TYPE_FORMAT_PROPERTIES_2,
        .pNext = &list,
    };
    get_properties(physical, format, &properties);
    if (list.drmFormatModifierCount == 0 || list.drmFormatModifierCount > 1024)
        return DRM_FORMAT_MOD_INVALID;
    list.pDrmFormatModifierProperties =
        calloc(list.drmFormatModifierCount, sizeof(*list.pDrmFormatModifierProperties));
    if (!list.pDrmFormatModifierProperties) return DRM_FORMAT_MOD_INVALID;
    get_properties(physical, format, &properties);
    uint64_t selected = DRM_FORMAT_MOD_INVALID;
    for (uint32_t index = 0; index < list.drmFormatModifierCount; ++index) {
        const VkDrmFormatModifierPropertiesEXT *candidate =
            &list.pDrmFormatModifierProperties[index];
        if (candidate->drmFormatModifierPlaneCount == 1 &&
            (candidate->drmFormatModifierTilingFeatures &
             VK_FORMAT_FEATURE_TRANSFER_DST_BIT) != 0) {
            selected = candidate->drmFormatModifier;
            break;
        }
    }
    free(list.pDrmFormatModifierProperties);
    return selected;
}
static bool make_slot(struct swapchain_entry *swap, struct capture_slot *slot) {
    struct device_entry *d = swap->device;
    slot->dmabuf_fd = -1;
    slot->fence_fd = -1;
    slot->drm_fourcc = drm_format(swap->format);
    if (slot->drm_fourcc == 0 || !d->get_image_subresource_layout) {
        capture_debug("unsupported swapchain format %u", (unsigned)swap->format);
        return false;
    }
    slot->modifier = d->drm_modifier_enabled
        ? choose_drm_modifier(d->physical, swap->format) : DRM_FORMAT_MOD_INVALID;
    const bool use_modifier = slot->modifier != DRM_FORMAT_MOD_INVALID;
    VkImageDrmFormatModifierListCreateInfoEXT modifier_list = {
        .sType = VK_STRUCTURE_TYPE_IMAGE_DRM_FORMAT_MODIFIER_LIST_CREATE_INFO_EXT,
        .drmFormatModifierCount = use_modifier ? 1U : 0U,
        .pDrmFormatModifiers = use_modifier ? &slot->modifier : NULL,
    };
    VkExternalMemoryImageCreateInfo ext = {
        .sType = VK_STRUCTURE_TYPE_EXTERNAL_MEMORY_IMAGE_CREATE_INFO,
        .pNext = use_modifier ? &modifier_list : NULL,
        .handleTypes = VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT,
    };
    VkImageCreateInfo image = { .sType = VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO, .pNext = &ext,
        .imageType = VK_IMAGE_TYPE_2D, .format = swap->format,
        .extent = { swap->extent.width, swap->extent.height, 1 }, .mipLevels = 1, .arrayLayers = 1,
        .samples = VK_SAMPLE_COUNT_1_BIT,
        .tiling = use_modifier ? VK_IMAGE_TILING_DRM_FORMAT_MODIFIER_EXT : VK_IMAGE_TILING_LINEAR,
        .usage = VK_IMAGE_USAGE_TRANSFER_DST_BIT, .sharingMode = VK_SHARING_MODE_EXCLUSIVE,
        .initialLayout = VK_IMAGE_LAYOUT_UNDEFINED };
    VkResult result = d->create_image(d->handle, &image, NULL, &slot->image);
    if (result != VK_SUCCESS) {
        capture_debug("export image creation failed: %d (modifier=%#llx)", result,
                      (unsigned long long)slot->modifier);
        return false;
    }
    VkMemoryRequirements req;
    d->get_image_memory_requirements(d->handle, slot->image, &req);
    uint32_t type = export_memory_type(d, req.memoryTypeBits);
    if (type == UINT32_MAX) {
        capture_debug("no device-local memory type for export image");
        return false;
    }
    VkExportMemoryAllocateInfo export_info = { .sType = VK_STRUCTURE_TYPE_EXPORT_MEMORY_ALLOCATE_INFO,
        .handleTypes = VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT };
    VkMemoryAllocateInfo alloc = { .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO, .pNext = &export_info,
        .allocationSize = req.size, .memoryTypeIndex = type };
    result = d->allocate_memory(d->handle, &alloc, NULL, &slot->memory);
    if (result != VK_SUCCESS) {
        capture_debug("export memory allocation failed: %d", result);
        return false;
    }
    result = d->bind_image_memory(d->handle, slot->image, slot->memory, 0);
    if (result != VK_SUCCESS) {
        capture_debug("export image bind failed: %d", result);
        return false;
    }
    if (use_modifier && d->get_image_modifier_properties) {
        VkImageDrmFormatModifierPropertiesEXT actual = {
            .sType = VK_STRUCTURE_TYPE_IMAGE_DRM_FORMAT_MODIFIER_PROPERTIES_EXT,
        };
        if (d->get_image_modifier_properties(d->handle, slot->image, &actual) != VK_SUCCESS)
            { capture_debug("cannot query export image DRM modifier"); return false; }
        slot->modifier = actual.drmFormatModifier;
    } else {
        slot->modifier = DRM_FORMAT_MOD_LINEAR;
    }
    VkImageSubresource subresource = {
        .aspectMask = use_modifier ? VK_IMAGE_ASPECT_MEMORY_PLANE_0_BIT_EXT
                                   : VK_IMAGE_ASPECT_COLOR_BIT,
    };
    VkSubresourceLayout layout;
    d->get_image_subresource_layout(d->handle, slot->image, &subresource, &layout);
    if (layout.offset > UINT32_MAX || layout.rowPitch > UINT32_MAX || layout.rowPitch == 0) {
        capture_debug("invalid export image layout");
        return false;
    }
    slot->offset = (uint32_t)layout.offset;
    slot->pitch = (uint32_t)layout.rowPitch;
    VkMemoryGetFdInfoKHR fd_info = { .sType = VK_STRUCTURE_TYPE_MEMORY_GET_FD_INFO_KHR, .memory = slot->memory,
        .handleType = VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT };
    result = d->get_memory_fd(d->handle, &fd_info, &slot->dmabuf_fd);
    if (result != VK_SUCCESS) { capture_debug("DMA-BUF export failed: %d", result); return false; }
    VkExportSemaphoreCreateInfo sem_export = { .sType = VK_STRUCTURE_TYPE_EXPORT_SEMAPHORE_CREATE_INFO,
        .handleTypes = VK_EXTERNAL_SEMAPHORE_HANDLE_TYPE_SYNC_FD_BIT };
    VkSemaphoreCreateInfo sem = { .sType = VK_STRUCTURE_TYPE_SEMAPHORE_CREATE_INFO, .pNext = &sem_export };
    result = d->create_semaphore(d->handle, &sem, NULL, &slot->capture_ready);
    if (result != VK_SUCCESS) { capture_debug("export semaphore creation failed: %d", result); return false; }
    /* The semaphore consumed by WSI need not be exportable.  Keeping it
     * separate is vital: a binary wait consumes its payload. */
    sem.pNext = NULL;
    result = d->create_semaphore(d->handle, &sem, NULL, &slot->present_ready);
    if (result != VK_SUCCESS) { capture_debug("present semaphore creation failed: %d", result); return false; }
    slot->fence_fd = -1;
    return true;
}
/* Linear external images are not a guaranteed DMA-BUF interchange format.
 * Failure is safe: present continues unmodified.  Production must negotiate
 * DRM modifiers with the recorder before allocating these images. */
static void setup_swap(struct swapchain_entry *swap) {
    struct device_entry *d = swap->device;
    if (!d->external_export_enabled || !d->get_memory_fd || !d->get_semaphore_fd) {
        capture_debug("external memory/semaphore functions are unavailable");
        return;
    }
    for (unsigned i = 0; i < LUMA_RING; i++) if (!make_slot(swap, &swap->slot[i])) {
        for (unsigned cleanup = 0; cleanup <= i; ++cleanup)
            release_slot(d, &swap->slot[cleanup]);
        return;
    }
    swap->export_ready = true;
    capture_debug("DMA-BUF ring ready: %ux%u format=%u", swap->extent.width,
                  swap->extent.height, (unsigned)swap->format);
}

static void image_barrier(struct device_entry *d, VkCommandBuffer cmd, VkImage image,
                          VkImageLayout old_layout, VkImageLayout new_layout,
                          VkAccessFlags src_access, VkAccessFlags dst_access,
                          VkPipelineStageFlags src_stage, VkPipelineStageFlags dst_stage) {
    VkImageMemoryBarrier b = { .sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER, .srcAccessMask = src_access,
        .dstAccessMask = dst_access, .oldLayout = old_layout, .newLayout = new_layout,
        .srcQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED, .dstQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
        .image = image, .subresourceRange = { VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1 } };
    d->cmd_pipeline_barrier(cmd, src_stage, dst_stage, 0, 0, NULL, 0, NULL, 1, &b);
}
static bool submit_capture(struct queue_entry *queue, struct swapchain_entry *swap, uint32_t image_index,
                           const VkPresentInfoKHR *present, struct capture_slot *slot) {
    struct device_entry *d = queue->device;
    VkCommandBuffer cmd = slot->command_buffer;
    if (!cmd) {
        VkCommandBufferAllocateInfo alloc = { .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,
            .commandPool = queue->command_pool, .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY, .commandBufferCount = 1 };
        if (d->allocate_command_buffers(d->handle, &alloc, &cmd) != VK_SUCCESS) return false;
        slot->command_buffer = cmd;
        slot->command_pool = queue->command_pool;
    } else if (d->reset_command_buffer(cmd, 0) != VK_SUCCESS) {
        return false;
    }
    VkCommandBufferBeginInfo begin = { .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO,
        .flags = VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT };
    if (d->begin_command_buffer(cmd, &begin) != VK_SUCCESS) return false;
    VkImage source = swap->images[image_index];
    image_barrier(d, cmd, source, VK_IMAGE_LAYOUT_PRESENT_SRC_KHR, VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL,
        VK_ACCESS_MEMORY_READ_BIT, VK_ACCESS_TRANSFER_READ_BIT, VK_PIPELINE_STAGE_ALL_COMMANDS_BIT, VK_PIPELINE_STAGE_TRANSFER_BIT);
    image_barrier(d, cmd, slot->image, slot->initialized ? VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL : VK_IMAGE_LAYOUT_UNDEFINED, VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL,
        0, VK_ACCESS_TRANSFER_WRITE_BIT, VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT, VK_PIPELINE_STAGE_TRANSFER_BIT);
    VkImageCopy copy = { .srcSubresource = { VK_IMAGE_ASPECT_COLOR_BIT, 0, 0, 1 },
        .dstSubresource = { VK_IMAGE_ASPECT_COLOR_BIT, 0, 0, 1 }, .extent = { swap->extent.width, swap->extent.height, 1 } };
    d->cmd_copy_image(cmd, source, VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL, slot->image, VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL, 1, &copy);
    image_barrier(d, cmd, source, VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL, VK_IMAGE_LAYOUT_PRESENT_SRC_KHR,
        VK_ACCESS_TRANSFER_READ_BIT, VK_ACCESS_MEMORY_READ_BIT, VK_PIPELINE_STAGE_TRANSFER_BIT, VK_PIPELINE_STAGE_ALL_COMMANDS_BIT);
    if (d->end_command_buffer(cmd) != VK_SUCCESS) return false;
    VkSemaphore signals[2] = { slot->present_ready, slot->capture_ready };
    VkSubmitInfo submit = { .sType = VK_STRUCTURE_TYPE_SUBMIT_INFO, .waitSemaphoreCount = present->waitSemaphoreCount,
        .pWaitSemaphores = present->pWaitSemaphores, .pWaitDstStageMask = NULL,
        .commandBufferCount = 1, .pCommandBuffers = &cmd, .signalSemaphoreCount = 2, .pSignalSemaphores = signals };
    /* ALL_COMMANDS is needed for the application's render-complete semaphore. */
    VkPipelineStageFlags waits[32];
    if (present->waitSemaphoreCount > 32) return false;
    for (uint32_t i = 0; i < present->waitSemaphoreCount; i++) waits[i] = VK_PIPELINE_STAGE_ALL_COMMANDS_BIT;
    submit.pWaitDstStageMask = waits;
    bool submitted = d->queue_submit(queue->handle, 1, &submit, VK_NULL_HANDLE) == VK_SUCCESS;
    if (submitted) slot->initialized = true;
    return submitted;
}

VKAPI_ATTR VkResult VKAPI_CALL vkCreateInstance(const VkInstanceCreateInfo *info, const VkAllocationCallbacks *alloc, VkInstance *out) {
    VkLayerInstanceCreateInfo *link = instance_link(info ? info->pNext : NULL);
    if (!link || !link->u.pLayerInfo) return VK_ERROR_INITIALIZATION_FAILED;
    instance_gipa = link->u.pLayerInfo->pfnNextGetInstanceProcAddr;
    link->u.pLayerInfo = link->u.pLayerInfo->pNext;
    PFN_vkCreateInstance next = (PFN_vkCreateInstance)instance_gipa(NULL, "vkCreateInstance");
    VkResult result = next ? next(info, alloc, out) : VK_ERROR_INITIALIZATION_FAILED;
    if (result == VK_SUCCESS && out) capture_instance = *out;
    return result;
}
VKAPI_ATTR VkResult VKAPI_CALL vkCreateDevice(VkPhysicalDevice phys, const VkDeviceCreateInfo *info, const VkAllocationCallbacks *alloc, VkDevice *out) {
    VkLayerDeviceCreateInfo *link = device_link(info ? info->pNext : NULL);
    if (!link || !link->u.pLayerInfo) return VK_ERROR_INITIALIZATION_FAILED;
    PFN_vkGetInstanceProcAddr gipa = link->u.pLayerInfo->pfnNextGetInstanceProcAddr;
    PFN_vkGetDeviceProcAddr gdpa = link->u.pLayerInfo->pfnNextGetDeviceProcAddr;
    link->u.pLayerInfo = link->u.pLayerInfo->pNext;
    PFN_vkCreateDevice next = (PFN_vkCreateDevice)gipa(NULL, "vkCreateDevice");
    PFN_vkEnumerateDeviceExtensionProperties enumerate_extensions =
        (PFN_vkEnumerateDeviceExtensionProperties)gipa(capture_instance,
                                                       "vkEnumerateDeviceExtensionProperties");
    const char *required[] = {
        VK_KHR_EXTERNAL_MEMORY_FD_EXTENSION_NAME,
        VK_EXT_EXTERNAL_MEMORY_DMA_BUF_EXTENSION_NAME,
        VK_KHR_EXTERNAL_SEMAPHORE_FD_EXTENSION_NAME,
        VK_EXT_IMAGE_DRM_FORMAT_MODIFIER_EXTENSION_NAME,
    };
    bool supported[4] = {false, false, false, false};
    uint32_t property_count = 0;
    VkExtensionProperties *properties = NULL;
    if (enumerate_extensions &&
        enumerate_extensions(phys, NULL, &property_count, NULL) == VK_SUCCESS &&
        property_count > 0 && property_count <= 65536) {
        properties = calloc(property_count, sizeof(*properties));
        if (properties && enumerate_extensions(phys, NULL, &property_count, properties) == VK_SUCCESS) {
            for (uint32_t property = 0; property < property_count; ++property)
                for (unsigned wanted = 0; wanted < 4; ++wanted)
                    supported[wanted] |= strcmp(properties[property].extensionName,
                                                required[wanted]) == 0;
        }
    }
    free(properties);
    const uint32_t original_count = info ? info->enabledExtensionCount : 0;
    const char **enabled = calloc((size_t)original_count + 4U, sizeof(*enabled));
    VkDeviceCreateInfo forwarded;
    if (!info || !enabled) return VK_ERROR_OUT_OF_HOST_MEMORY;
    forwarded = *info;
    if (original_count > 0)
        memcpy(enabled, info->ppEnabledExtensionNames, (size_t)original_count * sizeof(*enabled));
    uint32_t enabled_count = original_count;
    for (unsigned wanted = 0; wanted < 4; ++wanted) {
        bool already_enabled = false;
        for (uint32_t existing = 0; existing < original_count; ++existing)
            already_enabled |= strcmp(info->ppEnabledExtensionNames[existing], required[wanted]) == 0;
        if (!already_enabled && supported[wanted]) enabled[enabled_count++] = required[wanted];
    }
    forwarded.enabledExtensionCount = enabled_count;
    forwarded.ppEnabledExtensionNames = enabled;
    VkResult r = next ? next(phys, &forwarded, alloc, out) : VK_ERROR_INITIALIZATION_FAILED;
    if (r != VK_SUCCESS) {
        free(enabled);
        return r;
    }
    pthread_mutex_lock(&lock);
    for (unsigned i = 0; i < LUMA_MAX_DEVICES; i++) if (!devices[i].handle) {
        struct device_entry *d = &devices[i]; memset(d, 0, sizeof(*d)); d->handle = *out; d->physical = phys; d->gdpa = gdpa;
#define LOAD(name, field) d->field = (PFN_##name)gdpa(*out, #name)
        LOAD(vkDestroyDevice, destroy_device); LOAD(vkGetDeviceQueue, get_queue); LOAD(vkGetDeviceQueue2, get_queue2);
        LOAD(vkCreateSwapchainKHR, create_swapchain); LOAD(vkDestroySwapchainKHR, destroy_swapchain); LOAD(vkGetSwapchainImagesKHR, get_swapchain_images);
        LOAD(vkQueuePresentKHR, queue_present); LOAD(vkQueueSubmit, queue_submit); LOAD(vkCreateCommandPool, create_command_pool); LOAD(vkDestroyCommandPool, destroy_command_pool);
        LOAD(vkAllocateCommandBuffers, allocate_command_buffers); LOAD(vkFreeCommandBuffers, free_command_buffers); LOAD(vkBeginCommandBuffer, begin_command_buffer);
        LOAD(vkEndCommandBuffer, end_command_buffer); LOAD(vkResetCommandBuffer, reset_command_buffer); LOAD(vkCmdPipelineBarrier, cmd_pipeline_barrier); LOAD(vkCmdCopyImage, cmd_copy_image);
        LOAD(vkCreateImage, create_image); LOAD(vkDestroyImage, destroy_image); LOAD(vkGetImageMemoryRequirements, get_image_memory_requirements);
        LOAD(vkGetImageSubresourceLayout, get_image_subresource_layout);
        LOAD(vkGetImageDrmFormatModifierPropertiesEXT, get_image_modifier_properties);
        LOAD(vkAllocateMemory, allocate_memory); LOAD(vkFreeMemory, free_memory); LOAD(vkBindImageMemory, bind_image_memory); LOAD(vkCreateSemaphore, create_semaphore);
        LOAD(vkDestroySemaphore, destroy_semaphore); LOAD(vkGetMemoryFdKHR, get_memory_fd); LOAD(vkGetSemaphoreFdKHR, get_semaphore_fd);
#undef LOAD
        bool have_memory_fd = false, have_dma_buf = false, have_semaphore_fd = false,
             have_modifier = false;
        for (uint32_t e = 0; e < forwarded.enabledExtensionCount; e++) {
            const char *name = forwarded.ppEnabledExtensionNames[e];
            have_memory_fd |= strcmp(name, VK_KHR_EXTERNAL_MEMORY_FD_EXTENSION_NAME) == 0;
            have_dma_buf |= strcmp(name, VK_EXT_EXTERNAL_MEMORY_DMA_BUF_EXTENSION_NAME) == 0;
            have_semaphore_fd |= strcmp(name, VK_KHR_EXTERNAL_SEMAPHORE_FD_EXTENSION_NAME) == 0;
            have_modifier |= strcmp(name, VK_EXT_IMAGE_DRM_FORMAT_MODIFIER_EXTENSION_NAME) == 0;
        }
        d->external_export_enabled = have_memory_fd && have_dma_buf && have_semaphore_fd;
        d->drm_modifier_enabled = have_modifier;
        capture_debug("device export extensions: memory_fd=%d dma_buf=%d semaphore_fd=%d modifier=%d; functions memory=%d semaphore=%d",
                      have_memory_fd, have_dma_buf, have_semaphore_fd, have_modifier,
                      d->get_memory_fd != NULL, d->get_semaphore_fd != NULL);
        PFN_vkGetPhysicalDeviceMemoryProperties mem =
            (PFN_vkGetPhysicalDeviceMemoryProperties)gipa(
                capture_instance, "vkGetPhysicalDeviceMemoryProperties");
        if (mem) mem(phys, &d->memory);
        break;
    }
    pthread_mutex_unlock(&lock);
    free(enabled);
    return r;
}
static void remember_queue(VkDevice device, uint32_t family, VkQueue queue) {
    pthread_mutex_lock(&lock); struct device_entry *d = find_device(device);
    for (unsigned i = 0; d && i < LUMA_MAX_QUEUES; i++) if (!queues[i].handle || queues[i].handle == queue) {
        queues[i].handle = queue; queues[i].device = d; queues[i].family = family;
        if (!queues[i].command_pool && d->create_command_pool) {
            VkCommandPoolCreateInfo pool = { .sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO,
                .flags = VK_COMMAND_POOL_CREATE_TRANSIENT_BIT | VK_COMMAND_POOL_CREATE_RESET_COMMAND_BUFFER_BIT,
                .queueFamilyIndex = family };
            (void)d->create_command_pool(d->handle, &pool, NULL, &queues[i].command_pool);
        }
        break;
    }
    pthread_mutex_unlock(&lock);
}
VKAPI_ATTR void VKAPI_CALL vkGetDeviceQueue(VkDevice d, uint32_t family, uint32_t index, VkQueue *out) {
    struct device_entry *entry = find_device(d); if (!entry || !entry->get_queue) return;
    entry->get_queue(d, family, index, out); if (out && *out) remember_queue(d, family, *out);
}
VKAPI_ATTR void VKAPI_CALL vkGetDeviceQueue2(VkDevice d, const VkDeviceQueueInfo2 *info, VkQueue *out) {
    struct device_entry *entry = find_device(d); if (!entry || !entry->get_queue2) return;
    entry->get_queue2(d, info, out); if (out && *out) remember_queue(d, info->queueFamilyIndex, *out);
}
VKAPI_ATTR VkResult VKAPI_CALL vkCreateSwapchainKHR(VkDevice d, const VkSwapchainCreateInfoKHR *info, const VkAllocationCallbacks *alloc, VkSwapchainKHR *out) {
    struct device_entry *entry = find_device(d); if (!entry || !entry->create_swapchain) return VK_ERROR_INITIALIZATION_FAILED;
    VkSwapchainCreateInfoKHR forwarded = *info;
    PFN_vkGetPhysicalDeviceSurfaceCapabilitiesKHR get_capabilities = instance_gipa
        ? (PFN_vkGetPhysicalDeviceSurfaceCapabilitiesKHR)instance_gipa(
              capture_instance, "vkGetPhysicalDeviceSurfaceCapabilitiesKHR")
        : NULL;
    VkSurfaceCapabilitiesKHR capabilities;
    if (get_capabilities &&
        get_capabilities(entry->physical, info->surface, &capabilities) == VK_SUCCESS &&
        (capabilities.supportedUsageFlags & VK_IMAGE_USAGE_TRANSFER_SRC_BIT) != 0) {
        forwarded.imageUsage |= VK_IMAGE_USAGE_TRANSFER_SRC_BIT;
    }
    VkResult r = entry->create_swapchain(d, &forwarded, alloc, out); if (r != VK_SUCCESS) return r;
    pthread_mutex_lock(&lock);
    for (unsigned i = 0; i < LUMA_MAX_SWAPCHAINS; i++) if (!swaps[i].handle) {
        struct swapchain_entry *s = &swaps[i]; memset(s, 0, sizeof(*s));
        s->handle = *out; s->identity = next_swap_identity++; s->device = entry;
        s->extent = info->imageExtent; s->format = info->imageFormat;
        for (unsigned slot = 0; slot < LUMA_RING; ++slot) {
            s->slot[slot].dmabuf_fd = -1;
            s->slot[slot].fence_fd = -1;
        }
        uint32_t count = 16; if (entry->get_swapchain_images(d, *out, &count, s->images) == VK_SUCCESS) {
            s->image_count = count;
            if ((forwarded.imageUsage & VK_IMAGE_USAGE_TRANSFER_SRC_BIT) != 0) setup_swap(s);
        } break;
    }
    pthread_mutex_unlock(&lock);
    return r;
}
VKAPI_ATTR void VKAPI_CALL vkDestroySwapchainKHR(VkDevice device, VkSwapchainKHR swapchain,
                                                  const VkAllocationCallbacks *allocator) {
    struct device_entry *entry = find_device(device);
    PFN_vkDestroySwapchainKHR next = entry ? entry->destroy_swapchain : NULL;
    struct swapchain_entry *swap = find_swap(swapchain);
    if (swap) release_swap(swap);
    if (next) next(device, swapchain, allocator);
}
VKAPI_ATTR void VKAPI_CALL vkDestroyDevice(VkDevice device,
                                            const VkAllocationCallbacks *allocator) {
    struct device_entry *entry = find_device(device);
    PFN_vkDestroyDevice next = entry ? entry->destroy_device : NULL;
    for (unsigned index = 0; index < LUMA_MAX_SWAPCHAINS; ++index)
        if (swaps[index].device == entry) release_swap(&swaps[index]);
    for (unsigned index = 0; index < LUMA_MAX_QUEUES; ++index) {
        if (queues[index].device != entry) continue;
        if (queues[index].command_pool && entry && entry->destroy_command_pool)
            entry->destroy_command_pool(device, queues[index].command_pool, NULL);
        memset(&queues[index], 0, sizeof(queues[index]));
    }
    if (entry) memset(entry, 0, sizeof(*entry));
    if (next) next(device, allocator);
}
VKAPI_ATTR VkResult VKAPI_CALL vkQueuePresentKHR(VkQueue queue, const VkPresentInfoKHR *info) {
    struct queue_entry *q = find_queue(queue); if (!q || !q->device || !q->device->queue_present) return VK_ERROR_DEVICE_LOST;
    drain_acks();
    for (unsigned swap = 0; swap < LUMA_MAX_SWAPCHAINS; swap++)
        for (unsigned slot = 0; slot < LUMA_RING; slot++)
            if (swaps[swap].slot[slot].busy && !swaps[swap].slot[slot].sent)
                emit_slot(&swaps[swap], slot);
    /* A capture submission replaces the app's wait semaphores with its own
     * signal semaphore before forwarding the actual present. */
    if (!info || info->swapchainCount != 1 || info->waitSemaphoreCount > 32 || open_export_socket() < 0) return q->device->queue_present(queue, info);
    struct swapchain_entry *s = find_swap(info->pSwapchains[0]);
    uint32_t index = info->pImageIndices[0];
    if (!s || !s->export_ready || index >= s->image_count) return q->device->queue_present(queue, info);
    struct capture_slot *slot = NULL; unsigned slot_index = 0;
    for (unsigned n = 0; n < LUMA_RING; n++) { unsigned i = (s->cursor + n) % LUMA_RING; if (!s->slot[i].busy) { slot = &s->slot[i]; slot_index = i; break; } }
    if (!slot || !submit_capture(q, s, index, info, slot)) return q->device->queue_present(queue, info);
    s->cursor = (slot_index + 1) % LUMA_RING; slot->busy = true; slot->sent = false;
    slot->generation++; slot->pts_ns = monotonic_ns();
    VkSemaphore replacement = slot->present_ready;
    VkPresentInfoKHR forwarded = *info; forwarded.waitSemaphoreCount = 1; forwarded.pWaitSemaphores = &replacement;
    VkResult result = q->device->queue_present(queue, &forwarded);
    VkSemaphoreGetFdInfoKHR fence = { .sType = VK_STRUCTURE_TYPE_SEMAPHORE_GET_FD_INFO_KHR, .semaphore = slot->capture_ready,
        .handleType = VK_EXTERNAL_SEMAPHORE_HANDLE_TYPE_SYNC_FD_BIT };
    if (q->device->get_semaphore_fd(q->device->handle, &fence, &slot->fence_fd) == VK_SUCCESS) emit_slot(s, slot_index);
    return result;
}
VKAPI_ATTR PFN_vkVoidFunction VKAPI_CALL vkGetInstanceProcAddr(VkInstance instance, const char *name) {
    if (!name) return NULL;
    if (!strcmp(name, "vkCreateInstance")) return (PFN_vkVoidFunction)vkCreateInstance;
    if (!strcmp(name, "vkCreateDevice")) return (PFN_vkVoidFunction)vkCreateDevice;
    /* Applications may request device commands through either resolver. */
    if (!strcmp(name, "vkGetDeviceQueue")) return (PFN_vkVoidFunction)vkGetDeviceQueue;
    if (!strcmp(name, "vkGetDeviceQueue2")) return (PFN_vkVoidFunction)vkGetDeviceQueue2;
    if (!strcmp(name, "vkCreateSwapchainKHR")) return (PFN_vkVoidFunction)vkCreateSwapchainKHR;
    if (!strcmp(name, "vkDestroySwapchainKHR")) return (PFN_vkVoidFunction)vkDestroySwapchainKHR;
    if (!strcmp(name, "vkQueuePresentKHR")) return (PFN_vkVoidFunction)vkQueuePresentKHR;
    if (!strcmp(name, "vkDestroyDevice")) return (PFN_vkVoidFunction)vkDestroyDevice;
    return instance_gipa ? instance_gipa(instance, name) : NULL;
}
VKAPI_ATTR PFN_vkVoidFunction VKAPI_CALL vkGetDeviceProcAddr(VkDevice d, const char *name) {
    if (!name) return NULL;
    if (!strcmp(name, "vkGetDeviceQueue")) return (PFN_vkVoidFunction)vkGetDeviceQueue;
    if (!strcmp(name, "vkGetDeviceQueue2")) return (PFN_vkVoidFunction)vkGetDeviceQueue2;
    if (!strcmp(name, "vkCreateSwapchainKHR")) return (PFN_vkVoidFunction)vkCreateSwapchainKHR;
    if (!strcmp(name, "vkDestroySwapchainKHR")) return (PFN_vkVoidFunction)vkDestroySwapchainKHR;
    if (!strcmp(name, "vkQueuePresentKHR")) return (PFN_vkVoidFunction)vkQueuePresentKHR;
    if (!strcmp(name, "vkDestroyDevice")) return (PFN_vkVoidFunction)vkDestroyDevice;
    struct device_entry *entry = find_device(d); return entry ? entry->gdpa(d, name) : NULL;
}
VKAPI_ATTR VkResult VKAPI_CALL vkNegotiateLoaderLayerInterfaceVersion(VkNegotiateLayerInterface *v) {
    if (!v || v->sType != LAYER_NEGOTIATE_INTERFACE_STRUCT) return VK_ERROR_INITIALIZATION_FAILED;
    if (v->loaderLayerInterfaceVersion > 2) v->loaderLayerInterfaceVersion = 2;
    v->pfnGetInstanceProcAddr = vkGetInstanceProcAddr;
    v->pfnGetDeviceProcAddr = vkGetDeviceProcAddr;
    v->pfnGetPhysicalDeviceProcAddr = NULL;
    return VK_SUCCESS;
}
