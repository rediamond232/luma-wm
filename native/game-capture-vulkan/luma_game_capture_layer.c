/*
 * Luma Vulkan game-capture layer.
 *
 * This deliberately observes present calls only.  It does not read or alter
 * game memory, inject into an already-running process, or transfer pixels.
 * A future frame-export backend must preserve the synchronization contract of
 * vkQueuePresentKHR before it can be added here.
 */
#define _POSIX_C_SOURCE 200809L
#include <vulkan/vulkan.h>
#include <vulkan/vk_layer.h>

#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <time.h>
#include <unistd.h>

#define LUMA_MAX_INSTANCES 16
#define LUMA_MAX_DEVICES 16
#define LUMA_MAX_QUEUES 64
#define LUMA_EVENT_MAGIC 0x4c474331u /* LGC1 */

struct instance_entry { VkInstance handle; PFN_vkGetInstanceProcAddr gipa; };
struct device_entry {
    VkDevice handle;
    PFN_vkGetDeviceProcAddr gdpa;
    PFN_vkDestroyDevice destroy_device;
    PFN_vkGetDeviceQueue get_queue;
    PFN_vkGetDeviceQueue2 get_queue2;
};
struct queue_entry { VkQueue handle; PFN_vkQueuePresentKHR present; };
struct present_event {
    uint32_t magic;
    uint16_t version;
    uint16_t size;
    uint64_t monotonic_ns;
    uint64_t queue;
    uint32_t swapchain_count;
    uint32_t result;
};

static pthread_mutex_t table_lock = PTHREAD_MUTEX_INITIALIZER;
static struct instance_entry instances[LUMA_MAX_INSTANCES];
static struct device_entry devices[LUMA_MAX_DEVICES];
static struct queue_entry queues[LUMA_MAX_QUEUES];
static int event_socket = -2; /* -2: uninitialized, -1: disabled/unavailable */

static uint64_t monotonic_ns(void) {
    struct timespec now;
    clock_gettime(CLOCK_MONOTONIC, &now);
    return (uint64_t)now.tv_sec * 1000000000ull + (uint64_t)now.tv_nsec;
}

static void emit_present(VkQueue queue, uint32_t swapchain_count, VkResult result) {
    if (event_socket == -2) {
        const char *path = getenv("LUMA_GAME_CAPTURE_SOCKET");
        if (!path || !*path || strlen(path) >= sizeof(((struct sockaddr_un *)0)->sun_path)) {
            event_socket = -1;
        } else {
            int fd = socket(AF_UNIX, SOCK_DGRAM | SOCK_NONBLOCK | SOCK_CLOEXEC, 0);
            if (fd < 0) {
                event_socket = -1;
            } else {
                struct sockaddr_un address;
                memset(&address, 0, sizeof(address));
                address.sun_family = AF_UNIX;
                memcpy(address.sun_path, path, strlen(path) + 1);
                if (connect(fd, (const struct sockaddr *)&address, sizeof(address)) != 0) {
                    close(fd);
                    event_socket = -1;
                } else {
                    event_socket = fd;
                }
            }
        }
    }
    if (event_socket < 0) return;
    struct present_event event = {
        .magic = LUMA_EVENT_MAGIC, .version = 1, .size = sizeof(event),
        .monotonic_ns = monotonic_ns(), .queue = (uint64_t)(uintptr_t)queue,
        .swapchain_count = swapchain_count, .result = (uint32_t)result,
    };
    /* A full socket must never slow down a game's render thread. */
    (void)send(event_socket, &event, sizeof(event), MSG_DONTWAIT | MSG_NOSIGNAL);
}

static VkLayerInstanceCreateInfo *find_instance_link(const void *next) {
    for (const VkBaseInStructure *node = next; node; node = node->pNext) {
        if (node->sType == VK_STRUCTURE_TYPE_LOADER_INSTANCE_CREATE_INFO) {
            VkLayerInstanceCreateInfo *info = (VkLayerInstanceCreateInfo *)(uintptr_t)node;
            if (info->function == VK_LAYER_LINK_INFO) return info;
        }
    }
    return NULL;
}

static VkLayerDeviceCreateInfo *find_device_link(const void *next) {
    for (const VkBaseInStructure *node = next; node; node = node->pNext) {
        if (node->sType == VK_STRUCTURE_TYPE_LOADER_DEVICE_CREATE_INFO) {
            VkLayerDeviceCreateInfo *info = (VkLayerDeviceCreateInfo *)(uintptr_t)node;
            if (info->function == VK_LAYER_LINK_INFO) return info;
        }
    }
    return NULL;
}

