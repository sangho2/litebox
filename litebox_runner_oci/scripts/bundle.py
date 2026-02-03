#!/usr/bin/env python3
"""
OCI Bundle Creator for LiteBox

Creates an OCI-compliant bundle from a binary or OCI image.

Usage:
    # From binary:
    ./bundle.py /usr/bin/python3 -o /tmp/my-bundle -- /app/hello.py
    ./bundle.py /usr/bin/python3 -o /tmp/my-bundle --file hello.py:/app/hello.py -- /app/hello.py

    # From OCI image:
    ./bundle.py --image alpine:latest -o /tmp/alpine-bundle
    ./bundle.py --image python:3.11-alpine -o /tmp/python-bundle -- python3 -c "print('hi')"
"""

import argparse
import json
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path


def get_library_dependencies(binary: Path) -> list[Path]:
    """Get shared library dependencies using ldd."""
    try:
        result = subprocess.run(["ldd", str(binary)], capture_output=True, text=True)
        libs = []
        for line in result.stdout.splitlines():
            if "=>" in line:
                parts = line.split("=>")
                if len(parts) >= 2:
                    lib_path = parts[1].strip().split()[0]
                    if lib_path.startswith("/"):
                        libs.append(Path(lib_path))
            elif line.strip().startswith("/"):
                lib_path = line.strip().split()[0]
                libs.append(Path(lib_path))
        return libs
    except Exception as e:
        print(f"Warning: Failed to get dependencies for {binary}: {e}", file=sys.stderr)
        return []


def copy_file_to(src: Path, dest: Path) -> None:
    """Copy a file to destination path."""
    if not src.exists():
        print(f"Warning: {src} does not exist, skipping", file=sys.stderr)
        return
    real_src = src.resolve()
    dest.parent.mkdir(parents=True, exist_ok=True)
    if not dest.exists():
        shutil.copy2(real_src, dest)
        print(f"  Copied: {src} -> {dest}")


def copy_file_preserve_path(src: Path, dest_root: Path) -> None:
    """Copy a file to dest_root, preserving directory structure."""
    if not src.exists():
        print(f"Warning: {src} does not exist, skipping", file=sys.stderr)
        return
    real_src = src.resolve()
    dest = dest_root / str(src).lstrip("/")
    dest.parent.mkdir(parents=True, exist_ok=True)
    if not dest.exists():
        shutil.copy2(real_src, dest)
        print(f"  Copied: {src}")


def create_oci_config(args: list[str], cwd: str = "/") -> dict:
    """Create OCI runtime config.json content."""
    return {
        "ociVersion": "1.0.0",
        "process": {
            "args": args,
            "cwd": cwd,
            "user": {"uid": 0, "gid": 0}
        },
        "root": {"path": "rootfs"}
    }


def parse_file_mapping(mapping: str) -> tuple[Path, Path]:
    """Parse a file mapping in format 'src:dest' or just 'src' (preserves path)."""
    if ":" in mapping:
        src, dest = mapping.split(":", 1)
        return Path(src), Path(dest)
    else:
        src = Path(mapping)
        return src, src if src.is_absolute() else Path("/") / src


def pull_oci_image(image_ref: str, output_dir: Path) -> None:
    """Pull an OCI image and unpack it to a bundle using skopeo + umoci."""
    if not image_ref.startswith(("docker://", "oci:", "docker-archive:")):
        image_ref = f"docker://{image_ref}"
    
    print(f"Pulling OCI image: {image_ref}")
    
    with tempfile.TemporaryDirectory() as tmpdir:
        oci_dir = Path(tmpdir) / "oci"
        
        print("  Running skopeo copy...")
        result = subprocess.run(
            ["skopeo", "copy", image_ref, f"oci:{oci_dir}:latest"],
            capture_output=True, text=True,
        )
        if result.returncode != 0:
            print(f"Error: skopeo failed: {result.stderr}", file=sys.stderr)
            sys.exit(1)
        
        print("  Running umoci unpack...")
        result = subprocess.run(
            ["umoci", "unpack", "--rootless", "--image", f"{oci_dir}:latest", str(output_dir)],
            capture_output=True, text=True,
        )
        if result.returncode != 0:
            print(f"Error: umoci failed: {result.stderr}", file=sys.stderr)
            sys.exit(1)
    
    print(f"  Image unpacked to: {output_dir}")


