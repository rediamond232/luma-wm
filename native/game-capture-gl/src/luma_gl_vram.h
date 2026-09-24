#pragma once
#include <vulkan/vulkan.h>
#include <vector>
#include <cstring>

// Vulkan supplies exportable allocations only. It submits no GPU work and
// never touches the game context or swapchain.
struct LumaVram {
    VkInstance instance{};
    VkPhysicalDevice physical{};
    VkDevice device{};
    PFN_vkGetMemoryFdKHR get_fd{};
    PFN_vkGetSemaphoreFdKHR get_semaphore_fd{};
    bool initialize(const unsigned char uuid[16]) {
        VkApplicationInfo app{}; app.sType = VK_STRUCTURE_TYPE_APPLICATION_INFO; app.apiVersion = VK_API_VERSION_1_1;
        VkInstanceCreateInfo info{}; info.sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO; info.pApplicationInfo = &app;
        if (vkCreateInstance(&info, nullptr, &instance) != VK_SUCCESS) return false;
        uint32_t count{}; vkEnumeratePhysicalDevices(instance, &count, nullptr);
        std::vector<VkPhysicalDevice> devices(count);
        vkEnumeratePhysicalDevices(instance, &count, devices.data());
        for (auto candidate : devices) {
            VkPhysicalDeviceIDProperties id{}; id.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_ID_PROPERTIES;
            VkPhysicalDeviceProperties2 props{}; props.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_PROPERTIES_2; props.pNext = &id;
            vkGetPhysicalDeviceProperties2(candidate, &props);
            if (memcmp(id.deviceUUID, uuid, 16) == 0) { physical = candidate; break; }
        }
        if (!physical) return false;
        vkGetPhysicalDeviceQueueFamilyProperties(physical, &count, nullptr);
        std::vector<VkQueueFamilyProperties> queues(count);
        vkGetPhysicalDeviceQueueFamilyProperties(physical, &count, queues.data());
        uint32_t family = 0;
        while (family < count && !(queues[family].queueFlags & VK_QUEUE_GRAPHICS_BIT)) ++family;
        if (family == count) return false;
        const float priority = 0;
        VkDeviceQueueCreateInfo queue{}; queue.sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO;
        queue.queueFamilyIndex = family; queue.queueCount = 1; queue.pQueuePriorities = &priority;
        const char *extensions[] = {VK_KHR_EXTERNAL_MEMORY_FD_EXTENSION_NAME, VK_KHR_EXTERNAL_SEMAPHORE_FD_EXTENSION_NAME};
        VkDeviceCreateInfo create{}; create.sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO;
        create.queueCreateInfoCount = 1; create.pQueueCreateInfos = &queue;
        create.enabledExtensionCount = 2; create.ppEnabledExtensionNames = extensions;
        if (vkCreateDevice(physical, &create, nullptr, &device) != VK_SUCCESS) return false;
        get_fd = reinterpret_cast<PFN_vkGetMemoryFdKHR>(vkGetDeviceProcAddr(device, "vkGetMemoryFdKHR"));
        get_semaphore_fd = reinterpret_cast<PFN_vkGetSemaphoreFdKHR>(vkGetDeviceProcAddr(device, "vkGetSemaphoreFdKHR"));
        return get_fd && get_semaphore_fd;
    }
    bool semaphore(VkSemaphore &sem, int &fd) {
        VkExportSemaphoreCreateInfo export_info{}; export_info.sType = VK_STRUCTURE_TYPE_EXPORT_SEMAPHORE_CREATE_INFO;
        export_info.handleTypes = VK_EXTERNAL_SEMAPHORE_HANDLE_TYPE_OPAQUE_FD_BIT;
        VkSemaphoreCreateInfo create{}; create.sType = VK_STRUCTURE_TYPE_SEMAPHORE_CREATE_INFO; create.pNext = &export_info;
        if (vkCreateSemaphore(device, &create, nullptr, &sem) != VK_SUCCESS) return false;
        VkSemaphoreGetFdInfoKHR get{}; get.sType = VK_STRUCTURE_TYPE_SEMAPHORE_GET_FD_INFO_KHR;
        get.semaphore = sem; get.handleType = VK_EXTERNAL_SEMAPHORE_HANDLE_TYPE_OPAQUE_FD_BIT;
        return get_semaphore_fd(device, &get, &fd) == VK_SUCCESS;
    }
    bool allocate(uint32_t width, uint32_t height, VkImage &image, VkDeviceMemory &memory, uint64_t &size, int &fd) {
        VkExternalMemoryImageCreateInfo external{}; external.sType = VK_STRUCTURE_TYPE_EXTERNAL_MEMORY_IMAGE_CREATE_INFO;
        external.handleTypes = VK_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_FD_BIT;
        VkImageCreateInfo create{}; create.sType = VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO; create.pNext = &external;
        create.imageType = VK_IMAGE_TYPE_2D; create.format = VK_FORMAT_R8G8B8A8_UNORM;
        create.extent = {width, height, 1}; create.mipLevels = 1; create.arrayLayers = 1;
        create.samples = VK_SAMPLE_COUNT_1_BIT; create.tiling = VK_IMAGE_TILING_OPTIMAL;
        create.usage = VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT | VK_IMAGE_USAGE_TRANSFER_SRC_BIT | VK_IMAGE_USAGE_TRANSFER_DST_BIT | VK_IMAGE_USAGE_SAMPLED_BIT;
        if (vkCreateImage(device, &create, nullptr, &image) != VK_SUCCESS) return false;
        VkMemoryRequirements req{}; vkGetImageMemoryRequirements(device, image, &req);
        VkPhysicalDeviceMemoryProperties props{}; vkGetPhysicalDeviceMemoryProperties(physical, &props);
        uint32_t type = 0;
        while (type < props.memoryTypeCount && (!(req.memoryTypeBits & (1u << type)) || !(props.memoryTypes[type].propertyFlags & VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT))) ++type;
        if (type == props.memoryTypeCount) return false;
        VkMemoryDedicatedAllocateInfo dedicated{}; dedicated.sType = VK_STRUCTURE_TYPE_MEMORY_DEDICATED_ALLOCATE_INFO; dedicated.image = image;
        VkExportMemoryAllocateInfo export_info{}; export_info.sType = VK_STRUCTURE_TYPE_EXPORT_MEMORY_ALLOCATE_INFO;
        export_info.pNext = &dedicated; export_info.handleTypes = VK_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_FD_BIT;
        VkMemoryAllocateInfo allocation{}; allocation.sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO;
        allocation.pNext = &export_info; allocation.allocationSize = req.size; allocation.memoryTypeIndex = type;
        if (vkAllocateMemory(device, &allocation, nullptr, &memory) != VK_SUCCESS || vkBindImageMemory(device, image, memory, 0) != VK_SUCCESS) return false;
        size = req.size;
        VkMemoryGetFdInfoKHR get{}; get.sType = VK_STRUCTURE_TYPE_MEMORY_GET_FD_INFO_KHR;
        get.memory = memory; get.handleType = VK_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_FD_BIT;
        return get_fd(device, &get, &fd) == VK_SUCCESS;
    }
    ~LumaVram() {
        if (device) vkDestroyDevice(device, nullptr);
        if (instance) vkDestroyInstance(instance, nullptr);
    }
};
