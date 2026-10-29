#!/bin/bash
# Curl reachability test: 2 soft attempts + 1 hard attempt
# Usage: curl_test.sh <url>
# Exit 0 = reachable, Exit 1 = unreachable

URL="${1:-https://turkmenistanairlines.tm/}"

# Soft attempt 1 (ignore result)
curl -s --max-time 5 -o /dev/null "$URL" 2>/dev/null || true
# Soft attempt 2 (ignore result)
curl -s --max-time 5 -o /dev/null "$URL" 2>/dev/null || true
# Hard attempt (determines result)
HTTP_CODE=$(curl -s --max-time 5 -o /dev/null -w '%{http_code}' "$URL" 2>/dev/null)
if [ "$HTTP_CODE" -ge 200 ] && [ "$HTTP_CODE" -lt 400 ] 2>/dev/null; then
    echo "PASS:$HTTP_CODE"
    exit 0
else
    echo "FAIL:$HTTP_CODE"
    exit 1
fi
