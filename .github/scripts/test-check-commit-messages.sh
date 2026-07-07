#!/usr/bin/env bash

set -euo pipefail

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
checker="$script_dir/check-commit-messages.sh"

repo=$(mktemp -d)
trap 'rm -rf "$repo"' EXIT

git -C "$repo" init --quiet --initial-branch=main
git -C "$repo" config user.name "Commit Message Test"
git -C "$repo" config user.email "commit-message-test@example.com"

commit_message() {
  git -C "$repo" reset --hard --quiet "$base"
  printf '%s\n' "$1" | git -C "$repo" commit --allow-empty --quiet --file=-
  git -C "$repo" rev-parse HEAD
}

expect_pass() {
  local name=$1
  local head=$2
  local output

  if ! output=$(cd "$repo" && "$checker" "$base" "$head" 2>&1); then
    printf '%s\n' "$output" >&2
    echo "FAIL: $name should pass" >&2
    exit 1
  fi
}

expect_fail() {
  local name=$1
  local head=$2
  local expected=$3
  local output

  if output=$(cd "$repo" && "$checker" "$base" "$head" 2>&1); then
    echo "FAIL: $name should fail" >&2
    exit 1
  fi
  if [[ $output != *"$expected"* ]]; then
    printf '%s\n' "$output" >&2
    echo "FAIL: $name failed for the wrong reason" >&2
    exit 1
  fi
}

printf '%s\n' \
  'ci: Seed test history' \
  '' \
  'Create a base commit outside every checked range.' \
  '' \
  'Signed-off-by: Commit Message Test <commit-message-test@example.com>' |
  git -C "$repo" commit --allow-empty --quiet --file=-
base=$(git -C "$repo" rev-parse HEAD)

expect_pass "empty range" "$base"

line_72=$(printf 'x%.0s' {1..72})
valid_message=$'ci: Validate commit message style\n\n'"$line_72"$'\n\nSigned-off-by: Contributor With An Exceptionally Long Name <long-address@example.com>'
head=$(commit_message "$valid_message")
expect_pass "valid message and long trailer" "$head"

message=$'ci: Accept normalized Git trailers\n\nKeep alternate trailer separators intact.\n\nSigned-off-by:\tContributor With An Exceptionally Long Name <long-address@example.com>'
head=$(commit_message "$message")
expect_pass "long trailer with a tab separator" "$head"

message=$'fix(ci): validate commit messages\n\nUse a conventional-commit subject.\n\nSigned-off-by: Test <test@example.com>'
head=$(commit_message "$message")
expect_fail "conventional-commit subject" "$head" \
  "instead of a conventional-commit subject"

message=$'Validate commit messages\n\nOmit the subsystem prefix.\n\nSigned-off-by: Test <test@example.com>'
head=$(commit_message "$message")
expect_fail "missing subsystem" "$head" \
  "subject must use 'subsystem: Description'"

message=$'ci: validate commit messages\n\nStart the description with a lowercase letter.\n\nSigned-off-by: Test <test@example.com>'
head=$(commit_message "$message")
expect_fail "lowercase description" "$head" \
  "subject must use 'subsystem: Description'"

long_body=$(printf 'x%.0s' {1..73})
message=$'ci: Reject long prose lines\n\n'"$long_body"$'\n\nSigned-off-by: Test <test@example.com>'
head=$(commit_message "$message")
expect_fail "overlong prose" "$head" "line 3 is 73 columns"

long_subject="ci: A$(printf 'x%.0s' {1..68})"
head=$(commit_message "$long_subject")
expect_fail "overlong subject" "$head" "line 1 is 73 columns"

git -C "$repo" reset --hard --quiet "$base"
git -C "$repo" switch --quiet --create topic
printf '%s\n' \
  'ci: Add valid topic commit' \
  '' \
  'Create a valid non-merge commit.' |
  git -C "$repo" commit --allow-empty --quiet --file=-
git -C "$repo" switch --quiet main
git -C "$repo" merge --quiet --no-ff topic --message 'Merge topic branch'
head=$(git -C "$repo" rev-parse HEAD)
expect_pass "merge commit subject" "$head"

echo "All commit-message checks passed."
