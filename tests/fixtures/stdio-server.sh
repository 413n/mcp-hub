#!/bin/sh
# A minimal MCP server over stdio, for tests: one JSON-RPC message per line.
# Tools: "env" answers with its working directory and $FAKE_SECRET, "crash" exits.
# Not listed: "slow" answers after 2 seconds, answering other requests meanwhile, "hang"
# stops answering anything, "notify" says that the tool list changed.
# With $START_LOG set, appends its pid to that file. While the file $MORE_FLAG exists, there
# is one more tool, "more".
[ -n "$START_LOG" ] && echo $$ >> "$START_LOG"
echo "fake stdio server starting" >&2
while IFS= read -r line; do
    id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
    case "$line" in
    *'"method":"initialize"'*)
        version=$(printf '%s' "$line" | sed -n 's/.*"protocolVersion":"\([^"]*\)".*/\1/p')
        printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"%s","capabilities":{"tools":{}},"serverInfo":{"name":"fake-stdio","version":"1"}}}\n' "$id" "$version"
        ;;
    *'"method":"tools/list"'*)
        more=""
        [ -n "$MORE_FLAG" ] && [ -e "$MORE_FLAG" ] && more=',{"name":"more","inputSchema":{"type":"object"}}'
        printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"env","inputSchema":{"type":"object"}},{"name":"crash","inputSchema":{"type":"object"}}%s]}}\n' "$id" "$more"
        ;;
    *'"method":"tools/call"'*'"name":"crash"'*)
        echo "crashing on request" >&2
        exit 3
        ;;
    *'"method":"tools/call"'*'"name":"notify"'*)
        printf '{"jsonrpc":"2.0","method":"notifications/tools/list_changed"}\n'
        printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"notified"}]}}\n' "$id"
        ;;
    *'"method":"tools/call"'*'"name":"slow"'*)
        (
            sleep 2
            printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"slow done"}]}}\n' "$id"
        ) &
        ;;
    *'"method":"tools/call"'*'"name":"hang"'*)
        sleep 60
        ;;
    *'"method":"ping"'*)
        printf '{"jsonrpc":"2.0","id":%s,"result":{}}\n' "$id"
        ;;
    *'"method":"tools/call"'*)
        printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"cwd=%s secret=%s"}]}}\n' "$id" "$(pwd -P)" "$FAKE_SECRET"
        ;;
    esac
done
