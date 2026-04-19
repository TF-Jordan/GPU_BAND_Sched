if [ -r /etc/cricket/client.conf ]; then
    . /etc/cricket/client.conf

    if [ "${AUTO_PRELOAD:-false}" = "true" ] && [ -r /usr/local/lib/cricket/cricket-client.so ]; then
        export REMOTE_GPU_ADDRESS="${REMOTE_GPU_ADDRESS}:${REMOTE_GPU_PORT}"
        export LD_LIBRARY_PATH="/usr/local/lib/cricket:${LD_LIBRARY_PATH}"
        export LD_PRELOAD="/usr/local/lib/cricket/cricket-client.so${LD_PRELOAD:+:$LD_PRELOAD}"
    fi
fi
