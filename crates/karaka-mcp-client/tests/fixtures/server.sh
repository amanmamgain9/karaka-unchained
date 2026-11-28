#!/bin/sh
# A tiny 2025-era MCP server over stdio, for the round-trip tests. It needs
# nothing but sh and sed. Each process records its start (with whether the
# previous one was still alive), each request's method, and its exit, in the
# events file it is given.
events="$1"
previous=$(sed -n 's/^start \([0-9]*\).*/\1/p' "$events" | tail -n 1)
alive=false
if [ -n "$previous" ] && kill -0 "$previous" 2>/dev/null; then alive=true; fi
echo "start $$ $alive" >> "$events"
while IFS= read -r line; do
  id=$(printf '%s\n' "$line" | sed -n 's/^{"jsonrpc":"2.0","id":\([^,]*\),.*/\1/p')
  [ -z "$id" ] && continue
  method=$(printf '%s\n' "$line" | sed -n 's/.*"method":"\([^"]*\)".*/\1/p')
  echo "$method $$" >> "$events"
  case "$method" in
    server/discover)
      printf '{"jsonrpc":"2.0","id":%s,"error":{"code":-32601,"message":"Legacy server"}}\n' "$id" ;;
    initialize)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"2025-06-18","capabilities":{"tools":{}},"serverInfo":{"name":"fixture","version":"1"},"instructions":"Use add."}}\n' "$id" ;;
    tools/list)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"add","inputSchema":{"type":"object"}}]}}\n' "$id" ;;
    tools/call)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"3"}]}}\n' "$id" ;;
    *)
      printf '{"jsonrpc":"2.0","id":%s,"error":{"code":-32601,"message":"Method not found"}}\n' "$id" ;;
  esac
done
echo "exit $$" >> "$events"
