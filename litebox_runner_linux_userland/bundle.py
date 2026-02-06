#!/usr/bin/env python3
"""
Bundle script for litebox_runner_linux_userland.

Creates bundles with a rewritten binary and a tar containing rewritten libraries
at their original absolute paths (as expected by the dynamic linker).

Usage:
  # Prepare a binary with its dependencies
  ./bundle.py --binary /usr/bin/echo --output-dir /tmp/litebox-echo
  
  # Then run:
  /tmp/litebox-echo/run.sh hello

  # From a container image:
  ./bundle.py --image alpine:latest --output-dir /tmp/litebox-alpine
  /tmp/litebox-alpine/run.sh /bin/echo "Hello from Alpine"
"""

import argparse
import os
import shutil
import subprocess
import sys
import tarfile
import tempfile
from pathlib import Path


def run_cmd(cmd, check=True):
    return subprocess.run(cmd, capture_output=True, text=True, check=check)


def find_deps(binary):
    """Find shared library dependencies using ldd."""
    try:
        result = run_cmd(["ldd", binary], check=False)
        if result.returncode != 0:
            return []
        
        deps = []
        for line in result.stdout.splitlines():
            line = line.strip()
            if "=>" in line:
                parts = line.split("=>")
                if len(parts) >= 2:
                    path = parts[1].strip().split()[0]
                    if path.startswith("/") and os.path.exists(path):
                        deps.append(path)
            elif line.startswith("/"):
                path = line.split()[0]
                if os.path.exists(path):
                    deps.append(path)
        return deps
    except Exception:
        return []


def is_elf(filepath):
    try:
        with open(filepath, "rb") as f:
            return f.read(4) == b"\x7fELF"
    except Exception:
        return False


def rewrite_elf(src, dest, rewriter_path):
    try:
        result = run_cmd([rewriter_path, src, "-o", dest], check=False)
        return result.returncode == 0
    except Exception as e:
        print(f"Warning: failed to rewrite {src}: {e}", file=sys.stderr)
        return False


def bundle_from_binary(binary, output_dir, rewriter_path):
    """Create a bundle with rewritten binary and tar of libs at original paths."""
    binary = os.path.abspath(binary)
    if not os.path.exists(binary):
        print(f"Error: binary not found: {binary}", file=sys.stderr)
        sys.exit(1)
    
    os.makedirs(output_dir, exist_ok=True)
    
    # Rewrite binary to output dir
    bin_name = os.path.basename(binary)
    bin_dest = os.path.join(output_dir, f"{bin_name}.hooked")
    if rewrite_elf(binary, bin_dest, rewriter_path):
        print(f"Rewritten: {binary} -> {bin_dest}")
    else:
        print(f"Error: failed to rewrite {binary}", file=sys.stderr)
        sys.exit(1)
    
    # Create temp directory for tar contents (libs at original paths)
    with tempfile.TemporaryDirectory() as tmpdir:
        deps = find_deps(binary)
        for dep in deps:
            # Put lib at its original absolute path within tmpdir
            # e.g., /lib/x86_64-linux-gnu/libc.so.6 -> tmpdir/lib/x86_64-linux-gnu/libc.so.6
            rel_path = dep.lstrip("/")
            dest = os.path.join(tmpdir, rel_path)
            os.makedirs(os.path.dirname(dest), exist_ok=True)
            
            if rewrite_elf(dep, dest, rewriter_path):
                print(f"Rewritten: {dep}")
            else:
                shutil.copy2(dep, dest)
                print(f"Copied (rewrite failed): {dep}")
        
        # Create tar file - use GNU format (tar_ro may not support PAX)
        tar_path = os.path.join(output_dir, "libs.tar")
        with tarfile.open(tar_path, "w:") as tar:
            tar.format = tarfile.GNU_FORMAT
            # Add directories first
            for root, dirs, files in os.walk(tmpdir):
                for d in sorted(dirs):
                    dirpath = os.path.join(root, d)
                    arcname = os.path.relpath(dirpath, tmpdir)
                    tar.add(dirpath, arcname=arcname, recursive=False)
            # Then add files
            for root, dirs, files in os.walk(tmpdir):
                for f in sorted(files):
                    src = os.path.join(root, f)
                    arcname = os.path.relpath(src, tmpdir)
                    tar.add(src, arcname=arcname)
        print(f"Created: {tar_path}")
    
    # Create run script
    runner_path = shutil.which("litebox_runner_linux_userland")
    if not runner_path:
        script_dir = os.path.dirname(os.path.abspath(__file__))
        runner_path = os.path.join(script_dir, "..", "target", "release", "litebox_runner_linux_userland")
    
    run_script = os.path.join(output_dir, "run.sh")
    with open(run_script, "w") as f:
        f.write(f'''#!/bin/bash
# Auto-generated run script for litebox
BUNDLE_DIR="$(cd "$(dirname "$0")" && pwd)"
RUNNER="{os.path.abspath(runner_path)}"

exec "$RUNNER" -Z \\
    --interception-backend rewriter \\
    --initial-files "$BUNDLE_DIR/libs.tar" \\
    --env "LD_LIBRARY_PATH=/lib64:/lib32:/lib:/lib/x86_64-linux-gnu" \\
    --env "HOME=/" \\
    "$BUNDLE_DIR/{bin_name}.hooked" "$@"
''')
    os.chmod(run_script, 0o755)
    
    print(f"\nBundle created at: {output_dir}")
    print(f"Run with: {run_script} [args...]")


