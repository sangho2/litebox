# LiteBox OCI Runtime - Benchmark Results

Comprehensive performance benchmarks for litebox_runner_oci.

**Test Environment:**
- Ubuntu 24.04 LTS (Azure VM)
- Kernel: 6.8.x
- CPU: Azure Standard tier
- Storage: SSD-backed
- LiteBox version: with lazy rewriting and tar indexing

## Executive Summary

**Lazy-rewrite (`--lazy-rewrite`) is now the fastest mode for all workloads.**

| Image | Size | Files | Lazy-rewrite | vs Eager | vs Lazy-tar |
|-------|------|-------|--------------|----------|-------------|
| Alpine | 8.7MB | 84 | **252ms** | 22% faster | 22% faster |
| Debian | 82MB | 3,264 | **164ms** | 73% faster | 68% faster |
| Ubuntu | 84MB | 2,587 | **158ms** | 71% faster | 67% faster |
| Python | 131MB | 4,944 | **248ms** | 67% faster | 63% faster |
| Node.js | 205MB | 5,667 | **1059ms** | 26% faster | 23% faster |

## Cold Cache Performance (First Run)

All caches cleared before each run. Average of 3 runs.

| Image | Files | Eager | Lazy-tar | Lazy-rewrite |
|-------|-------|-------|----------|--------------|
| Alpine | 84 | 322ms | 323ms | **252ms** |
| Debian | 3,264 | 593ms | 509ms | **164ms** |
| Ubuntu | 2,587 | 540ms | 475ms | **158ms** |
| Python | 4,944 | 734ms | 667ms | **248ms** |
| Node.js | 5,667 | 1428ms | 1362ms | **1059ms** |

**Key Insights:**
- **Lazy-rewrite is 22-73% faster than Eager mode**
- Improvement is greatest for images with many executables (Debian, Ubuntu)
- Node.js improvement is smaller due to many large JS files (non-executable)

## Warm Cache Performance (Subsequent Runs)

One warmup run, then average of 3 runs with caches populated.

| Image | Eager | Lazy-tar | Lazy-rewrite | vs Eager | vs Lazy-tar |
|-------|-------|----------|--------------|----------|-------------|
| Alpine | 278ms | 233ms | **218ms** | 22% faster | 7% faster |
| Debian | 323ms | 141ms | **64ms** | 81% faster | 55% faster |
| Ubuntu | 292ms | 129ms | **63ms** | 79% faster | 52% faster |
| Python | 415ms | 182ms | **89ms** | 79% faster | 52% faster |
| Node.js | 666ms | 392ms | **322ms** | 52% faster | 18% faster |

**Key Insights:**
- Warm cache performance is even better (81% faster for Debian)
- Cached binary rewrites + cached tar = minimal startup overhead
- For repeated container runs, expect 52-81% improvement vs Eager

## Loading Mode Comparison

### How Each Mode Works

| Mode | Executables | Other Files | Rewriting |
|------|-------------|-------------|-----------|
| **Lazy-rewrite** | Critical only upfront | On-demand from tar | On-demand |
| Lazy-tar | All upfront | On-demand from tar | All upfront |
| Eager | All upfront | All upfront | All upfront |
| Squashfs | All upfront | Via kernel mount | All upfront |

### When to Use Each Mode

| Mode | Best For | Trade-offs |
|------|----------|------------|
| `--lazy-rewrite` | **Most workloads** | Fastest startup, minimal memory |
| `--lazy-tar` | Simple validation | All executables ready |
| (default/Eager) | Debugging | Predictable, all files in memory |
| `--lazy` (squashfs) | Kernel compatibility | Requires root/sudo |

## Why Lazy-Rewrite is Fastest

The new lazy-rewrite mode provides the best performance by:

1. **Only loading critical executables upfront:**
   - Dynamic linker (ld-linux or ld-musl)
   - Main binary (resolved from command)
   - Symlink targets of main binary

2. **Lazy transformation on first access:**
   - When an executable is opened, it's rewritten and cached
   - Uses the `ExecutableTransform` trait in the layered filesystem
   - Transformed files are promoted to upper layer for subsequent access

3. **Most workloads only use a fraction of executables:**
   - Python image has ~500 executables, but a simple script uses ~10
   - Debian has ~300 executables, but `echo "Hello"` uses ~3

### Startup Cost Breakdown

For a Python container running a simple script:

| Phase | Eager | Lazy-tar | Lazy-rewrite |
|-------|-------|----------|--------------|
| Tar loading | ~50ms | ~50ms | ~50ms |
| Index build | - | ~30ms | ~30ms |
| Executable rewriting | ~400ms (all) | ~400ms (all) | ~30ms (3 files) |
| Other file loading | ~280ms | ~0ms | ~0ms |
| **Total** | **~730ms** | **~480ms** | **~110ms** |

## Tar Indexing Performance

The tar filesystem uses O(1) hash-based lookups:

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
| Lazy-rewrite | ~2MB* | ~10MB* | ~20MB* |

*Memory grows as files are accessed. Lazy-rewrite typically uses less memory because fewer executables are loaded upfront.

## Binary Caching

Rewritten executables are cached in `~/.cache/litebox-oci/rewritten/`:

| Scenario | Cold Run | Warm Run | Speedup |
|----------|----------|----------|---------|
| Alpine (lazy-rewrite) | 252ms | 218ms | 13% |
| Python (lazy-rewrite) | 248ms | 89ms | 64% |
| Debian (lazy-rewrite) | 164ms | 64ms | 61% |

Tar files are cached in `~/.cache/litebox-oci/tar/` using rootfs content hash.

## Reproducing Benchmarks

### Setup Test Images

```bash
# Install tools
sudo apt install skopeo umoci

# Create bundles directory
mkdir -p /tmp/oci-bundles /tmp/oci-images

# Alpine bundle
skopeo copy docker://alpine:latest oci:/tmp/oci-images/alpine:latest
umoci unpack --rootless --image /tmp/oci-images/alpine:latest /tmp/oci-bundles/alpine
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

# Repeat for other images (debian, ubuntu, python, node)
```

### Run Benchmarks

```bash
# Build release binary
cargo build --release -p litebox_runner_oci

# Clear caches for cold run
rm -rf ~/.cache/litebox-oci/

# Test each mode
time target/release/litebox_runner_oci run -b /tmp/oci-bundles/alpine test-eager
time target/release/litebox_runner_oci run -b /tmp/oci-bundles/alpine --lazy-tar test-tar
time target/release/litebox_runner_oci run -b /tmp/oci-bundles/alpine --lazy-rewrite test-rewrite

# Cleanup
target/release/litebox_runner_oci delete test-eager test-tar test-rewrite
```

## Version History

- **2026-02-06**: Added lazy executable rewriting (`--lazy-rewrite`)
  - 22-73% faster than Eager mode (cold cache)
  - 52-81% faster than Eager mode (warm cache)
  - Only critical executables rewritten upfront

- **2026-02-06**: Added tar indexing for O(1) file lookups
  - Lazy-tar 37-59% faster for glibc-based images
  - Index build time: ~5-35ms depending on file count

- **2026-02-05**: Initial benchmark collection on Ubuntu 24.04 Azure
  - Tested Alpine, Python, and synthetic large images
  - Compared eager, lazy-tar, FUSE, ublk, and loop modes
