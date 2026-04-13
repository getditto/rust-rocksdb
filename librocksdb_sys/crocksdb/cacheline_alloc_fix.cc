// DB-1237: intercept cacheline_aligned_alloc via GNU ld --wrap.
//
// The upstream tikv/rocksdb implementation of cacheline_aligned_alloc in
// port/port_posix.cc silently returns nullptr when posix_memalign fails.
// StatisticsData::operator new[] always routes through this function; a null
// return leaves CoreLocalArray::data_ null, and any subsequent access to
// per-core statistics dereferences null + ticker_offset → SIGSEGV.
//
// This file is compiled by the cc crate (not cmake), so it is completely
// unaffected by cmake build caches.  build.rs injects
//   -Wl,--wrap,_ZN7rocksdb4port23cacheline_aligned_allocEm
// at link time (Linux only), which causes the linker to redirect all calls
// to cacheline_aligned_alloc to __wrap_... below.

#include <cstdlib>
#include <cstdio>

extern "C" {

// The original function, renamed to __real_... by the --wrap linker flag.
extern void*
__real__ZN7rocksdb4port23cacheline_aligned_allocEm(unsigned long size);

// Replacement: if posix_memalign (inside the real function) returns nullptr,
// fall back to plain malloc.  Cache-line alignment is a performance hint, not
// a correctness requirement.  Abort only if malloc also fails.
void*
__wrap__ZN7rocksdb4port23cacheline_aligned_allocEm(unsigned long size)
{
    void* m = __real__ZN7rocksdb4port23cacheline_aligned_allocEm(size);
    if (m == nullptr) {
        m = malloc(size);
        if (m == nullptr) {
            fprintf(stderr,
                    "cacheline_aligned_alloc: OOM for %lu bytes (DB-1237)\n",
                    size);
            abort();
        }
        // Print to stderr so the fallback is visible in CI logs.
        fprintf(stderr,
                "DB-1237: cacheline_aligned_alloc: posix_memalign failed for "
                "%lu bytes; fell back to malloc\n",
                size);
    }
    return m;
}

} // extern "C"
