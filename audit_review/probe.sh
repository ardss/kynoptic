#!/bin/bash
DB="$1"; EP="$2"; PORT=${3:-18901}
./kynoptic_copy.exe dashboard --db "$DB" --port $PORT >/dev/null 2>&1 &
sleep 2
curl -s "http://127.0.0.1:$PORT$EP"
echo
taskkill //IM kynoptic_copy.exe //F >/dev/null 2>&1
