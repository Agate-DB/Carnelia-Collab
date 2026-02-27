#!/bin/sh
set -e

if [ -n "$NGROK_AUTHTOKEN" ]; then
  ngrok config add-authtoken "$NGROK_AUTHTOKEN"
fi

ngrok start --config /app/ngrok.yml --all &

exec /app/carnelia-collab server --addr 0.0.0.0:4000 --data-dir /app/data --health-addr 0.0.0.0:8080

