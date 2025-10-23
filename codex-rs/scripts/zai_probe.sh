#!/usr/bin/env bash
set -euo pipefail

BASE_URL="${1:-https://api.z.ai/api/coding/paas/v4}"
MODEL_SLUG="${2:-glm-4.5-air}"
API_KEY="${ZAI_API_KEY:-}"

if [[ -z "${API_KEY}" ]]; then
  echo "error: set ZAI_API_KEY in the environment (or export it inline) before running this probe." >&2
  exit 1
fi

# Trim any trailing slash so we can safely append paths.
BASE_URL="${BASE_URL%/}"

probe_request() {
  local method="$1"
  local path="$2"
  local body="${3:-}"
  local url="${BASE_URL}${path}"
  local tmp_body
  tmp_body="$(mktemp)"

  if [[ -z "${body}" ]]; then
    status="$(curl -sS -o "${tmp_body}" -w "%{http_code}" \
      -X "${method}" "${url}" \
      -H "Authorization: Bearer ${API_KEY}" \
      -H "Accept: application/json")"
  else
    status="$(curl -sS -o "${tmp_body}" -w "%{http_code}" \
      -X "${method}" "${url}" \
      -H "Authorization: Bearer ${API_KEY}" \
      -H "Content-Type: application/json" \
      -H "Accept: application/json" \
      --data "${body}")"
  fi

  echo "=== ${method} ${url}"
  echo "HTTP ${status}"
  cat "${tmp_body}"
  echo
  rm -f "${tmp_body}"
}

probe_request GET "/models"

chat_payload="$(cat <<EOF
{
  "model": "${MODEL_SLUG}",
  "messages": [
    { "role": "user", "content": "Just reply with pong." }
  ],
  "max_tokens": 64
}
EOF
)"

probe_request POST "/chat/completions" "${chat_payload}"
