#!/bin/sh
# The backend now ships inside ZLO.app and ZLO starts it, so upgrading
# IronClaw means rebuilding the app. Kept so old habits still work.
exec sh "$(dirname "$0")/ironclaw_gpui/bundle.sh" "$@"
