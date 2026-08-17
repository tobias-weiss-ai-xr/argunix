#!/usr/bin/env bash
# SPDX-FileCopyrightText: 2026 openDesk Edu Contributors
# SPDX-License-Identifier: Apache-2.0

# Verification script for argunix integration with openDesk Edu

set -euo pipefail

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

# Function to print status
print_status() {
    local status=$1
    local message=$2
    case $status in
        "OK") echo -e "${GREEN}✓${NC} $message" ;;
        "WARN") echo -e "${YELLOW}⚠${NC} $message" ;;
        "ERROR") echo -e "${RED}✗${NC} $message" ;;
        "INFO") echo -e "${BLUE}ℹ${NC} $message" ;;
    esac
}

# Function to check if a file exists
check_file() {
    local file=$1
    local description=$2
    if [ -f "$file" ]; then
        print_status "OK" "$description exists: $file"
        return 0
    else
        print_status "ERROR" "$description missing: $file"
        return 1
    fi
}

# Function to check if a directory exists
check_dir() {
    local dir=$1
    local description=$2
    if [ -d "$dir" ]; then
        print_status "OK" "$description exists: $dir"
        return 0
    else
        print_status "ERROR" "$description missing: $dir"
        return 1
    fi
}

# Function to check if a string exists in a file
check_grep() {
    local pattern=$1
    local file=$2
    local description=$3
    if grep -q "$pattern" "$file" 2>/dev/null; then
        print_status "OK" "$description found in $file"
        return 0
    else
        print_status "ERROR" "$description NOT found in $file"
        return 1
    fi
}

echo ""
echo "======================================"
echo "argunix openDesk Integration Verification"
echo "======================================"
echo ""

ERRORS=0

# Check argunix repository
print_status "INFO" "Checking argunix repository..."
check_file "README.md" "argunix README"
check_file "OPENDESK_INTEGRATION.md" "Integration documentation"
check_dir "scripts" "Scripts directory"
echo ""

# Check opendesk-meta integration
print_status "INFO" "Checking opendesk-meta integration..."
if [ -d "/home/weissto_local/git/opendesk_git/opendesk-meta" ]; then
    METADIR="/home/weissto_local/git/opendesk_git/opendesk-meta"
    check_dir "$METADIR/helmfile/charts/argunix" "argunix Helm chart"
    check_file "$METADIR/helmfile/charts/argunix/Chart.yaml" "Chart.yaml"
    check_file "$METADIR/helmfile/charts/argunix/values.yaml" "values.yaml"
    check_dir "$METADIR/helmfile/charts/argunix/templates" "Chart templates"
    check_file "$METADIR/docs/ci-cd/argunix-integration.md" "Integration docs"
    check_grep "argunix" "$METADIR/README.md" "argunix in component matrix"
    check_grep "argunix" "$METADIR/README.md" "argunix in tech stack"
else
    print_status "WARN" "opendesk-meta not found at /home/weissto_local/git/opendesk_git/opendesk-meta"
fi
echo ""

# Check opendesk-edu integration
print_status "INFO" "Checking opendesk-edu integration..."
if [ -d "/home/weissto_local/git/opendesk_git/opendesk-meta/opendesk-edu" ]; then
    EDUDIR="/home/weissto_local/git/opendesk_git/opendesk-meta/opendesk-edu"
    check_dir "$EDUDIR/helmfile/apps/edu/argunix" "argunix app configuration"
    check_file "$EDUDIR/helmfile/apps/edu/argunix/helmfile.yaml.gotmpl" "helmfile.yaml.gotmpl"
    check_file "$EDUDIR/helmfile/apps/edu/argunix/values.yaml.gotmpl" "values.yaml.gotmpl"
    check_grep "argunix" "$EDUDIR/helmfile/environments/edu/ce-overrides.yaml.gotmpl" "argunix in ce-overrides"
else
    print_status "WARN" "opendesk-edu not found"
fi
echo ""

# Check opendesk-nix integration
print_status "INFO" "Checking opendesk-nix integration..."
if [ -d "/home/weissto_local/git/opendesk_git/opendesk-nix" ]; then
    NIXDIR="/home/weissto_local/git/opendesk_git/opendesk-nix"
    check_file "$NIXDIR/platform/nix/services/argunix.nix" "argunix Nix module"
    check_grep "argunix" "$NIXDIR/platform/nix/nixos/services.nix" "argunix in services catalog"
    check_dir "$NIXDIR/docker/argunix-builder" "argunix-builder Dockerfile"
    check_file "$NIXDIR/docker/argunix-builder/Dockerfile" "Dockerfile"
else
    print_status "WARN" "opendesk-nix not found"
fi
echo ""

# Check Git repository status
print_status "INFO" "Checking Git repository status..."

# Check argunix repository
cd /home/weissto_local/git/argunix
print_status "INFO" "argunix repository:"
print_status "INFO" "  Remote: $(git remote -v | grep github | head -1 | sed 's/^/  /')"
print_status "INFO" "  Branch: $(git branch --show-current)"
print_status "INFO" "  Last commit: $(git log --oneline -1)"
echo ""

# Check opendesk-meta repository
if [ -d "/home/weissto_local/git/opendesk_git/opendesk-meta/.git" ]; then
    cd /home/weissto_local/git/opendesk_git/opendesk-meta
    print_status "INFO" "opendesk-meta repository:"
    print_status "INFO" "  Remote: $(git remote -v | grep github | head -1 | sed 's/^/  /')"
    print_status "INFO" "  Branch: $(git branch --show-current)"
    print_status "INFO" "  Last commit: $(git log --oneline -1)"
echo ""
fi

# Check opendesk-nix repository
if [ -d "/home/weissto_local/git/opendesk_git/opendesk-nix/.git" ]; then
    cd /home/weissto_local/git/opendesk_git/opendesk-nix
    print_status "INFO" "opendesk-nix repository:"
    print_status "INFO" "  Remote: $(git remote -v | grep github | head -1 | sed 's/^/  /')"
    print_status "INFO" "  Branch: $(git branch --show-current)"
    print_status "INFO" "  Last commit: $(git log --oneline -1)"
echo ""
fi

# Summary
echo ""
echo "======================================"
echo "Verification Summary"
echo "======================================"
echo ""

if [ $ERRORS -eq 0 ]; then
    print_status "OK" "All checks passed!"
    print_status "INFO" "argunix is fully integrated with openDesk Edu"
    exit 0
else
    print_status "ERROR" "$ERRORS check(s) failed"
    print_status "INFO" "Please fix the errors above"
    exit 1
fi