static PFN_vkGetInstanceProcAddr instance_gipa(VkInstance instance) {
    for (size_t i = 0; i < LUMA_MAX_INSTANCES; ++i)
        if (instances[i].handle == instance) return instances[i].gipa;
    return NULL;
}
static struct device_entry *device_for(VkDevice device) {
    for (size_t i = 0; i < LUMA_MAX_DEVICES; ++i)
        if (devices[i].handle == device) return &devices[i];
    return NULL;
}
static PFN_vkQueuePresentKHR present_for(VkQueue queue) {
    for (size_t i = 0; i < LUMA_MAX_QUEUES; ++i)
        if (queues[i].handle == queue) return queues[i].present;
    return NULL;
}
static void remember_queue(VkQueue queue, PFN_vkQueuePresentKHR present) {
    if (!queue || !present) return;
    pthread_mutex_lock(&table_lock);
    for (size_t i = 0; i < LUMA_MAX_QUEUES; ++i) {
        if (!queues[i].handle || queues[i].handle == queue) {
            queues[i] = (struct queue_entry){ queue, present };
            break;
        }
    }
    pthread_mutex_unlock(&table_lock);
}

VKAPI_ATTR VkResult VKAPI_CALL vkCreateInstance(const VkInstanceCreateInfo *create_info,
                                                 const VkAllocationCallbacks *allocator,
                                                 VkInstance *instance) {
    VkLayerInstanceCreateInfo *link = find_instance_link(create_info ? create_info->pNext : NULL);
    if (!link || !link->u.pLayerInfo) return VK_ERROR_INITIALIZATION_FAILED;
    PFN_vkGetInstanceProcAddr next_gipa = link->u.pLayerInfo->pfnNextGetInstanceProcAddr;
    link->u.pLayerInfo = link->u.pLayerInfo->pNext;
    PFN_vkCreateInstance next_create = (PFN_vkCreateInstance)next_gipa(NULL, "vkCreateInstance");
    VkResult result = next_create(create_info, allocator, instance);
    if (result == VK_SUCCESS && instance && *instance) {
        pthread_mutex_lock(&table_lock);
        for (size_t i = 0; i < LUMA_MAX_INSTANCES; ++i) if (!instances[i].handle) {
            instances[i] = (struct instance_entry){ *instance, next_gipa }; break;
        }
        pthread_mutex_unlock(&table_lock);
    }
    return result;
}

VKAPI_ATTR VkResult VKAPI_CALL vkCreateDevice(VkPhysicalDevice physical_device,
                                               const VkDeviceCreateInfo *create_info,
                                               const VkAllocationCallbacks *allocator,
                                               VkDevice *device) {
    VkLayerDeviceCreateInfo *link = find_device_link(create_info ? create_info->pNext : NULL);
    if (!link || !link->u.pLayerInfo) return VK_ERROR_INITIALIZATION_FAILED;
    PFN_vkGetInstanceProcAddr next_gipa = link->u.pLayerInfo->pfnNextGetInstanceProcAddr;
    PFN_vkGetDeviceProcAddr next_gdpa = link->u.pLayerInfo->pfnNextGetDeviceProcAddr;
    link->u.pLayerInfo = link->u.pLayerInfo->pNext;
    PFN_vkCreateDevice next_create = (PFN_vkCreateDevice)next_gipa(NULL, "vkCreateDevice");
    VkResult result = next_create(physical_device, create_info, allocator, device);
    if (result == VK_SUCCESS && device && *device) {
        struct device_entry entry = {
            .handle = *device, .gdpa = next_gdpa,
            .destroy_device = (PFN_vkDestroyDevice)next_gdpa(*device, "vkDestroyDevice"),
            .get_queue = (PFN_vkGetDeviceQueue)next_gdpa(*device, "vkGetDeviceQueue"),
            .get_queue2 = (PFN_vkGetDeviceQueue2)next_gdpa(*device, "vkGetDeviceQueue2"),
        };
        pthread_mutex_lock(&table_lock);
        for (size_t i = 0; i < LUMA_MAX_DEVICES; ++i) if (!devices[i].handle) { devices[i] = entry; break; }
        pthread_mutex_unlock(&table_lock);
    }
    return result;
}

