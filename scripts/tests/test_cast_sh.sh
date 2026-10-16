#!/usr/bin/env bash
# Integration tests for scripts/cast.sh: runs the real script as a
# subprocess against throwaway fake FCast/HTTP servers (fake-fcast-server.pl,
# fake-http-server.pl), so the actual TCP round trip, the queue-reply skip
# logic, and the upload-image HTTP flow are all exercised for real -- not
# just the pure-framing logic scripts/tests/test_fcast_client.pl covers.
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CAST_SH="$SCRIPT_DIR/../cast.sh"

pass=0
fail=0

assert_eq() {
    local desc="$1" expected="$2" actual="$3"
    if [[ "$expected" == "$actual" ]]; then
        pass=$((pass + 1))
    else
        fail=$((fail + 1))
        echo "FAIL: $desc"
        echo "  expected: $expected"
        echo "  actual:   $actual"
    fi
}

assert_status_nonzero() {
    local desc="$1" status="$2"
    if [[ "$status" -ne 0 ]]; then
        pass=$((pass + 1))
    else
        fail=$((fail + 1))
        echo "FAIL: $desc (expected a nonzero exit status, got 0)"
    fi
}

# Waits for a fake server's --port-file to appear and have content, then
# echoes the port it picked.
read_port() {
    local port_file="$1" tries=50
    while [[ ! -s "$port_file" ]]; do
        tries=$((tries - 1))
        [[ $tries -gt 0 ]] || { echo "timed out waiting for $port_file" >&2; return 1; }
        sleep 0.1
    done
    cat "$port_file"
}

test_ping() {
    local port_file; port_file="$(mktemp -u)"
    perl "$SCRIPT_DIR/fake-fcast-server.pl" --port-file "$port_file" --scenario ping &
    local server_pid=$!
    local port; port="$(read_port "$port_file")"

    local out
    out="$(CASTOFF_PORT="$port" "$CAST_SH" ping)"
    wait "$server_pid" 2>/dev/null
    rm -f "$port_file"

    assert_eq "ping replies pong" "pong" "$out"
}

test_queue_clear_skips_interleaved_playback_update() {
    local port_file; port_file="$(mktemp -u)"
    perl "$SCRIPT_DIR/fake-fcast-server.pl" --port-file "$port_file" --scenario queue-skip &
    local server_pid=$!
    local port; port="$(read_port "$port_file")"

    local out
    out="$(CASTOFF_PORT="$port" "$CAST_SH" queue-clear)"
    wait "$server_pid" 2>/dev/null
    rm -f "$port_file"

    local current_index
    current_index="$(jq '.currentIndex' <<<"$out")"
    assert_eq "queue-clear decodes QueueState, not the interleaved PlaybackUpdate" "null" "$current_index"
}

test_ping_fails_loudly_when_server_closes_without_a_reply() {
    local port_file; port_file="$(mktemp -u)"
    perl "$SCRIPT_DIR/fake-fcast-server.pl" --port-file "$port_file" --scenario closes-without-reply &
    local server_pid=$!
    local port; port="$(read_port "$port_file")"

    CASTOFF_PORT="$port" "$CAST_SH" ping >/dev/null 2>&1
    local status=$?
    wait "$server_pid" 2>/dev/null
    rm -f "$port_file"

    assert_status_nonzero "ping exits nonzero when the connection closes before a reply" "$status"
}

test_upload_image() {
    local port_file; port_file="$(mktemp -u)"
    perl "$SCRIPT_DIR/fake-http-server.pl" --port-file "$port_file" &
    local server_pid=$!
    local port; port="$(read_port "$port_file")"

    local tmp_img; tmp_img="$(mktemp --suffix=.jpg)"
    printf '\xff\xd8\xff\xd9' > "$tmp_img"

    local out
    out="$(CASTOFF_IMAGE_PORT="$port" "$CAST_SH" upload-image "$tmp_img")"
    wait "$server_pid" 2>/dev/null
    rm -f "$port_file" "$tmp_img"

    local id
    id="$(jq -r '.id' <<<"$out")"
    assert_eq "upload-image prints the daemon's returned id" "test-id-1" "$id"
}

