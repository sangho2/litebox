#!/bin/bash
# Integration test script for litebox-oci with containerd
#
# Usage:
#   ./integration_test.sh          # Run all tests
#   ./integration_test.sh --build  # Build and install first, then test
#   ./integration_test.sh --clean  # Clean up after tests

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
BINARY_NAME="litebox-oci"
INSTALL_PATH="/usr/local/bin/$BINARY_NAME"
TEST_IMAGE="docker.io/library/alpine:latest"

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

pass_count=0
fail_count=0

log_info() {
    echo -e "${YELLOW}[INFO]${NC} $1"
}

log_pass() {
    echo -e "${GREEN}[PASS]${NC} $1"
    pass_count=$((pass_count + 1))
}

log_fail() {
    echo -e "${RED}[FAIL]${NC} $1"
    fail_count=$((fail_count + 1))
}

# Check if running as root or with sudo
check_sudo() {
    if [ "$EUID" -ne 0 ]; then
        if ! sudo -n true 2>/dev/null; then
            echo "This script requires sudo access for containerd operations."
            echo "Please run with sudo or configure passwordless sudo."
            exit 1
        fi
    fi
}

# Build and install litebox-oci
build_and_install() {
    log_info "Building litebox-oci (release)..."
    cd "$REPO_ROOT"
    if ! cargo build --release -p litebox_runner_oci; then
        log_fail "Build failed"
        exit 1
    fi

    log_info "Installing to $INSTALL_PATH..."
    sudo cp "$REPO_ROOT/target/release/litebox_runner_oci" "$INSTALL_PATH"
    sudo chmod +x "$INSTALL_PATH"

    log_info "Verifying installation..."
    if "$INSTALL_PATH" --version > /dev/null 2>&1; then
        log_pass "litebox-oci installed successfully"
    else
        log_fail "litebox-oci installation failed"
        exit 1
    fi
}

# Ensure containerd is running
ensure_containerd() {
    log_info "Checking containerd..."
    
    if ! command -v containerd &> /dev/null; then
        log_info "Installing containerd..."
        sudo apt-get update -qq
        sudo apt-get install -y -qq containerd
    fi

    if ! sudo systemctl is-active --quiet containerd; then
        log_info "Starting containerd..."
        sudo systemctl start containerd
        sleep 2
    fi

    if sudo systemctl is-active --quiet containerd; then
        log_pass "containerd is running"
    else
        log_fail "containerd failed to start"
        exit 1
    fi
}

# Pull test image if not present
ensure_test_image() {
    log_info "Ensuring test image is available..."
    
    local images
    images=$(sudo ctr images ls -q 2>/dev/null || echo "")
    
    if echo "$images" | grep -q "$TEST_IMAGE"; then
        log_pass "Test image ready"
    else
        log_info "Pulling $TEST_IMAGE..."
        if sudo ctr images pull "$TEST_IMAGE" > /dev/null 2>&1; then
            log_pass "Test image pulled"
        else
            log_fail "Failed to pull test image"
            exit 1
        fi
    fi
}

# Run a single test
run_test() {
    local test_name="$1"
    local container_id="test-$$-$RANDOM"
    shift
    local cmd=("$@")

    log_info "Running test: $test_name"
    
    local output
    local exit_code=0
    
    output=$(sudo ctr run --rm \
        --runc-binary "$INSTALL_PATH" \
        "$TEST_IMAGE" \
        "$container_id" \
        "${cmd[@]}" 2>&1) || exit_code=$?

    if [ $exit_code -eq 0 ]; then
        log_pass "$test_name"
        echo "    Output: $(echo "$output" | tail -1)"
        return 0
    else
        log_fail "$test_name (exit code: $exit_code)"
        echo "    Output: $output" | head -5
        return 1
    fi
}

# Run all integration tests
run_tests() {
    echo ""
    echo "========================================"
    echo "  LiteBox OCI Integration Tests"
    echo "========================================"
    echo ""

    # Basic tests
    run_test "echo" /bin/echo "Hello from LiteBox!" || true
    run_test "true" /bin/true || true
    run_test "uname" /bin/uname -a || true
    
    # Filesystem tests
    run_test "ls_root" /bin/ls / || true
    run_test "ls_bin" /bin/ls /bin || true
    run_test "cat_etc_passwd" /bin/cat /etc/passwd || true
    
    # Environment tests  
    run_test "pwd" /bin/pwd || true
    # Note: whoami requires /etc/passwd and getuid/getpwuid support, skipped for now
    # run_test "whoami" /bin/whoami || true
    
    # Shell tests
    run_test "sh_echo" /bin/sh -c "echo 'Shell works!'" || true

    echo ""
    echo "========================================"
    echo "  Test Results"
    echo "========================================"
    echo ""
    echo -e "  ${GREEN}Passed:${NC} $pass_count"
    echo -e "  ${RED}Failed:${NC} $fail_count"
    echo ""

    if [ $fail_count -eq 0 ]; then
        echo -e "${GREEN}All tests passed!${NC}"
        return 0
    else
        echo -e "${RED}Some tests failed.${NC}"
        return 1
    fi
}