VKAPI_ATTR void VKAPI_CALL vkGetDeviceQueue(VkDevice device, uint32_t family, uint32_t index, VkQueue *queue) {
    pthread_mutex_lock(&table_lock); struct device_entry *entry = device_for(device); PFN_vkGetDeviceQueue next = entry ? entry->get_queue : NULL; pthread_mutex_unlock(&table_lock);
    if (!next) return;
    next(device, family, index, queue);
    if (queue && *queue) {
        pthread_mutex_lock(&table_lock); entry = device_for(device); PFN_vkQueuePresentKHR present = entry ? (PFN_vkQueuePresentKHR)entry->gdpa(device, "vkQueuePresentKHR") : NULL; pthread_mutex_unlock(&table_lock);
        remember_queue(*queue, present);
    }
}

VKAPI_ATTR void VKAPI_CALL vkGetDeviceQueue2(VkDevice device, const VkDeviceQueueInfo2 *info, VkQueue *queue) {
    pthread_mutex_lock(&table_lock); struct device_entry *entry = device_for(device); PFN_vkGetDeviceQueue2 next = entry ? entry->get_queue2 : NULL; pthread_mutex_unlock(&table_lock);
    if (!next) return;
    next(device, info, queue);
    if (queue && *queue) {
        pthread_mutex_lock(&table_lock); entry = device_for(device); PFN_vkQueuePresentKHR present = entry ? (PFN_vkQueuePresentKHR)entry->gdpa(device, "vkQueuePresentKHR") : NULL; pthread_mutex_unlock(&table_lock);
        remember_queue(*queue, present);
    }
}

VKAPI_ATTR VkResult VKAPI_CALL vkQueuePresentKHR(VkQueue queue, const VkPresentInfoKHR *present_info) {
    pthread_mutex_lock(&table_lock); PFN_vkQueuePresentKHR next = present_for(queue); pthread_mutex_unlock(&table_lock);
    if (!next) return VK_ERROR_DEVICE_LOST;
    VkResult result = next(queue, present_info);
    emit_present(queue, present_info ? present_info->swapchainCount : 0, result);
    return result;
}

VKAPI_ATTR void VKAPI_CALL vkDestroyDevice(VkDevice device, const VkAllocationCallbacks *allocator) {
    pthread_mutex_lock(&table_lock); struct device_entry *entry = device_for(device); PFN_vkDestroyDevice next = entry ? entry->destroy_device : NULL;
    if (entry) memset(entry, 0, sizeof(*entry));
    for (size_t i = 0; i < LUMA_MAX_QUEUES; ++i) queues[i].handle = VK_NULL_HANDLE;
    pthread_mutex_unlock(&table_lock);
    if (next) next(device, allocator);
}

VKAPI_ATTR PFN_vkVoidFunction VKAPI_CALL vkGetInstanceProcAddr(VkInstance instance, const char *name) {
    if (!name) return NULL;
    if (!strcmp(name, "vkCreateInstance")) return (PFN_vkVoidFunction)vkCreateInstance;
    if (!strcmp(name, "vkCreateDevice")) return (PFN_vkVoidFunction)vkCreateDevice;
    PFN_vkGetInstanceProcAddr next = instance_gipa(instance);
    return next ? next(instance, name) : NULL;
}

VKAPI_ATTR PFN_vkVoidFunction VKAPI_CALL vkGetDeviceProcAddr(VkDevice device, const char *name) {
    if (!name) return NULL;
    if (!strcmp(name, "vkQueuePresentKHR")) return (PFN_vkVoidFunction)vkQueuePresentKHR;
    if (!strcmp(name, "vkGetDeviceQueue")) return (PFN_vkVoidFunction)vkGetDeviceQueue;
    if (!strcmp(name, "vkGetDeviceQueue2")) return (PFN_vkVoidFunction)vkGetDeviceQueue2;
    if (!strcmp(name, "vkDestroyDevice")) return (PFN_vkVoidFunction)vkDestroyDevice;
    pthread_mutex_lock(&table_lock); struct device_entry *entry = device_for(device); PFN_vkGetDeviceProcAddr next = entry ? entry->gdpa : NULL; pthread_mutex_unlock(&table_lock);
    return next ? next(device, name) : NULL;
}

VKAPI_ATTR VkResult VKAPI_CALL vkNegotiateLoaderLayerInterfaceVersion(VkNegotiateLayerInterface *version) {
    if (!version || version->sType != LAYER_NEGOTIATE_INTERFACE_STRUCT) return VK_ERROR_INITIALIZATION_FAILED;
    if (version->loaderLayerInterfaceVersion > 2) version->loaderLayerInterfaceVersion = 2;
    return VK_SUCCESS;
}