def bundle_from_image(image, output_dir, rewriter_path):
    """Create a bundle from a container image."""
    for tool in ["skopeo", "umoci"]:
        if not shutil.which(tool):
            print(f"Error: {tool} not found", file=sys.stderr)
            sys.exit(1)
    
    with tempfile.TemporaryDirectory() as tmpdir:
        oci_dir = os.path.join(tmpdir, "oci")
        bundle_dir = os.path.join(tmpdir, "bundle")
        tar_stage = os.path.join(tmpdir, "tar_stage")
        
        print(f"Pulling {image}...")
        result = run_cmd(["skopeo", "copy", f"docker://{image}", f"oci:{oci_dir}:latest"], check=False)
        if result.returncode != 0:
            print(f"Error: {result.stderr}", file=sys.stderr)
            sys.exit(1)
        
        print("Unpacking...")
        run_cmd(["umoci", "unpack", "--rootless", "--image", f"{oci_dir}:latest", bundle_dir], check=False)
        
        rootfs = os.path.join(bundle_dir, "rootfs")
        os.makedirs(output_dir, exist_ok=True)
        os.makedirs(tar_stage, exist_ok=True)
        
        print("Rewriting ELF files...")
        rewritten = 0
        copied = 0
        
        # Rewrite all ELF files and place in tar_stage at their original paths
        for root, dirs, files in os.walk(rootfs):
            for f in files:
                src = os.path.join(root, f)
                rel = os.path.relpath(src, rootfs)  # e.g., bin/echo or lib/libc.so.6
                dest = os.path.join(tar_stage, rel)
                os.makedirs(os.path.dirname(dest), exist_ok=True)
                
                try:
                    if is_elf(src):
                        if rewrite_elf(src, dest, rewriter_path):
                            rewritten += 1
                        else:
                            shutil.copy2(src, dest)
                            copied += 1
                    else:
                        shutil.copy2(src, dest)
                        copied += 1
                except (PermissionError, OSError) as e:
                    # Skip files we can't read
                    pass
            
            # Handle symlinks
            for d in dirs:
                dirpath = os.path.join(root, d)
                if os.path.islink(dirpath):
                    rel = os.path.relpath(dirpath, rootfs)
                    dest = os.path.join(tar_stage, rel)
                    target = os.readlink(dirpath)
                    try:
                        os.makedirs(os.path.dirname(dest), exist_ok=True)
                        if not os.path.exists(dest):
                            os.symlink(target, dest)
                    except (PermissionError, OSError):
                        pass
        
        # Create tar file - use GNU format (tar_ro may not support PAX)
        tar_path = os.path.join(output_dir, "rootfs.tar")
        with tarfile.open(tar_path, "w:") as tar:
            tar.format = tarfile.GNU_FORMAT
            for root, dirs, files in os.walk(tar_stage):
                for f in sorted(files):
                    src = os.path.join(root, f)
                    arcname = os.path.relpath(src, tar_stage)
                    tar.add(src, arcname=arcname)
                for d in sorted(dirs):
                    dirpath = os.path.join(root, d)
                    if os.path.islink(dirpath):
                        arcname = os.path.relpath(dirpath, tar_stage)
                        tar.add(dirpath, arcname=arcname)
        
        print(f"Created: {tar_path} (rewritten: {rewritten}, copied: {copied})")
        
        # Create run script - user specifies the command, we prepend rootfs prefix in tar
        runner_path = shutil.which("litebox_runner_linux_userland")
        if not runner_path:
            script_dir = os.path.dirname(os.path.abspath(__file__))
            runner_path = os.path.join(script_dir, "..", "target", "release", "litebox_runner_linux_userland")
        
        run_script = os.path.join(output_dir, "run.sh")
        with open(run_script, "w") as f:
            f.write(f'''#!/bin/bash
# Auto-generated run script for litebox container image
# Usage: ./run.sh /bin/echo "Hello"
BUNDLE_DIR="$(cd "$(dirname "$0")" && pwd)"
RUNNER="{os.path.abspath(runner_path)}"

if [ $# -eq 0 ]; then
    echo "Usage: $0 <command> [args...]"
    echo "Example: $0 /bin/echo hello"
    exit 1
fi

# The binary must exist in the tar at the given path
# We extract the binary to a temp file and run it
CMD="$1"
shift

# Extract binary from tar to temp location with correct name (for busybox applets)
TEMP_DIR=$(mktemp -d)
BIN_NAME=$(basename "$CMD")
TEMP_BIN="$TEMP_DIR/$BIN_NAME"
tar -xf "$BUNDLE_DIR/rootfs.tar" -O "${{CMD#/}}" > "$TEMP_BIN" 2>/dev/null
if [ $? -ne 0 ]; then
    echo "Error: $CMD not found in rootfs.tar"
    rm -rf "$TEMP_DIR"
    exit 1
fi
chmod +x "$TEMP_BIN"

# Run with the tar as initial-files (provides libraries at their original paths)
"$RUNNER" -Z \\
    --interception-backend rewriter \\
    --initial-files "$BUNDLE_DIR/rootfs.tar" \\
    --env "LD_LIBRARY_PATH=/lib64:/lib32:/lib:/lib/x86_64-linux-gnu:/usr/lib" \\
    --env "HOME=/" \\
    --env "PATH=/bin:/usr/bin:/sbin:/usr/sbin" \\
    "$TEMP_BIN" "$@"
EXIT_CODE=$?

rm -rf "$TEMP_DIR"
exit $EXIT_CODE
''')
        os.chmod(run_script, 0o755)
        
        print(f"\nBundle created at: {output_dir}")
        print(f"Run with: {run_script} /bin/echo hello")


