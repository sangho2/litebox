# LiteBox OCI Runtime - Benchmark Results

Comprehensive performance benchmarks for litebox_runner_oci.

**Test Environment:**
- Ubuntu 24.04 LTS (Azure VM)
- Kernel: 6.8.x with ublk support
- CPU: Azure Standard tier
- Storage: SSD-backed

## Container Startup by Image Type

| Image | Size | Files | Eager | Lazy-tar | Winner | Notes |
|-------|------|-------|-------|----------|--------|-------|
| Alpine (busybox) | 12MB | 432 | 0.27s | **0.23s** | Lazy-tar | Simple, few libs |
| Python 3.12 (bundle) | 74MB | 1,226 | **0.13s** | 0.54s | Eager | Many stdlib probes |
| Python 3.11-alpine | 57MB | 2,941 | 0.32s | 0.55s | Eager | Many small files |
| Python 3.11-slim | 130MB | 6,000+ | **0.38s** | 0.80s | Eager | Debian-based |
| Large data (synthetic) | 174MB | 1,230 | **0.18s** | 0.60s | Eager | 100MB data file |
| Huge data (synthetic) | 574MB | 1,235 | **0.37s** | 0.78s | Eager | 500MB data files |

**Key Insights:**
- Eager mode scales linearly with file count/size
- Lazy-tar has fixed tar parsing overhead (~0.5s for complex images)
- Lazy-tar only wins for simple images with minimal file lookups
- LiteBox's in-memory filesystem is very efficient at bulk loading

## Loading Mode Comparison

Tested with Alpine 12MB image (432 files):

| Mode | First Run | Subsequent | Setup Required |
|------|-----------|------------|----------------|
| Eager | 0.27s | 0.27s | None |
| **Lazy-tar** | **0.23s** | **0.23s** | None |
| Loop+squashfs | 0.36s | 0.36s | mksquashfs |
| ublk+squashfs | 0.39s | 0.27s | modprobe, rublk |
| Squashfuse (FUSE) | 0.48s | 0.48s | squashfuse pkg |
| Ratarmount (FUSE) | 0.50s | 0.45s | ratarmount, index |

## FUSE Overhead Analysis

| Tool | Mount Time | Per-access Overhead | Total for Alpine |
|------|------------|---------------------|------------------|
| squashfuse | ~150ms | ~5-10μs | +200ms |
| ratarmount | ~180ms | ~10-20μs | +200ms |
| archivemount | ~200ms | ~20-50μs | +250ms |

**Conclusion:** FUSE overhead is dominated by mount setup, not per-file access. Unsuitable for one-shot containers where startup time matters.

## ublk vs Loop Device

| Metric | ublk | Loop Device |
|--------|------|-------------|
| Device creation | 26ms | <1ms |
| Mount time | 22ms | 15ms |
| Read latency | ~5μs | ~5μs |
| Kernel cache | Yes | Yes |
| Setup complexity | Medium | Low |

**ublk benefits:**
- io_uring integration for async I/O
- Userspace control over block device behavior
- Better for custom block device implementations

**When to use loop device:**
- Simple squashfs mounting
- Lower setup complexity
- No special kernel modules needed

## Binary Caching Impact

Rewritten executables are cached in `~/.cache/litebox-oci/rewritten/`:

| Scenario | First Run | Cached Run | Speedup |
|----------|-----------|------------|---------|
| Alpine (echo) | 0.31s | 0.27s | 13% |
| Python hello | 0.18s | 0.13s | 28% |
| Go binary | 0.25s | 0.22s | 12% |

**Most impactful for:** Python containers with many shared libraries to rewrite.

## Memory Usage Estimates

| Mode | Alpine 12MB | Python 74MB | Large 174MB |
|------|-------------|-------------|-------------|
| Eager | ~15MB | ~85MB | ~180MB |
| Lazy-tar | ~5MB* | ~20MB* | ~10MB* |
| ublk+squashfs | ~2MB | ~5MB | ~3MB |

*Lazy-tar only loads accessed files. Memory grows as more files are accessed during execution.

## Tar Parsing Overhead

The lazy-tar mode uses LiteBox's `tar_ro::FileSystem` which does O(n) linear scans:

| Image Files | Lookup Time | Cumulative (100 lookups) |
|-------------|-------------|--------------------------|
| 432 (Alpine) | ~0.5ms | ~50ms |
| 1,226 (Python) | ~1.5ms | ~150ms |
| 6,000 (slim) | ~7ms | ~700ms |

**Why Python is slower with lazy-tar:**
1. Python stdlib probes many paths for imports
2. Each `open()` triggers full tar scan
3. Complex applications may do 100+ file lookups

**Future improvement:** Tar indexing (like ratarmount) would provide O(1) lookups.

## Recommendations by Use Case

| Use Case | Recommended Mode | Rationale |
|----------|------------------|-----------|
| Simple busybox/Alpine | `--lazy-tar` | Fastest, lowest memory |
| Python/Node/Go apps | Eager (default) | Fewer tar lookups |
| Large data containers | Eager (default) | Linear scaling beats O(n) lookup |
| Repeated same image | ublk+squashfs | Kernel cache persists |
| Memory-constrained | `--lazy-tar` | On-demand loading |
| Development/debugging | Eager (default) | Simplest, most predictable |

## Reproducing Benchmarks

### Setup Test Images

```bash
# Alpine bundle
skopeo copy docker://alpine:latest oci:alpine-oci:latest
umoci unpack --image alpine-oci:latest /tmp/alpine-bundle

# Python bundle (custom, smaller than full image)
mkdir -p /tmp/python-bundle/rootfs
cp -a /usr/bin/python3 /tmp/python-bundle/rootfs/
# ... copy required libs

# Create config.json with appropriate args
```

### Run Benchmarks

```bash
# Eager mode (default)
time litebox_runner_oci run --bundle /tmp/alpine-bundle test1

# Lazy-tar mode
time litebox_runner_oci run --bundle /tmp/alpine-bundle --lazy-tar test2

# With squashfs
mksquashfs /tmp/alpine-bundle/rootfs /tmp/alpine.squashfs
sudo mount -o loop /tmp/alpine.squashfs /tmp/alpine-bundle/rootfs
time litebox_runner_oci run --bundle /tmp/alpine-bundle test3
```

### Clear Caches Between Runs

```bash
# Clear binary cache
rm -rf ~/.cache/litebox-oci/rewritten/

# Clear tar cache
sudo rm -rf /root/.cache/litebox-oci/tar/

# Clear squashfs cache
sudo rm -rf /root/.cache/litebox-oci/squashfs/

# Drop kernel page cache (for ublk/loop tests)
sudo sh -c 'echo 3 > /proc/sys/vm/drop_caches'
```

## Version History

- **2026-02-05**: Initial benchmark collection on Ubuntu 24.04 Azure
  - Tested Alpine, Python, and synthetic large images
  - Compared eager, lazy-tar, FUSE, ublk, and loop modes
  - Documented memory usage and caching impact
