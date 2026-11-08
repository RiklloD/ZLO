#!/bin/sh
# Send a message to a thread through the local gateway, so a ZLO window
# that has the thread open streams the answer without anyone typing.
#   sh tools/ironclaw_rune/energy_send.sh <thread_id> "<prompt>"
set -eu
home=${IRONCLAW_REBORN_HOME:-$HOME/.ironclaw/reborn}
token=$(tr -d '\n' < "$home/webui-token")
base=http://127.0.0.1:3000/api/webchat/v2
body=$(python3 -c 'import json,sys,uuid; print(json.dumps({"client_action_id":str(uuid.uuid4()),"thread_id":sys.argv[1],"content":sys.argv[2],"attachments":[],"mention_roots":[]}))' "$1" "$2")
curl -sf -H "Authorization: Bearer $token" -H 'Content-Type: application/json' -d "$body" "$base/channels/web-app/messages" >/dev/null
