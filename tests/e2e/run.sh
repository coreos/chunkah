#!/bin/bash
set -euo pipefail
shopt -s inherit_errexit

usage() {
    cat <<EOF
Usage: $(basename "$0") [OPTIONS] [TEST...]

Run end-to-end tests for chunkah.

Options:
    -v, --verbose    Show test output in real-time (default: only on failure)
    -h, --help       Show this help message

Arguments:
    TEST             Test name(s) to run (without 'test-' prefix and '.sh' suffix).
                     If none specified, all test-*.sh files are run.

Environment:
    CHUNKAH_IMG      Container image to test (default: localhost/chunkah:latest)

Examples:
    $(basename "$0")                    # Run all tests
    $(basename "$0") fedora-build       # Run only test-fedora-build.sh
    $(basename "$0") -v fedora-build    # Run with verbose output
EOF
}

CHUNKAH_IMG=${CHUNKAH_IMG:-localhost/chunkah:latest}
export CHUNKAH_IMG

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "${SCRIPT_DIR}"

ONLY_TESTS=()
VERBOSE=false
while [[ $# -gt 0 ]]; do
    case $1 in
        --help|-h)
            usage
            exit 0
            ;;
        --verbose|-v)
            VERBOSE=true
            shift
            ;;
        -*)
            echo "Error: Unknown option: $1" >&2
            echo "Try '$(basename "$0") --help' for more information." >&2
            exit 1
            ;;
        *)
            ONLY_TESTS+=("$1")
            shift
            ;;
    esac
done

# Find and run test scripts
if [[ ${#ONLY_TESTS[@]} -gt 0 ]]; then
    tests=()
    for name in "${ONLY_TESTS[@]}"; do
        testfile="test-${name}.sh"
        if [[ ! -f "${testfile}" ]]; then
            echo "Error: Test not found: ${testfile}" >&2
            exit 1
        fi
        tests+=("${testfile}")
    done
else
    tests=( test-*.sh )
fi

total=${#tests[@]}
passed=0
failed=0
failed_tests=()

echo ""
echo "Running ${total} e2e test(s)..."
echo ""

for test in "${tests[@]}"; do
    echo "=== Running ${test} ==="
    TMPDIR=$(mktemp -d)
    export TMPDIR
    abstest=$(realpath "${test}")

    if [[ ${VERBOSE} == true ]]; then exec 3>&2; else exec 3>"${TMPDIR}/.out"; fi
    if (cd "${TMPDIR}" && bash "${abstest}") >&3 2>&3; then
        echo "=== PASSED: ${test} ==="
        passed=$((passed+1))
    else
        echo "=== FAILED: ${test} ==="
        failed=$((failed+1))
        failed_tests+=("${test}")
        if [[ ${VERBOSE} == false ]]; then
            echo "--- Test output ---"
            cat "${TMPDIR}/.out"
            echo "--- End output ---"
        fi
    fi

    exec 3>&-
    rm -rf "${TMPDIR}"
    unset TMPDIR
    echo ""
done

echo "========================================"
echo "Results: ${passed} passed, ${failed} failed"
echo "========================================"

if [[ "${failed}" -gt 0 ]]; then
    echo "Failed tests:"
    for t in "${failed_tests[@]}"; do
        echo "  - ${t}"
    done
    exit 1
fi
