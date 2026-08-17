#!/usr/bin/env bash
set -euo pipefail

# AgentFlow Task Dispatcher Script
# Dispatches all pending agent development tasks to the AgentFlow system

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
TASKS_DIR="$PROJECT_DIR/agentflow/tasks"

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

# Configuration
AGENTFLOW_SERVER="${AGENTFLOW_SERVER:-http://localhost:8080}"
DRY_RUN="${DRY_RUN:-false}"
WAIT_FOR_COMPLETION="${WAIT:-false}"

 Usage() {
    cat <<EOF
Usage: $(basename "$0") [OPTIONS]

Dispatch all AgentFlow agent development tasks to the running system.

Options:
  --server URL       AgentFlow server URL (default: http://localhost:8080)
  --dry-run          Show tasks but don't submit (default: false)
  --wait             Wait for all tasks to complete (default: false)
  --help, -h         Show this help message

Examples:
  # Dry run - show what would be submitted
  $(basename "$0") --dry-run

  # Submit to local server
  $(basename "$0")

  # Submit to specific server and wait
  $(basename "$0") --server http://ci.opendesk.works:8080 --wait

  # Submit all tasks in background
  $(basename "$0") &

EOF
    exit 0
}

# Parse arguments
while [[ $# -gt 0 ]]; do
    case "$1" in
        --server)
            AGENTFLOW_SERVER="$2"
            shift 2
            ;;
        --dry-run)
            DRY_RUN="true"
            shift
            ;;
        --wait)
            WAIT_FOR_COMPLETION="true"
            shift
            ;;
        --help|-h)
            Usage
            ;;
        *)
            echo "Unknown option: $1"
            Usage
            ;;
    esac
done

echo -e "${BLUE}======================================================="
echo "  AgentFlow Task Dispatcher"
echo -e "=======================================================${NC}"
echo

# Check if we're in the right directory
if [ ! -d "$TASKS_DIR" ]; then
    echo -e "${RED}Error: Tasks directory not found at $TASKS_DIR${NC}"
    echo "Please run this script from the project root or set PROJECT_DIR."
    exit 1
fi

# List all task files
echo -e "${YELLOW}Scanning for task files...${NC}"
TASK_FILES=($TASKS_DIR/*.yaml)

if [ ${#TASK_FILES[@]} -eq 0 ]; then
    echo -e "${RED}Error: No task files found in $TASKS_DIR${NC}"
    exit 1
fi

echo -e "${GREEN}Found ${#TASK_FILES[@]} task file(s):${NC}"
echo

# Display each task
SUBMITTED=0
FAILED=0
SKIPPED=0

for task_file in "${TASK_FILES[@]}"; do
    # Extract task metadata
    TASK_ID=$(grep '^id:' "$task_file" | head -1 | cut -d' ' -f2 | tr -d "\"'" ) || true
    TASK_TITLE=$(grep '^title:' "$task_file" | head -1 | cut -d' ' -f2- | tr -d "\"'" ) || true
    TASK_EFFORT=$(grep '^estimated_duration:' "$task_file" | head -1 | cut -d' ' -f2 | tr -d "\"'" ) || true
    TASK_PRIORITY=$(grep '^priority:' "$task_file" | head -1 | cut -d' ' -f2 | tr -d "\"'" ) || true
    
    # Convert effort to hours
    if [[ "$TASK_EFFORT" =~ ^[0-9]+$ ]]; then
        HOURS=$((TASK_EFFORT / 3600))
        MINUTES=$(( (TASK_EFFORT % 3600) / 60 ))
        DURATION="${HOURS}h ${MINUTES}m"
    else
        DURATION="$TASK_EFFORT"
    fi
    
    # Display task info
    echo -e "  ${BLUE}📝 $(basename "$task_file")${NC}"
    echo "     ID:       $TASK_ID"
    echo "     Title:    $TASK_TITLE"
    echo "     Priority: $TASK_PRIORITY"
    echo "     Effort:   $DURATION"
    echo
    
    if [ "$DRY_RUN" = "true" ]; then
        echo -e "     ${YELLOW}[DRY RUN] Would submit this task${NC}"
        SKIPPED=$((SKIPPED + 1))
    else
        # Check if AgentFlow server is running
        if ! ping_server; then
            echo -e "     ${RED}[ERROR] AgentFlow server not responding at $AGENTFLOW_SERVER${NC}"
            FAILED=$((FAILED + 1))
            continue
        fi
        
        # Submit the task
        if submit_task "$task_file"; then
            SUBMITTED=$((SUBMITTED + 1))
            echo -e "     ${GREEN}✓ Submitted successfully${NC}"
        else
            FAILED=$((FAILED + 1))
            echo -e "     ${RED}✗ Failed to submit${NC}"
        fi
    fi
    echo
done

# Summary
echo -e "${BLUE}======================================================="
echo "  Summary"
echo -e "=======================================================${NC}"
echo -e "  Tasks found:    ${#TASK_FILES[@]}"
echo -e "  ${GREEN}Submitted:      $SUBMITTED${NC}"
echo -e "  ${RED}Failed:          $FAILED${NC}"
echo -e "  ${YELLOW}Skipped (dry-run): $SKIPPED${NC}"
echo

# Wait for completion if requested
if [ "$WAIT_FOR_COMPLETION" = "true" ] && [ "$DRY_RUN" = "false" ] && [ $SUBMITTED -gt 0 ]; then
    echo -e "${YELLOW}Waiting for tasks to complete...${NC}"
    wait_for_completion
fi

echo -e "${GREEN}Task dispatch complete!${NC}"
exit 0

# Functions

ping_server() {
    # Try to ping the server
    timeout 5 curl -s -o /dev/null "$AGENTFLOW_SERVER/health" > /dev/null 2>&1 || \
    timeout 5 curl -s -o /dev/null "$AGENTFLOW_SERVER/api/health" > /dev/null 2>&1
}

submit_task() {
    local task_file="$1"
    local task_yaml
    
    # Read task file
    task_yaml=$(cat "$task_file") || return 1
    
    # Submit via HTTP API
    response=$(curl -s -w "\n%{http_code}" -X POST \
        -H "Content-Type: application/yaml" \
        -d "$task_yaml" \
        "$AGENTFLOW_SERVER/api/tasks" 2>&1) || true
    
    http_code=$(echo "$response" | tail -n1)
    body=$(echo "$response" | sed '$d')
    
    if [ "$http_code" -ge 200 ] && [ "$http_code" -lt 300 ]; then
        echo "$body" | head -5
        return 0
    else
        echo "$body" | head -10
        return 1
    fi
}

wait_for_completion() {
    # Poll for task completion
    local all_complete=false
    local timeout=3600  # 1 hour timeout
    local start_time=$(date +%s)
    
    while ! $all_complete; do
        sleep 10
        
        local elapsed=$(( $(date +%s) - start_time ))
        if [ $elapsed -gt $timeout ]; then
            echo -e "${RED}Timeout waiting for tasks${NC}"
            break
        fi
        
        # Check status of all submitted tasks
        # This is a placeholder - would need actual task IDs
        # all_complete=true
        
        echo -n "."
    done
    
    echo
}
