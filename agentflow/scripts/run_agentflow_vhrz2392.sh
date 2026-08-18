#!/bin/bash
set -e

# AgentFlow System Startup Script for vhrz2392
# Usage: ./run_agentflow_vhrz2392.sh [start|stop|restart|status]

REMOTE="ansible@vhrz2392.hrz.uni-marburg.de"
SSH_OPTS="-i ~/.ssh/id_ed25519_ssh -o KexAlgorithms=curve25519-sha256"
REMOTE_DIR="/home/ansible/git/argunix/agentflow"
REMOTE_CARGO="/home/ansible/.cargo/bin"

# Server configuration
SERVER_BIND="0.0.0.0:3000"
SERVER_LOG_LEVEL="info"

# Agent ports
NATS_PORT=4222
HTTP_PORT=3000
LOG_LEVEL="debug"

start_server() {
    echo "🚀 Starting AgentFlow server on vhrz2392..."
    
    # Set environment variables and run server
    ssh $SSH_OPTS $REMOTE "cd $REMOTE_DIR && \
        RUST_LOG=$SERVER_LOG_LEVEL \
        AGENTFLOW_BIND_ADDRESS=$SERVER_BIND \
        $REMOTE_CARGO/agentflow-server 2>&1 & \
        echo \$! > $REMOTE_DIR/server.pid && \
        echo 'Server started with PID: ' \$(cat $REMOTE_DIR/server.pid)"
    
    echo "✅ Server should be running at http://vhrz2392:3000"
}

start_dispatch_example() {
    echo "🚀 Running notification dispatch example on vhrz2392..."
    ssh $SSH_OPTS $REMOTE "cd $REMOTE_DIR && RUST_LOG=debug $REMOTE_CARGO/dispatch_notification 2>&1"
    echo "✅ Dispatch example completed"
}

stop_server() {
    echo "🛑 Stopping AgentFlow server on vhrz2392..."
    ssh $SSH_OPTS $REMOTE "kill \$(cat $REMOTE_DIR/server.pid 2>/dev/null) 2>/dev/null; rm -f $REMOTE_DIR/server.pid; echo 'Server stopped'"
    echo "✅ Server stopped"
}

status() {
    echo "📊 Checking AgentFlow system status on vhrz2392..."
    ssh $SSH_OPTS $REMOTE "ps aux | grep agentflow | grep -v grep || echo 'No agentflow processes running'"
    ssh $SSH_OPTS $REMOTE "cat $REMOTE_DIR/server.pid 2>/dev/null && echo ': Server PID' || echo 'No server.pid file'"
    ssh $SSH_OPTS $REMOTE "netstat -tlnp | grep 3000 || echo 'Port 3000 not in use'"
}

case "$1" in
    start)
        start_server
        ;;
    dispatch)
        start_dispatch_example
        ;;
    stop)
        stop_server
        ;;
    restart)
        stop_server
        sleep 2
        start_server
        ;;
    status)
        status
        ;;
    *)
        echo "Usage: $0 [start|stop|restart|status|dispatch]"
        echo ""
        echo "Commands:"
        echo "  start     - Start AgentFlow server on vhrz2392"
        echo "  dispatch  - Run notification dispatch example"
        echo "  stop      - Stop AgentFlow server"
        echo "  restart   - Restart AgentFlow server"
        echo "  status    - Check system status"
        exit 1
        ;;
esac
