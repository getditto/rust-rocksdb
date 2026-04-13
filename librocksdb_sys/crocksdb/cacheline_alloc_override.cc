// DB-1237: Override cacheline_aligned_alloc to never return nullptr.
//
// The upstream tikv/rocksdb implementation (port/port_posix.cc) uses
// posix_memalign and silently returns nullptr if it fails. This leaves
// CoreLocalArray::data_ null, causing a SIGSEGV on the first statistics
// access (e.g. recordTick crashes at null + ticker_offset).
//
// Fix: provide a STRONG definition here (in libcrocksdb.a, which is linked
// before librocksdb.a) that falls back to malloc when posix_memalign fails.
// build.rs also weakens the symbol in librocksdb.a via objcopy so the linker
// always picks this definition over the original.
//
// Cache-line alignment is a performance hint, not a correctness requirement,
// so malloc is a valid fallback.  If malloc also fails we abort immediately
// with a diagnostic message rather than returning nullptr silently.

#include <cstdlib>
#include <cstdio>
#include <cerrno>

// Match the alignment value used by RocksDB's port_posix.h (64 on x86_64/arm64).
#ifndef CACHE_LINE_SIZE
#  if defined(__powerpc__) || defined(__aarch64__)
#    define CACHE_LINE_SIZE 128U
#  else
#    define CACHE_LINE_SIZE 64U
#  endif
#endif

namespace rocksdb {
namespace port {

void* cacheline_aligned_alloc(size_t size) {
#if defined(_POSIX_C_SOURCE) && _POSIX_C_SOURCE >= 200112L || \
    defined(_XOPEN_SOURCE) && _XOPEN_SOURCE >= 600 || \
    defined(__APPLE__)
    void* m = nullptr;
    int err = posix_memalign(&m, CACHE_LINE_SIZE, size);
    if (err != 0 || m == nullptr) {
        // posix_memalign failed; fall back to plain malloc.
        // Cache-line alignment is a performance hint, not a correctness
        // requirement.  Returning nullptr causes CoreLocalArray::data_ to be
        // null, which leads to SIGSEGV in recordTick (DB-1237).
        m = malloc(size);
        if (m == nullptr) {
            fprintf(stderr,
                    "DB-1237: cacheline_aligned_alloc: OOM for %zu bytes "
                    "(posix_memalign err=%d)\n",
                    size, err);
            abort();
        }
    }
    return m;
#else
    return malloc(size);
#endif
}

} // namespace port
} // namespace rocksdb