# Clean up test artifacts
cleanup() {
    log_info "Cleaning up..."
    
    # Kill any lingering test containers
    for container in $(sudo ctr containers ls -q 2>/dev/null | grep "^test-" || true); do
        sudo ctr containers rm "$container" 2>/dev/null || true
    done

    # Clean up state directory
    sudo rm -rf /run/litebox-oci/containers/test-* 2>/dev/null || true
    
    log_pass "Cleanup complete"
}

# Direct CLI tests (without containerd)
run_cli_tests() {
    echo ""
    echo "========================================"
    echo "  LiteBox OCI CLI Tests"
    echo "========================================"
    echo ""

    local test_root="/tmp/litebox-cli-test-$$"
    local bundle_dir="/tmp/litebox-test-bundle-$$"
    
    # Create a simple test bundle
    log_info "Creating test bundle..."
    mkdir -p "$bundle_dir/rootfs/bin" "$bundle_dir/rootfs/lib64" "$bundle_dir/rootfs/lib/x86_64-linux-gnu"
    
    cp /bin/echo "$bundle_dir/rootfs/bin/" 2>/dev/null || true
    
    # Copy echo dependencies
    for lib in $(ldd /bin/echo 2>/dev/null | grep -o '/[^ ]*' || true); do
        local dir=$(dirname "$lib")
        mkdir -p "$bundle_dir/rootfs$dir" 2>/dev/null || true
        cp -L "$lib" "$bundle_dir/rootfs$lib" 2>/dev/null || true
    done

    # Create minimal config.json
    cat > "$bundle_dir/config.json" << 'EOF'
{
  "ociVersion": "1.0.0",
  "process": {
    "args": ["/bin/echo", "CLI test works!"],
    "cwd": "/",
    "user": {"uid": 0, "gid": 0}
  },
  "root": {"path": "rootfs"}
}
EOF

    # Test create
    log_info "Testing 'create' command..."
    if "$INSTALL_PATH" --root "$test_root" create -b "$bundle_dir" --pid-file "$test_root/test.pid" cli-test > /dev/null 2>&1; then
        log_pass "create command"
    else
        log_fail "create command"
    fi

    # Test state
    log_info "Testing 'state' command..."
    local state_output
    state_output=$("$INSTALL_PATH" --root "$test_root" state cli-test 2>/dev/null || echo "")
    if echo "$state_output" | grep -q '"status": "created"'; then
        log_pass "state command"
    else
        log_fail "state command"
    fi

    # Test list
    log_info "Testing 'list' command..."
    local list_output
    list_output=$("$INSTALL_PATH" --root "$test_root" list 2>/dev/null || echo "")
    if echo "$list_output" | grep -q "cli-test"; then
        log_pass "list command"
    else
        log_fail "list command"
    fi

    # Test delete
    log_info "Testing 'delete' command..."
    if "$INSTALL_PATH" --root "$test_root" delete --force cli-test > /dev/null 2>&1; then
        log_pass "delete command"
    else
        log_fail "delete command"
    fi

    # Cleanup
    rm -rf "$test_root" "$bundle_dir"
}

# Main
main() {
    case "${1:-}" in
        --build)
            check_sudo
            build_and_install
            ensure_containerd
            ensure_test_image
            run_cli_tests
            run_tests
            ;;
        --clean)
            check_sudo
            cleanup
            ;;
        --cli-only)
            run_cli_tests
            ;;
        --help)
            echo "Usage: $0 [--build|--clean|--cli-only|--help]"
            echo ""
            echo "Options:"
            echo "  --build     Build and install litebox-oci, then run tests"
            echo "  --clean     Clean up test artifacts"
            echo "  --cli-only  Run only CLI tests (no containerd)"
            echo "  --help      Show this help"
            echo ""
            echo "Without options, runs containerd integration tests only."
            ;;
        *)
            check_sudo
            ensure_containerd
            ensure_test_image
            run_tests
            ;;
    esac
}

main "$@"
