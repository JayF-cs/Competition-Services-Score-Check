#!/usr/bin/env bash
exec /usr/bin/google-chrome \
  --headless=new \
  --no-sandbox \
  --remote-debugging-port=9222 \
  --user-data-dir=/tmp/scorer-chrome \
  --disable-background-timer-throttling \
  --disable-renderer-backgrounding \
  --disable-backgrounding-occluded-windows