assert_contains() {
    local desc="$1" haystack="$2" needle="$3"
    if [[ "$haystack" == *"$needle"* ]]; then
        pass=$((pass + 1))
    else
        fail=$((fail + 1))
        echo "FAIL: $desc"
        echo "  expected to contain: $needle"
        echo "  actual:              $haystack"
    fi
}

test_play_transmits_headers() {
    local port_file; port_file="$(mktemp -u)"
    perl "$SCRIPT_DIR/fake-fcast-server.pl" --port-file "$port_file" --scenario echo-request &
    local server_pid=$!
    local port; port="$(read_port "$port_file")"

    local out
    out="$(CASTOFF_PORT="$port" "$CAST_SH" play 'http://example.com/v.mp4' \
        --header 'Accept=video/mp4' --header 'Referer=http://example.com/')"
    wait "$server_pid" 2>/dev/null
    rm -f "$port_file"

    assert_eq "play sends the URL on the wire" "http://example.com/v.mp4" "$(jq -r '.url' <<<"$out")"
    assert_eq "play folds the first --header into the body's headers" "video/mp4" "$(jq -r '.headers.Accept' <<<"$out")"
    assert_eq "play folds the second --header into the body's headers" "http://example.com/" "$(jq -r '.headers.Referer' <<<"$out")"
}

# Regression: a --header value missing its '=' used to be silently dropped
# by the jq `capture(...)` pipeline (which exits 0 on no match), so the
# command succeeded while sending fewer headers than asked -- with every
# header malformed, `headers` was omitted entirely. It must now fail loudly
# before any connection is made.
test_malformed_header_fails_loudly() {
    local port_file; port_file="$(mktemp -u)"
    perl "$SCRIPT_DIR/fake-fcast-server.pl" --port-file "$port_file" --scenario echo-request &
    local server_pid=$!
    local port; port="$(read_port "$port_file")"

    local out status
    out="$(CASTOFF_PORT="$port" "$CAST_SH" play 'http://example.com/v.mp4' \
        --header 'Accept=video/mp4' --header 'totallybadheader' 2>&1)"
    status=$?
    # The fixed script rejects the bad header before connecting, so the fake
    # server would otherwise sit in accept() forever.
    kill "$server_pid" 2>/dev/null
    wait "$server_pid" 2>/dev/null
    rm -f "$port_file"

    assert_status_nonzero "a malformed --header fails the command" "$status"
    assert_contains "the error explains the KEY=VALUE requirement" "$out" "KEY=VALUE"
}

test_missing_dependency_fails_with_a_clear_message() {
    # Strips every jq-bearing directory out of PATH (leaving perl/curl
    # reachable) to reproduce the captain's real report: `nix shell` (not
    # `nix develop`) has perl/curl as ambient system tools but not this
    # flake's jq. More than one jq can be on PATH at once (the dev shell's
    # plus an ambient system/user one, e.g. on the CI runner), so strip all
    # of them -- stripping only the first `command -v jq` reports would
    # leave a second jq reachable and cast.sh would never see a missing
    # dependency at all.
    local dir trimmed_path=""
    local IFS=:
    for dir in $PATH; do
        [[ -x "$dir/jq" ]] && continue
        trimmed_path+="${trimmed_path:+:}$dir"
    done

    local out status
    out="$(PATH="$trimmed_path" "$CAST_SH" ping 2>&1)"
    status=$?

    assert_status_nonzero "cast.sh exits nonzero when a dependency is missing" "$status"
    assert_contains "the error names the missing tool" "$out" "missing required tool(s): jq"
    assert_contains "the error points at the fix" "$out" "nix develop"
}

test_ping
test_queue_clear_skips_interleaved_playback_update
test_ping_fails_loudly_when_server_closes_without_a_reply
test_upload_image
test_play_transmits_headers
test_malformed_header_fails_loudly
test_missing_dependency_fails_with_a_clear_message

echo "passed: $pass failed: $fail"
[[ $fail -eq 0 ]]