def create_bundle_from_binary(
    binary: Path,
    output_dir: Path,
    extra_args: list[str] | None = None,
    file_mappings: list[tuple[Path, Path]] | None = None,
    dir_mappings: list[tuple[Path, Path]] | None = None,
) -> None:
    """Create an OCI bundle for the given binary."""
    if output_dir.exists():
        shutil.rmtree(output_dir)
    
    rootfs = output_dir / "rootfs"
    rootfs.mkdir(parents=True)
    
    print(f"Creating OCI bundle at: {output_dir}")
    print(f"Target binary: {binary}")
    
    copy_file_preserve_path(binary, rootfs)
    
    print("Copying library dependencies...")
    for lib in get_library_dependencies(binary):
        copy_file_preserve_path(lib, rootfs)
    
    if file_mappings:
        print("Copying extra files...")
        for src, dest in file_mappings:
            copy_file_to(src, rootfs / str(dest).lstrip("/"))
    
    if dir_mappings:
        print("Copying directories...")
        for src, dest in dir_mappings:
            dest_path = rootfs / str(dest).lstrip("/")
            shutil.copytree(src, dest_path, dirs_exist_ok=True)
            print(f"  Copied dir: {src} -> {dest}")
    
    args = [str(binary)] + (extra_args or [])
    
    config_path = output_dir / "config.json"
    with open(config_path, "w") as f:
        json.dump(create_oci_config(args), f, indent=2)
    print(f"Created: {config_path}")
    
    total_size = sum(f.stat().st_size for f in rootfs.rglob("*") if f.is_file())
    print(f"\nBundle created successfully!")
    print(f"  Size: {total_size / 1024 / 1024:.1f} MB")
    print(f"  Command: {' '.join(args)}")


def create_bundle_from_image(
    image_ref: str,
    output_dir: Path,
    cmd_args: list[str] | None = None,
) -> None:
    """Create an OCI bundle from an OCI image."""
    if output_dir.exists():
        shutil.rmtree(output_dir)
    
    pull_oci_image(image_ref, output_dir)
    
    if cmd_args:
        config_path = output_dir / "config.json"
        with open(config_path) as f:
            config = json.load(f)
        config["process"]["args"] = cmd_args
        with open(config_path, "w") as f:
            json.dump(config, f, indent=2)
        print(f"Updated command: {' '.join(cmd_args)}")
    
    rootfs = output_dir / "rootfs"
    total_size = sum(f.stat().st_size for f in rootfs.rglob("*") if f.is_file())
    print(f"\nBundle created successfully!")
    print(f"  Size: {total_size / 1024 / 1024:.1f} MB")


def main():
    parser = argparse.ArgumentParser(
        description="Create an OCI bundle from a binary or OCI image",
        usage="%(prog)s [--image IMAGE | BINARY] -o OUTPUT [options] [-- CMD [ARGS...]]"
    )
    parser.add_argument(
        "binary", type=Path, nargs="?",
        help="Path to the target binary (mutually exclusive with --image)"
    )
    parser.add_argument(
        "--image", "-i", type=str,
        help="OCI image reference (e.g., alpine:latest, docker://python:3.11-slim)"
    )
    parser.add_argument(
        "-o", "--output", type=Path, required=True,
        help="Output directory for the bundle"
    )
    parser.add_argument(
        "--file", action="append", dest="files",
        help="File to include: 'src:dest' or 'src'. Only for binary mode."
    )
    parser.add_argument(
        "--dir", action="append", dest="dirs",
        help="Directory to include: 'src:dest' or 'src'. Only for binary mode."
    )
    parser.add_argument(
        "--python-stdlib", action="store_true",
        help="Include Python standard library. Only for binary mode."
    )
    parser.add_argument(
        "cmd", nargs="*",
        help="Command and arguments (after --)"
    )
    
    # Handle -- separator for command arguments
    try:
        sep_idx = sys.argv.index("--")
        main_args = sys.argv[1:sep_idx]
        cmd_args = sys.argv[sep_idx + 1:]
    except ValueError:
        main_args = sys.argv[1:]
        cmd_args = []
    
    args = parser.parse_args(main_args)
    
    # Validate mutually exclusive options
    if args.image and args.binary:
        parser.error("Cannot specify both binary and --image")
    if not args.image and not args.binary:
        parser.error("Must specify either binary or --image")
    
    if args.image:
        create_bundle_from_image(
            image_ref=args.image,
            output_dir=args.output,
            cmd_args=cmd_args if cmd_args else None,
        )
    else:
        binary = args.binary
        if not binary.is_absolute():
            result = shutil.which(str(binary))
            if result:
                binary = Path(result)
            else:
                print(f"Error: Binary not found: {binary}", file=sys.stderr)
                sys.exit(1)
        
        if not binary.exists():
            print(f"Error: Binary does not exist: {binary}", file=sys.stderr)
            sys.exit(1)
        
        file_mappings = [parse_file_mapping(f) for f in (args.files or [])]
        dir_mappings = [parse_file_mapping(d) for d in (args.dirs or [])]
        
        if args.python_stdlib:
            import sysconfig
            stdlib_path = Path(sysconfig.get_path("stdlib"))
            if stdlib_path.exists():
                dir_mappings.append((stdlib_path, stdlib_path))
                print(f"Including Python stdlib: {stdlib_path}")
        
        create_bundle_from_binary(
            binary=binary,
            output_dir=args.output,
            extra_args=cmd_args if cmd_args else None,
            file_mappings=file_mappings,
            dir_mappings=dir_mappings,
        )


if __name__ == "__main__":
    main()
