# LiteBox OCI Runtime - Benchmark Results

Comprehensive performance benchmarks for litebox_runner_oci.

**Test Environment:**
- Ubuntu 24.04 LTS (Azure VM)
- Kernel: 6.8.x with ublk support
- CPU: Azure Standard tier
- Storage: SSD-backed
- LiteBox version: with tar indexing (O(1) file lookups)

## Container Startup by Image Type

Benchmarks with tar indexing enabled (all times in seconds, averaged over 3 runs):

| Image | Size | Files | Eager (cached) | Lazy-tar (indexed) | Improvement |
|-------|------|-------|----------------|-------------------|-------------|
| Alpine | 8.7MB | 84 | 0.27s | **0.22s** | 19% faster |
| Debian bookworm-slim | 82MB | 3,264 | 0.32s | **0.13s** | **59% faster** |
| Ubuntu 24.04 | 84MB | 2,587 | 0.30s | **0.13s** | **57% faster** |
| Python 3.11-slim | 131MB | 4,944 | 0.40s | **0.18s** | **55% faster** |
| Node.js 20-slim | 205MB | 5,667 | 0.65s | **0.41s** | **37% faster** |

**Key Insights:**
- **Lazy-tar with indexing now wins for ALL image types**
- Tar indexing provides O(1) file lookups instead of O(n) linear scan
- Index is built once on tar load (one-time O(n) cost, ~50ms for 5000 files)
- Improvement grows with file count: 19% for 84 files → 59% for 3264 files
- Glibc-based distros (Debian, Ubuntu) see the largest improvement

## First Run vs Cached Run

First run includes tar creation and caching:

| Image | First Run (tar creation) | Cached Run | Cache Hit Speedup |
|-------|--------------------------|------------|-------------------|
| Alpine | 0.24s | 0.22s | 8% |
| Debian | 0.23s | 0.13s | 43% |
| Python | 0.34s | 0.18s | 47% |
| Node.js | 0.60s | 0.41s | 32% |

Tar files are cached in `~/.cache/litebox-oci/tar/` using rootfs content hash.

## Loading Mode Comparison

Tested with Alpine 8.7MB image (84 files):

| Mode | Time | Setup Required | Notes |
|------|------|----------------|-------|
| **Lazy-tar (indexed)** | **0.22s** | None | Fastest for most cases |
| Eager | 0.27s | None | Simple, predictable |
| Loop+squashfs | 0.36s | mksquashfs | Kernel mount overhead |
| ublk+squashfs | 0.39s | modprobe, rublk | Best for repeated access |
| Squashfuse (FUSE) | 0.48s | squashfuse pkg | FUSE overhead (~200ms) |

## Tar Indexing Performance

The tar indexing implementation provides O(1) lookups:

| Operation | Before (O(n) scan) | After (indexed) | Speedup |
|-----------|-------------------|-----------------|---------|
| open() | ~1.5ms per file | ~1μs | ~1500x |
| stat() | ~1.5ms per file | ~1μs | ~1500x |
| readdir() | ~5ms per dir | ~100μs | ~50x |

**Index build time (one-time cost):**

| Files | Index Build Time |
|-------|------------------|
| 84 (Alpine) | ~5ms |
| 3,264 (Debian) | ~20ms |
| 4,944 (Python) | ~30ms |
| 5,667 (Node) | ~35ms |

## Memory Usage Estimates

| Mode | Alpine 8.7MB | Python 131MB | Node 205MB |
|------|--------------|--------------|------------|
| Eager | ~12MB | ~140MB | ~220MB |
| Lazy-tar | ~3MB* | ~15MB* | ~25MB* |
| ublk+squashfs | ~2MB | ~5MB | ~8MB |

*Lazy-tar only loads accessed files. Memory grows as files are accessed.

## Binary Caching Impact

Rewritten executables are cached in `~/.cache/litebox-oci/rewritten/`:

| Scenario | First Run | Cached Run | Speedup |
|----------|-----------|------------|---------|
| Alpine (echo) | 0.31s | 0.27s | 13% |
| Python hello | 0.52s | 0.40s | 23% |
| Node hello | 0.82s | 0.65s | 21% |

## Recommendations by Use Case

| Use Case | Recommended Mode | Rationale |
|----------|------------------|-----------|
| **All workloads** | `--lazy-tar` | Now fastest with indexing |
| Simple busybox/Alpine | `--lazy-tar` | Lowest memory, fast |
| Python/Node/Go apps | `--lazy-tar` | Indexed lookups are fast |
| Large data containers | `--lazy-tar` | Only loads needed files |
| Repeated same image | ublk+squashfs | Kernel cache persists |
| Memory-constrained | `--lazy-tar` | On-demand loading |

## Reproducing Benchmarks

### Setup Test Images

```bash
# Install tools
sudo apt install skopeo umoci

# Alpine bundle
mkdir -p /tmp/oci-bundles /tmp/oci-images
skopeo copy docker://alpine:latest oci:/tmp/oci-images/alpine:latest
umoci unpack --rootless --image /tmp/oci-images/alpine:latest /tmp/oci-bundles/alpine

# Create minimal config.json
cat > /tmp/oci-bundles/alpine/config.json << 'EOF'
{
  "ociVersion": "1.0.0",
  "root": { "path": "rootfs" },
  "process": {
    "args": ["/bin/echo", "Hello"],
    "env": ["PATH=/bin"],
    "cwd": "/",
    "user": { "uid": 0, "gid": 0 }
  }
}
EOF
```

### Run Benchmarks

```bash
# Clear caches
rm -rf ~/.cache/litebox-oci/tar/
rm -rf ~/.cache/litebox-oci/rewriter/

# Eager mode (default)
time litebox_runner_oci run --bundle /tmp/oci-bundles/alpine test-eager

# Lazy-tar mode (with indexing)
time litebox_runner_oci run --bundle /tmp/oci-bundles/alpine --lazy-tar test-lazy

# Cleanup
litebox_runner_oci delete test-eager test-lazy
```

## Version History

- **2026-02-06**: Added tar indexing for O(1) file lookups
  - Lazy-tar now fastest for all image types
  - 37-59% improvement for glibc-based images
  - Index build time: ~5-35ms depending on file count

- **2026-02-05**: Initial benchmark collection on Ubuntu 24.04 Azure
  - Tested Alpine, Python, and synthetic large images
  - Compared eager, lazy-tar, FUSE, ublk, and loop modes