def main():
    parser = argparse.ArgumentParser(description="Create bundles for litebox_runner_linux_userland")
    
    source = parser.add_mutually_exclusive_group(required=True)
    source.add_argument("--binary", help="Local binary path")
    source.add_argument("--image", help="Container image (e.g., alpine:latest)")
    
    parser.add_argument("--output-dir", "-o", required=True, help="Output directory")
    parser.add_argument("--rewriter-path", help="Path to litebox_syscall_rewriter")
    
    args = parser.parse_args()
    
    # Find rewriter
    rewriter_path = args.rewriter_path
    if not rewriter_path:
        candidates = [
            "./target/release/litebox_syscall_rewriter",
            "./target/debug/litebox_syscall_rewriter",
        ]
        for c in candidates:
            if os.path.exists(c):
                rewriter_path = os.path.abspath(c)
                break
    
    if not rewriter_path or not os.path.exists(rewriter_path):
        print("Error: litebox_syscall_rewriter not found. Build with: cargo build --release -p litebox_syscall_rewriter",
              file=sys.stderr)
        sys.exit(1)
    
    if args.binary:
        bundle_from_binary(args.binary, args.output_dir, rewriter_path)
    elif args.image:
        bundle_from_image(args.image, args.output_dir, rewriter_path)


if __name__ == "__main__":
    main()
