#pragma once
#include <GL/gl.h>
#include <GL/glext.h>
#include <EGL/egl.h>
#include <cstring>
#include <unistd.h>

// Resolve extension dispatch rather than assuming vendor symbols are exported
// by libGL. Both GLX and EGL desktop OpenGL use this external-memory interface.
struct LumaGlMemory {
    PFNGLGENSEMAPHORESEXTPROC gen_semaphore{};
    PFNGLDELETESEMAPHORESEXTPROC delete_semaphore{};
    PFNGLIMPORTSEMAPHOREFDEXTPROC import_semaphore{};
    PFNGLWAITSEMAPHOREEXTPROC wait{};
    PFNGLSIGNALSEMAPHOREEXTPROC signal{};
    PFNGLCREATEMEMORYOBJECTSEXTPROC create{};
    PFNGLDELETEMEMORYOBJECTSEXTPROC destroy{};
    PFNGLMEMORYOBJECTPARAMETERIVEXTPROC parameter{};
    PFNGLIMPORTMEMORYFDEXTPROC import_fd{};
    PFNGLTEXSTORAGEMEM2DEXTPROC storage{};
    PFNGLGETUNSIGNEDBYTEVEXTPROC uuid{};
    template<class T> static T proc(const char *name) {
        auto raw = eglGetProcAddress(name); T fn{};
        static_assert(sizeof(raw) == sizeof(fn)); memcpy(&fn, &raw, sizeof(fn)); return fn;
    }
    bool load() {
        bool supported = false, semaphore_supported = false;
        GLint count{}; glGetIntegerv(GL_NUM_EXTENSIONS, &count);
        for (GLint i = 0; i < count; ++i) {
            if (strcmp(reinterpret_cast<const char *>(glGetStringi(GL_EXTENSIONS, i)), "GL_EXT_memory_object_fd") == 0) supported = true;
            if (strcmp(reinterpret_cast<const char *>(glGetStringi(GL_EXTENSIONS, i)), "GL_EXT_semaphore_fd") == 0) semaphore_supported = true;
        }
        if (!supported || !semaphore_supported) return false;
        gen_semaphore = proc<PFNGLGENSEMAPHORESEXTPROC>("glGenSemaphoresEXT");
        delete_semaphore = proc<PFNGLDELETESEMAPHORESEXTPROC>("glDeleteSemaphoresEXT");
        import_semaphore = proc<PFNGLIMPORTSEMAPHOREFDEXTPROC>("glImportSemaphoreFdEXT");
        wait = proc<PFNGLWAITSEMAPHOREEXTPROC>("glWaitSemaphoreEXT");
        signal = proc<PFNGLSIGNALSEMAPHOREEXTPROC>("glSignalSemaphoreEXT");
        create = proc<PFNGLCREATEMEMORYOBJECTSEXTPROC>("glCreateMemoryObjectsEXT");
        destroy = proc<PFNGLDELETEMEMORYOBJECTSEXTPROC>("glDeleteMemoryObjectsEXT");
        parameter = proc<PFNGLMEMORYOBJECTPARAMETERIVEXTPROC>("glMemoryObjectParameterivEXT");
        import_fd = proc<PFNGLIMPORTMEMORYFDEXTPROC>("glImportMemoryFdEXT");
        storage = proc<PFNGLTEXSTORAGEMEM2DEXTPROC>("glTexStorageMem2DEXT");
        uuid = proc<PFNGLGETUNSIGNEDBYTEVEXTPROC>("glGetUnsignedBytevEXT");
        return create && destroy && parameter && import_fd && storage && uuid && gen_semaphore && delete_semaphore && import_semaphore && wait && signal;
    }
    bool semaphore(int fd, GLuint &sem) {
        const int imported = dup(fd);
        if (imported < 0) return false;
        gen_semaphore(1, &sem); import_semaphore(sem, GL_HANDLE_TYPE_OPAQUE_FD_EXT, imported);
        return sem != 0;
    }
    void acquire(GLuint sem, GLuint tex) {
        const GLenum layout = GL_LAYOUT_GENERAL_EXT;
        wait(sem, 0, nullptr, 1, &tex, &layout);
    }
    void release(GLuint sem, GLuint tex) {
        const GLenum layout = GL_LAYOUT_GENERAL_EXT;
        signal(sem, 0, nullptr, 1, &tex, &layout);
    }
    bool texture(int fd, uint64_t size, unsigned width, unsigned height, GLuint &memory, GLuint &tex) {
        const int imported = dup(fd);
        if (imported < 0) return false;
        create(1, &memory);
        const GLint dedicated = GL_TRUE;
        parameter(memory, GL_DEDICATED_MEMORY_OBJECT_EXT, &dedicated);
        import_fd(memory, size, GL_HANDLE_TYPE_OPAQUE_FD_EXT, imported);
        glGenTextures(1, &tex); glBindTexture(GL_TEXTURE_2D, tex);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_TILING_EXT, GL_OPTIMAL_TILING_EXT);
        storage(GL_TEXTURE_2D, 1, GL_RGBA8, width, height, memory, 0);
        GLint actual{}; glGetTexLevelParameteriv(GL_TEXTURE_2D, 0, GL_TEXTURE_WIDTH, &actual);
        return actual == int(width);
    }
};
