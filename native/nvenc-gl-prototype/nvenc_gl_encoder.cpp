#include "nvenc_gl_encoder.hpp"

#include <EGL/egl.h>
#include <ffnvcodec/nvEncodeAPI.h>
#include <dlfcn.h>

#include <array>
#include <cstring>
#include <deque>
#include <string>
#include <utility>
#include <vector>

namespace luma::nvenc_gl {
namespace {

using CreateInstanceFn = NVENCSTATUS (NVENCAPI *)(NV_ENCODE_API_FUNCTION_LIST*);

const char* status_name(NVENCSTATUS s) {
    switch (s) {
    case NV_ENC_SUCCESS: return "NV_ENC_SUCCESS";
    case NV_ENC_ERR_LOCK_BUSY: return "NV_ENC_ERR_LOCK_BUSY";
    case NV_ENC_ERR_NEED_MORE_INPUT: return "NV_ENC_ERR_NEED_MORE_INPUT";
    case NV_ENC_ERR_UNSUPPORTED_DEVICE: return "NV_ENC_ERR_UNSUPPORTED_DEVICE";
    case NV_ENC_ERR_INVALID_ENCODERDEVICE: return "NV_ENC_ERR_INVALID_ENCODERDEVICE";
    case NV_ENC_ERR_UNSUPPORTED_PARAM: return "NV_ENC_ERR_UNSUPPORTED_PARAM";
    default: return "NVENC error";
    }
}

bool failed(NVENCSTATUS s, std::string& out, const char* action) {
    if (s == NV_ENC_SUCCESS) return false;
    out = std::string(action) + ": " + status_name(s) + " (" + std::to_string(s) + ')';
    return true;
}

} // namespace

struct Encoder::Impl {
    struct Slot {
        NV_ENC_REGISTERED_PTR registered = nullptr;
        NV_ENC_INPUT_PTR mapped = nullptr;
        NV_ENC_OUTPUT_PTR bitstream = nullptr;
        bool in_flight = false;
    };

