# LiteBox OCI Runtime - Integration Testing Instructions

This document provides step-by-step instructions for manually testing the LiteBox OCI runtime.

## Prerequisites

- Rust toolchain installed
- `skopeo` and `umoci` for OCI image handling
- `containerd` for container runtime integration tests
- Root access for containerd operations

## 1. Build and Install

```bash
cd /workspace/litebox
cargo build -p litebox_runner_oci --release
sudo cp target/release/litebox_runner_oci /usr/local/bin/litebox-oci
```

Verify installation:

```bash
litebox-oci --version
litebox-oci info
```

## 2. Create Test Bundle (Alpine)

Pull and unpack an Alpine image to create an OCI bundle:

```bash
# Create bundle directory
mkdir -p /tmp/test-bundle
cd /tmp/test-bundle

# Pull Alpine image using skopeo
skopeo copy docker://alpine:latest oci:alpine:latest

# Unpack to a temporary directory
umoci unpack --image alpine:latest bundle

# Move rootfs to expected location
mv bundle/rootfs .
rm -rf bundle alpine

# Create OCI config.json
cat > config.json << 'EOF'
{
  "ociVersion": "1.0.0",
  "root": { "path": "rootfs" },
  "process": {
    "args": ["/bin/echo", "Hello from LiteBox!"],
    "env": ["PATH=/usr/local/bin:/usr/bin:/bin"],
    "cwd": "/",
    "user": { "uid": 0, "gid": 0 }
  }
}
EOF
```

## 3. Test Direct Run

Run a container directly (create + start in one step):

```bash
litebox-oci run --bundle /tmp/test-bundle test-run
```

Expected output:
```
Hello from LiteBox!
```

## 4. Test OCI Lifecycle Commands

### 4.1 Create Container

```bash
litebox-oci create -b /tmp/test-bundle test-lifecycle
```

### 4.2 Check State

```bash
litebox-oci state test-lifecycle
```

Expected output shows `"status": "created"`.

### 4.3 List Containers

```bash
litebox-oci list
```

### 4.4 Start Container

```bash
litebox-oci start test-lifecycle
```

### 4.5 Delete Container

```bash
litebox-oci delete test-lifecycle
```

### 4.6 Force Delete (if running)

```bash
litebox-oci delete --force test-lifecycle
```

## 5. Test via containerd

### 5.1 Start containerd

```bash
sudo systemctl start containerd
```

### 5.2 Pull Test Image

```bash
sudo ctr image pull docker.io/library/alpine:latest
```

### 5.3 Run Containers with LiteBox Runtime

**Echo test:**
```bash
sudo ctr run --rm --runc-binary /usr/local/bin/litebox-oci \
  docker.io/library/alpine:latest test-echo \
  /bin/echo "Hello via containerd"
```

**Shell command:**
```bash
sudo ctr run --rm --runc-binary /usr/local/bin/litebox-oci \
  docker.io/library/alpine:latest test-uname \
  /bin/sh -c "uname -a"
```

**List files:**
```bash
sudo ctr run --rm --runc-binary /usr/local/bin/litebox-oci \
  docker.io/library/alpine:latest test-ls \
  /bin/ls /
```

**Read file:**
```bash
sudo ctr run --rm --runc-binary /usr/local/bin/litebox-oci \
  docker.io/library/alpine:latest test-cat \
  /bin/cat /etc/os-release
```

## 6. Debugging

### Enable Debug Logging

```bash
RUST_LOG=debug litebox-oci run --bundle /tmp/test-bundle test-debug
```

### Check Container State Directory

```bash
ls -la /run/litebox-oci/containers/
```

### View Container State

```bash
cat /run/litebox-oci/containers/<container-id>/state.json
```

## 7. Rootless Operation

LiteBox OCI supports rootless operation without requiring root privileges.

### 7.1 Rootless CLI

Use `--root` to specify a user-writable state directory:

```bash
# Run rootless
litebox-oci --root ~/.litebox-oci run --bundle /tmp/test-bundle test-run

# Check containers
ls ~/.litebox-oci/containers/
cat ~/.litebox-oci/containers/*/state.json

# Lifecycle commands
litebox-oci --root ~/.litebox-oci create -b /tmp/test-bundle test1
litebox-oci --root ~/.litebox-oci list
litebox-oci --root ~/.litebox-oci delete test1
```

Set an alias for convenience:

```bash
alias litebox-oci='litebox-oci --root ~/.litebox-oci'
```

### 7.2 Rootless containerd

```bash
# Install rootless containerd (if not already)
containerd-rootless-setuptool.sh install

# Start rootless containerd
systemctl --user start containerd

# Pull image (rootless)
ctr --address ~/.local/share/containerd/containerd.sock \
    image pull docker.io/library/alpine:latest

# Run with litebox-oci (rootless)
ctr --address ~/.local/share/containerd/containerd.sock \
    run --rm --runc-binary $(which litebox-oci) \
    docker.io/library/alpine:latest test-rootless \
    /bin/echo "Hello rootless!"
```

### 7.3 Rootless nerdctl

`nerdctl` provides better rootless support:

```bash
# Run rootless with nerdctl
nerdctl run --rm --runtime=$(which litebox-oci) alpine echo "Hello"
```

### 7.4 Rootless containerd Configuration

Create `~/.config/containerd/config.toml`:

```toml
[plugins."io.containerd.grpc.v1.cri".containerd.runtimes.litebox]
  runtime_type = "io.containerd.runc.v2"
  [plugins."io.containerd.grpc.v1.cri".containerd.runtimes.litebox.options]
    BinaryName = "/home/YOUR_USER/.cargo/bin/litebox-oci"
    Root = "/home/YOUR_USER/.litebox-oci"
```

Replace `YOUR_USER` with your actual username.

## 8. Cleanup

```bash
# Remove test bundle
rm -rf /tmp/test-bundle

# Remove container state
sudo rm -rf /run/litebox-oci/containers/*
```

## Troubleshooting

### "container already exists" error

Delete the existing container first:
```bash
litebox-oci delete --force <container-id>
```

### containerd permission denied

Ensure you're running with `sudo` for containerd operations.

### Process not found after start

The container may have exited. Check the state:
```bash
litebox-oci state <container-id>
```

If status is `stopped`, the process has completed.
