#!/usr/bin/env bash

set -euo pipefail

if (($# != 2)); then
  echo "usage: $0 <base-commit> <head-commit>" >&2
  exit 2
fi

base=$1
head=$2

git cat-file -e "${base}^{commit}"
git cat-file -e "${head}^{commit}"

failed=0
conventional_subject_re='^(fix|feat|feature)(\([^)]*\))?!?:[[:space:]]'
subject_re='^[a-z0-9][a-z0-9_./+-]*:[[:space:]][A-Z]'

report_error() {
  local commit=$1
  local message=$2

  printf '::error title=Invalid commit message::Commit %.12s: %s\n' \
    "$commit" "$message"
  failed=1
}

while IFS= read -r commit; do
  message=$(git show --no-patch --format=%B "$commit")
  subject=${message%%$'\n'*}

  if [[ ${subject,,} =~ $conventional_subject_re ]]; then
    report_error "$commit" \
      "use 'subsystem: Description' instead of a conventional-commit subject"
  elif [[ ! $subject =~ $subject_re ]]; then
    report_error "$commit" \
      "subject must use 'subsystem: Description'"
  fi

  mapfile -t trailers < <(
    printf '%s\n' "$message" | git interpret-trailers --parse
  )

  line_number=0
  while IFS= read -r line || [[ -n $line ]]; do
    ((line_number += 1))
    ((${#line} <= 72)) && continue

    is_trailer=0
    normalized_line=$line
    if [[ $line =~ ^([^:]+:)[[:space:]]+(.*)$ ]]; then
      trailer_token=${BASH_REMATCH[1]}
      trailer_value=${BASH_REMATCH[2]}
      while [[ $trailer_value == *[[:space:]] ]]; do
        trailer_value=${trailer_value%?}
      done
      normalized_line="$trailer_token $trailer_value"
    fi
    for trailer in "${trailers[@]}"; do
      if [[ $normalized_line == "$trailer" ]]; then
        is_trailer=1
        break
      fi
    done
    ((is_trailer)) && continue

    report_error "$commit" \
      "line $line_number is ${#line} columns; wrap prose at 72"
  done <<<"$message"
done < <(git rev-list --reverse --no-merges "${base}..${head}")

exit "$failed"