    void* library = nullptr;
    void* encoder = nullptr;
    NV_ENCODE_API_FUNCTION_LIST api{};
    Config config{};
    std::vector<Slot> slots;
    std::deque<std::uint32_t> pending;
    std::string last_error;
};

Encoder::Encoder() : impl_(std::make_unique<Impl>()) {}
Encoder::~Encoder() { const char* ignored = nullptr; shutdown(&ignored); }

bool Encoder::initialize(void* egl_context, const Config& config, const GLuint* textures, const char** error) {
    auto& p = *impl_;
    NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS open{};
    NV_ENC_INITIALIZE_PARAMS init{};
    if (p.encoder || !egl_context || !textures || config.width == 0 || config.height == 0 ||
        config.fps == 0 || config.texture_count < 2 || !config.on_packet) {
        p.last_error = "invalid OpenGL NVENC configuration";
        if (error) *error = p.last_error.c_str();
        return false;
    }

    p.library = dlopen("libnvidia-encode.so.1", RTLD_NOW | RTLD_LOCAL);
    if (!p.library) {
        p.last_error = std::string("dlopen libnvidia-encode.so.1: ") + dlerror();
        if (error) *error = p.last_error.c_str();
        return false;
    }
    auto create = reinterpret_cast<CreateInstanceFn>(dlsym(p.library, "NvEncodeAPICreateInstance"));
    if (!create) {
        p.last_error = "NvEncodeAPICreateInstance is missing";
        if (error) *error = p.last_error.c_str();
        return false;
    }
    p.api.version = NV_ENCODE_API_FUNCTION_LIST_VER;
    if (failed(create(&p.api), p.last_error, "NvEncodeAPICreateInstance")) goto fail;

    open.version = NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER;
    open.deviceType = NV_ENC_DEVICE_TYPE_OPENGL;
    open.device = egl_context;
    open.apiVersion = NVENCAPI_VERSION;
    if (failed(p.api.nvEncOpenEncodeSessionEx(&open, &p.encoder), p.last_error, "NvEncOpenEncodeSessionEx(OpenGL)")) goto fail;

    init.version = NV_ENC_INITIALIZE_PARAMS_VER;
    init.encodeGUID = NV_ENC_CODEC_H264_GUID;
    init.presetGUID = NV_ENC_PRESET_P1_GUID; // lowest-latency performance preset
    init.encodeWidth = config.width;
    init.encodeHeight = config.height;
    init.darWidth = config.width;
    init.darHeight = config.height;
    init.frameRateNum = config.fps;
    init.frameRateDen = 1;
    init.enablePTD = 1;
    init.tuningInfo = NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY;
    if (failed(p.api.nvEncInitializeEncoder(p.encoder, &init), p.last_error, "NvEncInitializeEncoder")) goto fail;

    p.config = config;
    p.slots.resize(config.texture_count);
    for (std::uint32_t i = 0; i < config.texture_count; ++i) {
        NV_ENC_INPUT_RESOURCE_OPENGL_TEX gl_texture{};
        gl_texture.texture = textures[i];
        gl_texture.target = 0x0DE1; // GL_TEXTURE_2D; avoid pulling GL headers into ABI header
        NV_ENC_REGISTER_RESOURCE resource{};
        resource.version = NV_ENC_REGISTER_RESOURCE_VER;
        resource.resourceType = NV_ENC_INPUT_RESOURCE_TYPE_OPENGL_TEX;
        resource.width = config.width;
        resource.height = config.height;
        resource.pitch = config.width * 4;
        resource.resourceToRegister = &gl_texture;
        resource.bufferFormat = NV_ENC_BUFFER_FORMAT_ABGR;
        resource.bufferUsage = NV_ENC_INPUT_IMAGE;
        if (failed(p.api.nvEncRegisterResource(p.encoder, &resource), p.last_error, "NvEncRegisterResource(GL_TEXTURE_2D)")) goto fail;
        p.slots[i].registered = resource.registeredResource;

        NV_ENC_CREATE_BITSTREAM_BUFFER bs{};
        bs.version = NV_ENC_CREATE_BITSTREAM_BUFFER_VER;
        if (failed(p.api.nvEncCreateBitstreamBuffer(p.encoder, &bs), p.last_error, "NvEncCreateBitstreamBuffer")) goto fail;
        p.slots[i].bitstream = bs.bitstreamBuffer;
    }
    return true;

fail:
    if (error) *error = p.last_error.c_str();
    shutdown(nullptr);
    return false;
}

bool Encoder::poll(const char** error) {
    auto& p = *impl_;
    while (!p.pending.empty()) {
        const std::uint32_t index = p.pending.front();
        auto& slot = p.slots[index];
        NV_ENC_LOCK_BITSTREAM lock{};
        lock.version = NV_ENC_LOCK_BITSTREAM_VER;
        lock.doNotWait = 1;
        lock.outputBitstream = slot.bitstream;
        const NVENCSTATUS status = p.api.nvEncLockBitstream(p.encoder, &lock);
        if (status == NV_ENC_ERR_LOCK_BUSY) return true;
        if (failed(status, p.last_error, "NvEncLockBitstream")) {
            if (error) *error = p.last_error.c_str();
            return false;
        }
        Packet packet{static_cast<const std::uint8_t*>(lock.bitstreamBufferPtr), lock.bitstreamSizeInBytes,
                      lock.outputTimeStamp, lock.pictureType == NV_ENC_PIC_TYPE_IDR};
        p.config.on_packet(packet);
        if (failed(p.api.nvEncUnlockBitstream(p.encoder, slot.bitstream), p.last_error, "NvEncUnlockBitstream") ||
            failed(p.api.nvEncUnmapInputResource(p.encoder, slot.mapped), p.last_error, "NvEncUnmapInputResource")) {
            if (error) *error = p.last_error.c_str();
            return false;
        }
        slot.mapped = nullptr;
        slot.in_flight = false;
        p.pending.pop_front();
    }
    return true;
}

bool Encoder::submit(std::uint32_t texture_index, std::uint64_t pts, bool force_idr, const char** error) {
    auto& p = *impl_;
    if (!poll(error)) return false;
    if (texture_index >= p.slots.size()) {
        p.last_error = "texture index outside NVENC ring";
        if (error) *error = p.last_error.c_str();
        return false;
    }
    auto& slot = p.slots[texture_index];
    if (slot.in_flight) return false; // intentionally drop rather than delay game present

    NV_ENC_MAP_INPUT_RESOURCE map{};
    map.version = NV_ENC_MAP_INPUT_RESOURCE_VER;
    map.registeredResource = slot.registered;
    if (failed(p.api.nvEncMapInputResource(p.encoder, &map), p.last_error, "NvEncMapInputResource")) {
        if (error) *error = p.last_error.c_str();
        return false;
    }
    slot.mapped = map.mappedResource;
    NV_ENC_PIC_PARAMS pic{};
    pic.version = NV_ENC_PIC_PARAMS_VER;
    pic.inputWidth = p.config.width;
    pic.inputHeight = p.config.height;
    pic.inputPitch = p.config.width;
    pic.inputBuffer = slot.mapped;
    pic.bufferFmt = map.mappedBufferFmt;
    pic.outputBitstream = slot.bitstream;
    pic.pictureStruct = NV_ENC_PIC_STRUCT_FRAME;
    pic.inputTimeStamp = pts;
    pic.inputDuration = 1;
    if (force_idr) pic.encodePicFlags = NV_ENC_PIC_FLAG_FORCEIDR;
    const NVENCSTATUS status = p.api.nvEncEncodePicture(p.encoder, &pic);
    if (status != NV_ENC_SUCCESS && status != NV_ENC_ERR_NEED_MORE_INPUT) {
        p.api.nvEncUnmapInputResource(p.encoder, slot.mapped);
        slot.mapped = nullptr;
        failed(status, p.last_error, "NvEncEncodePicture");
        if (error) *error = p.last_error.c_str();
        return false;
    }
    slot.in_flight = true;
    p.pending.push_back(texture_index);
    return true;
}

bool Encoder::shutdown(const char** error) {
    auto& p = *impl_;
    if (!p.encoder) {
        if (p.library) { dlclose(p.library); p.library = nullptr; }
        return true;
    }
    // Flush must use a null input buffer. Locking is intentionally blocking
    // only on shutdown, never on the hook's present path.
    NV_ENC_PIC_PARAMS eos{};
    eos.version = NV_ENC_PIC_PARAMS_VER;
    eos.encodePicFlags = NV_ENC_PIC_FLAG_EOS;
    p.api.nvEncEncodePicture(p.encoder, &eos);
    for (auto& slot : p.slots) {
        if (slot.mapped) p.api.nvEncUnmapInputResource(p.encoder, slot.mapped);
        if (slot.bitstream) p.api.nvEncDestroyBitstreamBuffer(p.encoder, slot.bitstream);
        if (slot.registered) p.api.nvEncUnregisterResource(p.encoder, slot.registered);
    }
    const NVENCSTATUS status = p.api.nvEncDestroyEncoder(p.encoder);
    p.encoder = nullptr;
    p.slots.clear();
    p.pending.clear();
    if (p.library) { dlclose(p.library); p.library = nullptr; }
    if (failed(status, p.last_error, "NvEncDestroyEncoder")) {
        if (error) *error = p.last_error.c_str();
        return false;
    }
    return true;
}

} // namespace luma::nvenc_gl
